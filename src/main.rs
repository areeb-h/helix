//! Helix — a modern scientific programming language.
//!
//! Phase 1: a tree-walking interpreter for the core language.
//!
//! Usage:
//!     helix path/to/script.helix      run a file
//!     helix                           start the REPL

mod align;
mod ast;
mod autodiff;
mod backend;
mod bed;
mod bio;
mod bundle;
mod bytecode;
mod capability;
mod chart;
mod dataframe;
mod doctest;
mod error;
mod fmt;
mod gff;
mod hbc;
mod http;
mod interp;
mod jit;
mod json;
mod lattice;
mod lexer;
mod managed;
mod module;
mod namespace;
mod net;
mod parser;
mod pkg;
mod python;
mod registry;
mod render;
mod rng;
mod sam;
mod serve;
mod simd;
mod stats;
mod strfmt;
mod suggest;
mod symbol;
mod tensor;
mod token;
mod types;
mod value;
mod vcf;
mod vm;
mod writers;

// Process-wide allocator. mimalloc replaces the system allocator (glibc/musl/system
// malloc) for every Rust allocation — a pure runtime win on Helix's allocation-heavy
// paths (Polars/Arrow buffers, AST nodes, `Value` clones, parsing) and the documented
// fix for musl's slow default allocator, so the static musl build stays glibc-fast
// (ADR 0009 §8, ADR 0016). It is `GlobalAlloc`-only — it does NOT interpose libc
// `malloc` or CPython's allocators, so it composes safely with the embedded-CPython
// `python` feature. Gated on the default-on `mimalloc` feature so an allocator-
// debugging build (`--no-default-features`) falls back to the system allocator.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::{self, Write};
use std::process::ExitCode;

use error::HelixError;
use interp::Interp;
use value::Value;

fn main() -> ExitCode {
    install_robustness_hooks();
    configure_thread_pool();
    // Return freed memory to the OS promptly (mimalloc `purge_delay = 0`) instead of
    // its default ~10 ms hold. Helix processes are typically short-lived (CLI,
    // serverless), so they exit before that delay ever fires — leaving freed pages
    // resident and inflating peak RSS by tens of MB. Immediate purging keeps the
    // allocator's wall-time win while cutting peak RSS to ~system-allocator levels on
    // the data workloads (measured: VCF read 1.48x->1.08x, group-by 1.77x->1.46x).
    // `15` is `mi_option_purge_delay` in mimalloc v3 (the version the crate builds);
    // the enum's `deprecated_*` placeholders keep that index stable across v3 releases.
    // …but PURGE BY RESET, NOT BY DECOMMIT. `purge_delay = 0` above says "return pages
    // immediately"; `purge_decommits = 1` (the default) says "and unmap them", so every freed
    // buffer over mimalloc's large-object threshold costs a full page-fault storm the next
    // time that memory is touched. Together they made ordinary allocation-heavy code 2.7x
    // slower and produced an undocumented ~10x cliff at exactly 65,536 i64 elements — 512 KiB,
    // the threshold — which nothing in the language explains and which a library author would
    // read as their own bug:
    //
    // Two binaries differing only by this option, runs INTERLEAVED, min of 9, on a box at load
    // average 0.48 — because a first attempt at these numbers was taken at load 9.3 and read
    // 0.57 s and 0.18 s for the same case in two runs:
    //
    //                             decommit      reset            peak RSS
    //     append 80k               4.710 s      0.470 s   10.0x   20.2 -> 24.6 MB
    //     append 65k (below cliff) 0.230 s      0.240 s    0.96x  (the cliff is this option)
    //     map-chains 200k x2000    3.030 s      1.040 s    2.9x   32.2 -> 34.9 MB
    //     200 sorts of a 100k      0.250 s      0.170 s    1.47x  31.0 -> 29.6 MB
    //     large array 20M          0.030 s      0.020 s          190.8 -> 191.0 MB
    //
    // Reset keeps the RSS win this pair was added for, because the pages are still returned —
    // they are just madvised rather than unmapped. THE LAST ROW IS THE ONE THAT MATTERS: the
    // large-array data workloads are why `purge_delay = 0` is here, and their peak RSS is
    // unchanged. The cost is ~4 MB on small programs, which is not a trade, it is a rounding
    // error.
    #[cfg(feature = "mimalloc")]
    unsafe {
        libmimalloc_sys::mi_option_set(15, 0);
        libmimalloc_sys::mi_option_set(5, 0);
    }
    // The bytecode VM — the default engine — recurses on the *heap* (frames in a
    // `Vec`), so it runs on the ordinary main-thread stack. Only the tree-walker
    // recurses on the native stack, and it is now reached just as a rare
    // compile-fallback or under `HELIX_NOVM` / the REPL; those paths spawn a
    // big-stack thread on demand (see `run_on_big_stack`). So the process no longer
    // reserves 2 GiB up front for every invocation.
    run()
}

/// `HELIX_THREADS` — cap the worker threads Helix may use, or `1` to run fully serial.
///
/// Helix parallelizes array work past [`crate::interp::PAR_MATH_THRESHOLD`], which is a
/// WALL-CLOCK-FOR-CPU TRADE and not always the one a caller wants. Measured on the k1 dot
/// product (50M elements): the default uses ~2.8 cores to finish in 0.18 s, while one thread
/// takes 0.26 s — so parallelism buys 1.44× wall for ~2× the CPU time. On a laptop, a shared
/// box, or a machine running several jobs, that is the wrong trade, and until now there was no
/// documented way to decline it: the pool's size could only be changed through rayon's own
/// `RAYON_NUM_THREADS`, which is an implementation detail a Helix user has no reason to know.
///
/// Results do NOT depend on this value. Parallel `map`/`filter` are elementwise, so chunking
/// cannot reorder anything; float reductions are never reassociated (that would change the last
/// bits and break the three-engine oracle); and the parallel nested reduce partitions over
/// independent outer indices and collects in order. `HELIX_THREADS=1` is therefore a pure
/// CPU/latency control, which the test
/// `cli::thread_count_changes_cpu_not_results` pins.
///
/// Invalid or absent values leave rayon's default (one worker per core). Set before any parallel
/// work runs, and failure to install is ignored — a pool already built is not an error worth
/// aborting a program over.
fn configure_thread_pool() {
    let Ok(raw) = std::env::var("HELIX_THREADS") else { return };
    let Ok(n) = raw.trim().parse::<usize>() else { return };
    if n == 0 {
        return; // 0 would mean "rayon picks"; say nothing rather than surprise the caller
    }
    let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
}

/// Process-wide robustness: never crash on a broken pipe, and present any unexpected
/// internal panic as a clean message instead of a raw Rust backtrace.
fn install_robustness_hooks() {
    // Rust ignores SIGPIPE, so writing to a closed stdout (`helix … | head`) returns
    // EPIPE and the stdlib panics. Restoring the default action terminates the process
    // cleanly via the signal instead — the normal behaviour of any Unix tool.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // A panic is always a bug — no user input should reach one. But if one ever slips
    // through, print a concise, friendly line rather than a backtrace. (The build uses
    // panic=abort, so this runs just before the abort: a clean message and a defined
    // non-zero exit, not recovery.)
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!(" ({}:{})", l.file(), l.line()))
            .unwrap_or_default();
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unexpected internal error".to_string());
        eprintln!("error: internal error{loc}: {msg}");
        eprintln!("help: this is a bug in Helix; please report it with the program that triggered it.");
    }));
}

fn run() -> ExitCode {
    // Install the capability authority before anything runs (ADR 0021). Phase 1 defaults to
    // `Off` (no checks) unless `HELIX_CAP=audit|enforce` is set, so this is a no-op for every
    // existing program; a bundled exe will later carry its baked grant here instead of env.
    capability::install_from_env();
    // A standalone executable built with `helix build` carries its program appended to
    // this binary. If we are such an artifact, run the embedded program and ignore the
    // command line entirely (the args belong to the user's program, not to `helix`).
    // A plain `helix` binary has no overlay, so this returns `None` and the CLI runs.
    if let Some((source, filename)) = bundle::embedded() {
        return run_on_big_stack(move || run_source(&source, &filename));
    }
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        // No args (or `repl`) → interactive session. The REPL drives the
        // tree-walker line by line, so give it the big stack.
        None | Some("repl") => run_on_big_stack(repl),
        Some("--version") | Some("-V") | Some("version") => {
            println!("helix {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help") | Some("-h") | Some("help") => {
            print_help();
            ExitCode::SUCCESS
        }
        // `helix run <script>` — the explicit form.
        Some("run") => match args.get(2) {
            Some(path) => run_file(path),
            None => {
                eprintln!("error: `helix run` needs a script path, e.g. `helix run main.helix`");
                ExitCode::FAILURE
            }
        },
        // `helix eval "<code>"` — run a one-liner.
        Some("eval") | Some("-e") => match args.get(2) {
            Some(code) => run_eval(code),
            None => {
                eprintln!("error: `helix eval` needs code, e.g. `helix eval \"print(1 + 2)\"`");
                ExitCode::FAILURE
            }
        },
        // `helix check <script>…` — load + type-check, produce nothing, run nothing.
        Some("check") => run_check(&args),
        // `helix fmt <script>… [--check]` — normalize whitespace; never change a token.
        Some("fmt") => run_fmt(&args),
        // `helix build <script> [-o name]` — bundle a program into a standalone exe.
        Some("build") => run_build(&args),
        // `helix emit-hbc <script> [--entry NAME] [-o out.hbc]` — compile to a `.hbc`
        // Helix Bytecode Container for ctype's ring-0 `hvm` (ADR 0023).
        Some("emit-hbc") => run_emit_hbc(&args),
        // `helix python <…>` — manage CPython runtimes for interop.
        Some("python") => run_python_cli(&args),
        // `helix new <name>` — initialize a `helix.toml`.
        Some("new") => match args.get(2) {
            Some(name) => pkg_result(pkg::cli_new(name)),
            None => {
                eprintln!("error: `helix new` needs a package name, e.g. `helix new mylib`");
                ExitCode::FAILURE
            }
        },
        // `helix add <name> --path <dir> | --url <tarball> [--sha256 <hash>]`.
        Some("add") => pkg_result(parse_add(&args)),
        // `helix sync` — resolve dependencies and (re)write the hash-pinned `helix.lock`.
        Some("sync") => pkg_result(pkg::cli_sync()),
        // `helix verify` — check the project matches helix.lock (CI gate; no build/run).
        Some("verify") => pkg_result(pkg::cli_verify()),
        // `helix test [path]` — run `*_test.helix` files and report pass/fail.
        Some("test") => cli_test(&args),
        // `helix doc [Type]` — list the methods on a type (API discovery).
        Some("doc") => cli_doc(&args),
        // `helix describe` — the whole API as JSON (machine-readable, for LLMs/agents/tools).
        Some("describe") => cli_describe(),
        // Shorthand: `helix script.helix` runs a file directly.
        Some(path) => run_file(path),
    }
}

/// `helix doc [Type|builtins]` — list a type's methods (or the free functions) so the
/// API is discoverable without triggering an unknown-method error. Names come straight
/// from the registry (the same source the checker and "did you mean?" hints use).
fn cli_doc(args: &[String]) -> ExitCode {
    use crate::registry;
    let tables = registry::type_method_tables();
    let sorted = |names: &[&'static str]| {
        let mut v: Vec<&str> = names.to_vec();
        v.sort_unstable();
        v.join(", ")
    };
    match args.get(2).map(|s| s.as_str()) {
        // Overview: every receiver type + its method count, plus how to drill in.
        None => {
            println!("Helix methods by receiver type — `helix doc <Type>` lists one type:\n");
            for (ty, methods) in tables {
                println!("  {:<10} {} methods", ty, methods.len());
            }
            println!("\n  {:<10} {}", "(universal)", registry::UNIVERSAL_METHODS.join(", "));
            println!("\nFree functions: `helix doc builtins`");
            ExitCode::SUCCESS
        }
        // The free-function builtins (sqrt, read_csv, …).
        Some("builtins") | Some("functions") => {
            let names: Vec<&'static str> = registry::BUILTINS.iter().map(|b| b.path).collect();
            println!("Free functions ({}):\n  {}", names.len(), sorted(&names));
            ExitCode::SUCCESS
        }
        // A specific type (case-insensitive: `helix doc dna`).
        Some(query) => {
            let q = query.to_ascii_lowercase();
            match tables.iter().find(|(ty, _)| ty.to_ascii_lowercase() == q) {
                Some((ty, methods)) => {
                    println!("{} methods ({}):\n  {}", ty, methods.len(), sorted(methods));
                    println!("\nUniversal (any value): {}", registry::UNIVERSAL_METHODS.join(", "));
                    ExitCode::SUCCESS
                }
                None => {
                    let known: Vec<&str> = tables.iter().map(|(t, _)| *t).collect();
                    eprintln!(
                        "error: unknown type `{}`. Try one of: {} (or `builtins`).",
                        query,
                        known.join(", ")
                    );
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// `helix describe` — emit the entire public API (free functions + per-type methods) as
/// JSON on stdout, each tagged with its capability effect. This is the machine-readable
/// twin of `helix doc`: a catalog an LLM/agent/tool grounds on to generate correct Helix
/// (real names, which methods live on which receiver, what is capability-gated) instead of
/// hallucinating. Sourced from the registry — the same single source of truth the checker,
/// runtime, and `did you mean?` hints use, so it can never drift from the language.
fn cli_describe() -> ExitCode {
    use crate::{capability, registry};
    let builtins: Vec<serde_json::Value> = registry::BUILTINS
        .iter()
        .map(|b| {
            serde_json::json!({
                "name": b.path,
                "pure": b.pure,
                "effect": capability::effect_of(b.path).label(),
                "category": registry::category_of(b.path),
            })
        })
        .collect();
    let mut methods = serde_json::Map::new();
    for (ty, ms) in registry::type_method_tables() {
        let arr: Vec<serde_json::Value> = ms
            .iter()
            .map(|&m| serde_json::json!({ "name": m, "effect": capability::method_effect_of(m).label() }))
            .collect();
        methods.insert(ty.to_string(), serde_json::Value::Array(arr));
    }
    let doc = serde_json::json!({
        "helix_version": env!("CARGO_PKG_VERSION"),
        "builtins": builtins,
        "methods": methods,
        "universal_methods": registry::UNIVERSAL_METHODS,
        // Effect categories a consumer may see; the gated ones require a capability grant.
        "effects": ["pure", "fs-read", "fs-write", "net", "process", "env"],
    });
    match serde_json::to_string_pretty(&doc) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: could not serialize the API catalog: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `helix python <subcommand>` — manage managed CPython runtimes (behind the
/// `managed` feature; a build without it explains how to get it).
fn run_python_cli(args: &[String]) -> ExitCode {
    #[cfg(feature = "managed")]
    {
        match managed::cli(&args[2..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(feature = "managed"))]
    {
        let _ = args;
        eprintln!("error: this build has no managed-runtime support");
        eprintln!("help: rebuild with `cargo build --features managed`.");
        ExitCode::FAILURE
    }
}

/// Run a one-liner passed on the command line (`helix eval "..."`). Single source
/// (no imports); errors render against a `<eval>` filename.
/// Turn a package-manager subcommand result into an exit code, printing the error
/// (and its hint) to stderr on failure.
/// Parse `helix add <name> [--path P | --url U [--sha256 H]]` into a `cli_add` call.
fn parse_add(args: &[String]) -> Result<(), crate::error::HelixError> {
    use crate::error::HelixError;
    let mkerr = |m: String| HelixError::new(m, 0, 0);
    let name = args.get(2).ok_or_else(|| {
        mkerr("`helix add` needs a name, e.g. `helix add stats --path ../stats`".to_string())
    })?;

    let (mut path, mut url, mut sha256) = (None, None, None);
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => path = Some(arg_value(args, i)?),
            "--url" => url = Some(arg_value(args, i)?),
            "--sha256" => sha256 = Some(arg_value(args, i)?),
            other => return Err(mkerr(format!("unknown option `{other}` for `helix add`"))),
        }
        i += 2;
    }

    let source = match (path, url) {
        (Some(p), None) => pkg::AddSource::Path(p),
        (None, Some(u)) => pkg::AddSource::Url { url: u, sha256 },
        (Some(_), Some(_)) => return Err(mkerr("pass either --path or --url, not both".to_string())),
        (None, None) => {
            return Err(mkerr("`helix add` needs --path <dir> or --url <tarball>".to_string()));
        }
    };
    pkg::cli_add(name, source)
}

/// The value following a flag at index `i` (`--flag value`), or a clean error.
fn arg_value(args: &[String], i: usize) -> Result<String, crate::error::HelixError> {
    args.get(i + 1)
        .cloned()
        .ok_or_else(|| crate::error::HelixError::new(format!("`{}` needs a value", args[i]), 0, 0))
}

fn pkg_result(r: Result<(), crate::error::HelixError>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e.message);
            if let Some(h) = &e.hint {
                eprintln!("  {h}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run_eval(code: &str) -> ExitCode {
    // The whole pipeline runs on the big stack so deeply-nested source can't
    // overflow the parser/type-checker/compiler before the depth guard fires.
    run_on_big_stack(|| run_source(code, "<eval>"))
}

/// Lex, parse, type-check and run a single source string under `filename` (used by
/// both `helix eval` and a `helix build` standalone artifact). Errors render against
/// `filename`. Must be called on the big stack (the front-end recurses over the AST).
fn run_source(code: &str, filename: &str) -> ExitCode {
    // Record how to re-run this program, so a sharded `listen(port, shards)` can launch
    // identical worker interpreters (no-op after the first call / on shard workers).
    serve::set_rerun(serve::Rerun::Source(code.to_string(), filename.to_string()));
    let tokens = match lexer::lex(code) {
        Ok(t) => t,
        Err(e) => {
            eprint!("{}", e.render(code, filename));
            return ExitCode::FAILURE;
        }
    };
    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            eprint!("{}", e.render(code, filename));
            return ExitCode::FAILURE;
        }
    };
    let spans = vec![module::Span {
        start_line: 1,
        source: code.to_string(),
        filename: filename.to_string(),
    }];
    match run_program(&program, &spans, false) {
        Ok(()) => ExitCode::SUCCESS,
        Err(rendered) => {
            eprint!("{}", rendered);
            ExitCode::FAILURE
        }
    }
}

/// `helix check <script>…` — load and type-check, running nothing and writing nothing.
/// The fast "does this still compile?" answer every serious toolchain has (`cargo
/// check`, `tsc --noEmit`, `go vet`).
///
/// It exists because the release pipeline broke on a file nothing ever compiled:
/// `bench/crosslang/b3_groupby.helix` still said `io.read_csv(…)`, a spelling the
/// language had removed, and a PGO training run was the first thing to notice. The
/// existing gates all *run* their programs — `cargo test` over `tests/corpus`,
/// `scripts/vmparity.sh` over `examples/` — so neither can cover a benchmark that
/// needs a 250 MB generated fixture first. Type-checking needs no fixture, so it can
/// cover every `.helix` in the repository, which is now what `scripts/checkall.sh`
/// does.
///
/// SEVERAL PATHS IN ONE PROCESS, deliberately: `helix check $(git ls-files '*.helix')`
/// is that whole-repo gate, and paying process startup plus one 2 GiB stack thread
/// once rather than 149 times is the difference between a gate people run and a gate
/// people skip.
///
/// The diagnostics are `helix run`'s own, not a second opinion — [`check_file_capture`]
/// is [`run_file_capture`] with the execution removed. A file that checks clean here
/// and still fails when run therefore failed for a *runtime* reason.
fn run_check(args: &[String]) -> ExitCode {
    let mut paths: Vec<&str> = Vec::new();
    for a in args.iter().skip(2) {
        if a.starts_with('-') {
            eprintln!("error: unknown option `{a}` for `helix check`");
            return ExitCode::FAILURE;
        }
        paths.push(a);
    }
    if paths.is_empty() {
        eprintln!("error: `helix check` needs at least one script path, e.g. `helix check main.helix`");
        return ExitCode::FAILURE;
    }
    // Resolve every path BEFORE checking any of them, so a typo in the last argument is
    // reported immediately rather than after the first 148 files have been checked.
    let mut resolved = Vec::with_capacity(paths.len());
    for p in paths {
        match resolve_script(p) {
            Ok(path) => resolved.push(path),
            Err(msg) => {
                eprint!("{msg}");
                return ExitCode::FAILURE;
            }
        }
    }
    // One big-stack thread for the whole batch: the loader and the checker both recurse
    // over the AST, and spawning that thread per file would dominate the run.
    run_on_big_stack(move || {
        let mut failed = 0usize;
        for path in &resolved {
            match check_file_capture(path) {
                Ok(()) => println!("ok   {}", path.display()),
                Err(rendered) => {
                    failed += 1;
                    println!("FAIL {}", path.display());
                    eprint!("{rendered}");
                }
            }
        }
        if resolved.len() > 1 {
            println!("checked {} files, {failed} failed", resolved.len());
        }
        if failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    })
}

/// `helix fmt <script>… [--check]` — format in place, or report which files would change.
///
/// NO OTHER FLAGS, EVER. Not "none yet" — none by design. See `src/fmt.rs` for why, but the
/// short version is that every option is a future argument, and the two tools that took the
/// other road (rustfmt's ~90 options, most nightly-only; prettier, which calls four of its
/// own "historical artifacts" and has frozen the set) both regret it publicly.
///
/// It only needs the file to LEX, never to parse, so it works on a half-written file — which
/// is the moment a formatter is most wanted and the moment prettier, rustfmt, black and
/// gofmt all refuse.
fn run_fmt(args: &[String]) -> ExitCode {
    let mut check_only = false;
    let mut paths: Vec<&str> = Vec::new();
    for a in args.iter().skip(2) {
        match a.as_str() {
            "--check" => check_only = true,
            other if other.starts_with('-') => {
                eprintln!("error: unknown option `{other}` for `helix fmt`");
                eprintln!("  the only option is `--check`; `helix fmt` has no style settings.");
                return ExitCode::FAILURE;
            }
            other => paths.push(other),
        }
    }
    if paths.is_empty() {
        eprintln!("error: `helix fmt` needs at least one script path, e.g. `helix fmt main.helix`");
        return ExitCode::FAILURE;
    }
    let mut resolved = Vec::with_capacity(paths.len());
    for p in paths {
        match resolve_script(p) {
            Ok(path) => resolved.push(path),
            Err(msg) => {
                eprint!("{msg}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut changed = 0usize;
    let mut failed = 0usize;
    for path in &resolved {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read `{}`: {e}", path.display());
                failed += 1;
                continue;
            }
        };
        let out = match fmt::format_source(&src) {
            Ok(s) => s,
            // A LEX error is the one thing that stops it: there is no token stream to be
            // faithful to. A PARSE error is fine and is formatted like anything else.
            Err(e) => {
                eprint!("{}", e.render(&src, &path.display().to_string()));
                failed += 1;
                continue;
            }
        };
        if out == src {
            continue;
        }
        changed += 1;
        if check_only {
            println!("would reformat {}", path.display());
        } else if let Err(e) = std::fs::write(path, &out) {
            eprintln!("error: cannot write `{}`: {e}", path.display());
            failed += 1;
        } else {
            println!("formatted {}", path.display());
        }
    }
    if failed > 0 {
        return ExitCode::FAILURE;
    }
    if check_only && changed > 0 {
        eprintln!("{changed} file(s) would be reformatted");
        return ExitCode::FAILURE;
    }
    if !check_only && changed == 0 {
        println!("already formatted");
    }
    ExitCode::SUCCESS
}

/// `helix build <script> [-o name]` — bundle a single-file program into a standalone
/// executable (see `src/bundle.rs`). Runs on the big stack: the build path loads and
/// type-checks the program, both of which recurse over the AST.
fn run_build(args: &[String]) -> ExitCode {
    let entry = match args.get(2) {
        // The `.helix` extension is optional here exactly as it is for `run`.
        Some(p) if !p.starts_with('-') => match resolve_script(p) {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(msg) => {
                eprint!("{msg}");
                return ExitCode::FAILURE;
            }
        },
        _ => {
            eprintln!("error: `helix build` needs a script path, e.g. `helix build main.helix -o tool`");
            return ExitCode::FAILURE;
        }
    };
    // Optional `-o <name>` / `--output <name>`.
    let mut out: Option<String> = None;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => match args.get(i + 1) {
                Some(name) => {
                    out = Some(name.clone());
                    i += 2;
                }
                None => {
                    eprintln!("error: `{}` needs an output name", args[i]);
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("error: unknown option `{other}` for `helix build`");
                return ExitCode::FAILURE;
            }
        }
    }
    run_on_big_stack(move || {
        match bundle::build(std::path::Path::new(&entry), out.as_deref().map(std::path::Path::new)) {
            Ok(path) => {
                println!("built standalone executable: {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {}", e.message);
                if let Some(h) = &e.hint {
                    eprintln!("  {h}");
                }
                ExitCode::FAILURE
            }
        }
    })
}

/// `helix emit-hbc <script> [--entry NAME] [-o out.hbc] [--dump]` — compile a program
/// and emit a `.hbc` (Helix Bytecode Container) for ctype's ring-0 `hvm` interpreter
/// (ADR 0023). Only the dependency-free core subset (Int/Float/Bool arithmetic, frame
/// locals, `if`/`while`, direct + tail calls) lowers; anything else is a precise,
/// source-attributed error. Runs on the big stack (the front-end recurses over the AST).
fn run_emit_hbc(args: &[String]) -> ExitCode {
    let entry = match args.get(2) {
        Some(p) if !p.starts_with('-') => match resolve_script(p) {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(msg) => {
                eprint!("{msg}");
                return ExitCode::FAILURE;
            }
        },
        _ => {
            eprintln!(
                "error: `helix emit-hbc` needs a script path, e.g. `helix emit-hbc main.helix --entry compute -o main.hbc`"
            );
            return ExitCode::FAILURE;
        }
    };
    // Optional `-o <out>`, `--entry <fn>`, `--dump`.
    let mut out: Option<String> = None;
    let mut entry_fn: Option<String> = None;
    let mut dump = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => match args.get(i + 1) {
                Some(name) => {
                    out = Some(name.clone());
                    i += 2;
                }
                None => {
                    eprintln!("error: `{}` needs an output name", args[i]);
                    return ExitCode::FAILURE;
                }
            },
            "--entry" => match args.get(i + 1) {
                Some(name) => {
                    entry_fn = Some(name.clone());
                    i += 2;
                }
                None => {
                    eprintln!("error: `--entry` needs a function name");
                    return ExitCode::FAILURE;
                }
            },
            "--dump" => {
                dump = true;
                i += 1;
            }
            other => {
                eprintln!("error: unknown option `{other}` for `helix emit-hbc`");
                return ExitCode::FAILURE;
            }
        }
    }
    run_on_big_stack(move || {
        let entry_path = std::path::PathBuf::from(&entry);
        // Load (read + lex + parse + namespace-resolve the import graph).
        let loaded = match module::load(&entry_path) {
            Ok(l) => l,
            Err(rendered) => {
                eprint!("{rendered}");
                return ExitCode::FAILURE;
            }
        };
        // Type-check so the compiler routes receiver-polymorphic methods correctly.
        let types = match types::check(&loaded.stmts) {
            Ok(t) => t,
            Err(e) => {
                eprint!("{}", render_err(e, &loaded.spans, loaded.multi_module));
                return ExitCode::FAILURE;
            }
        };
        // Compile to bytecode (total for any type-checked program).
        let program = match bytecode::compile_with_types(&loaded.stmts, Some(types)) {
            Ok(p) => p,
            Err(_) => {
                eprintln!(
                    "internal error: the compiler could not lower a type-checked program (please report)"
                );
                return ExitCode::FAILURE;
            }
        };
        // `--dump`: print the compiled instruction stream (to stderr) — a debugging aid.
        if dump {
            eprintln!("— compiled program ({} functions) —", program.funcs.len());
            for (fi, ch) in program.funcs.iter().enumerate() {
                let name = program.func_names.get(fi).map(|s| s.as_str()).unwrap_or("?");
                eprintln!(
                    "[{fi}] {name}  n_params={} n_locals={} consts={:?}",
                    ch.n_params, ch.n_locals, ch.consts
                );
                for (j, opx) in ch.code.iter().enumerate() {
                    eprintln!("    {j:3}: {opx:?}");
                }
            }
        }
        // Default entry: the last non-`<main>` top-level function (else `<main>`).
        let entry_name = match &entry_fn {
            Some(n) => n.clone(),
            None => program
                .func_names
                .iter()
                .rev()
                .find(|n| n.as_str() != "<main>")
                .cloned()
                .unwrap_or_else(|| "<main>".to_string()),
        };
        let emitted = match hbc::emit(&program, &entry_name) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("error: cannot emit `.hbc`: {e}");
                return ExitCode::FAILURE;
            }
        };
        let out_path = match &out {
            Some(o) => std::path::PathBuf::from(o),
            None => entry_path.with_extension("hbc"),
        };
        if let Err(e) = std::fs::write(&out_path, &emitted.bytes) {
            eprintln!("error: could not write {}: {e}", out_path.display());
            return ExitCode::FAILURE;
        }
        println!(
            "wrote {} ({} bytes, {} function(s), {} constant(s)); entry `{}` is index 0",
            out_path.display(),
            emitted.bytes.len(),
            emitted.funcs.len(),
            emitted.nconsts,
            entry_name,
        );
        // hvm addresses functions by index (no name table in `.hbc`), so print the map.
        for f in &emitted.funcs {
            println!(
                "  [{}] {}  nargs={} nlocals={}{}",
                f.index,
                f.name,
                f.nargs,
                f.nlocals,
                if f.index == 0 { "   <- entry" } else { "" }
            );
        }
        ExitCode::SUCCESS
    })
}

/// Run `f` on a thread with a 2 GiB stack. The **entire** front-end (parse,
/// namespace-resolve, type-check, compile) and the tree-walker all recurse on the
/// native stack over the AST, so they run here — and the parser's `MAX_PARSE_DEPTH`
/// guard then turns pathological nesting/chaining into a clean error well before
/// this stack could overflow (the totality guarantee). Scoped, so `f` can borrow
/// caller-local data (the source text and loaded program) without cloning.
fn run_on_big_stack<F: FnOnce() -> ExitCode + Send>(f: F) -> ExitCode {
    // A 1 GiB stack gives the tree-walker's native recursion (capped at
    // MAX_CALL_DEPTH = 20_000) ample headroom while reserving less address space than a
    // 2 GiB stack, so the spawn is far less likely to be refused under a tight
    // memory/ulimit. If the OS still refuses the thread, fail with a clean error rather
    // than aborting the process (the previous `.expect` turned constrained memory into
    // a crash for every program).
    std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .stack_size(1024 * 1024 * 1024)
            .spawn_scoped(scope, f)
        {
            Ok(handle) => handle.join().unwrap_or(ExitCode::FAILURE),
            Err(e) => {
                eprintln!("error: could not allocate the interpreter stack: {e}");
                eprintln!("help: free some memory or raise the address-space/stack ulimit.");
                ExitCode::FAILURE
            }
        }
    })
}

fn print_help() {
    println!(
        "Helix {} — a scientific programming language\n\n\
         USAGE:\n    \
         helix <script>           run a script (shorthand; `.helix` optional)\n    \
         helix run <script>       run a script (`.helix` optional: `helix run main`)\n    \
         helix eval \"<code>\"       run a one-liner\n    \
         helix check <script>…    type-check without running (fast; takes many paths)\n    \
         helix fmt <script>…      format (no options; `--check` reports instead of writing)\n    \
         helix build <script>     bundle a program into a standalone executable\n    \
         helix emit-hbc <script>  compile to a .hbc bytecode container (for ctype's hvm)\n    \
         helix repl               start an interactive session\n    \
         helix new <name>         create a helix.toml in the current directory\n    \
         helix add <name> ...     add a dependency (--path <dir> | --url <tarball>)\n    \
         helix sync               resolve dependencies and write helix.lock\n    \
         helix verify             check the project matches helix.lock (no build)\n    \
         helix test [path]        run *_test.helix files and report pass/fail\n    \
         helix doc [Type]         list a type's methods (Array/String/Dna/…) or `builtins`\n    \
         helix describe           the whole API as JSON (for LLMs/agents/tools)\n    \
         helix version            show the version\n    \
         helix help               show this help\n\n\
         The default `helix` is a self-contained binary. A build with the `python`\n\
         feature adds CPython interop (see docs/python-interop.md).",
        env!("CARGO_PKG_VERSION")
    );
}

/// Resolve a user-supplied script path, letting the `.helix` extension be omitted:
/// `helix run hello` finds `hello.helix`.
///
/// Three rules, each chosen for a reason:
///
/// 1. **An exact file wins.** If the path names a real file it is used unchanged, so an
///    extensionless script (or one with any other extension) still runs, and a directory
///    holding `hello` cannot shadow a `hello.helix` beside it.
/// 2. **The extension is APPENDED, never substituted.** `format!("{path}.helix")`, not
///    `with_extension("helix")` — the latter REPLACES, so `helix run notes.txt` would
///    silently run `notes.helix`, a different file the user did not name.
/// 3. **A directory is its own error.** Naming a directory is a different mistake from
///    naming nothing, and saying so beats "no such file" when the thing plainly exists.
///
/// Returns the rendered error, so callers just print it.
fn resolve_script(path: &str) -> Result<std::path::PathBuf, String> {
    let p = std::path::Path::new(path);
    if p.is_file() {
        return Ok(p.to_path_buf());
    }
    let with_ext = std::path::PathBuf::from(format!("{path}.helix"));
    if with_ext.is_file() {
        return Ok(with_ext);
    }
    if p.is_dir() {
        return Err(format!(
            "error: cannot read `{path}`: it is a directory, not a script\n\
             help: name a file inside it, e.g. `{path}/main.helix` (or `{path}/main`)\n"
        ));
    }
    // Only mention the appended candidate when one was actually tried. Saying "looked for
    // `x.helix` and `x.helix.helix`" to someone who already typed the extension is noise
    // that makes the tool look confused about its own filenames.
    if p.extension().is_some_and(|e| e == "helix") {
        return Err(format!("error: cannot read `{path}`: no such file\n"));
    }
    Err(format!(
        "error: cannot read `{path}`: no such file\n\
         help: looked for `{path}` and `{path}.helix`\n"
    ))
}

fn run_file(path: &str) -> ExitCode {
    let path = &match resolve_script(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(msg) => {
            eprint!("{msg}");
            return ExitCode::FAILURE;
        }
    };
    // Record how to re-run this entry file, so a sharded `listen(port, shards)` can
    // launch identical worker interpreters that re-load the same program.
    serve::set_rerun(serve::Rerun::File(std::path::PathBuf::from(path)));
    // The whole pipeline runs on the big stack (see `run_on_big_stack`) so the
    // front-end's AST recursion can't overflow before the depth guard fires.
    run_on_big_stack(|| match run_file_capture(std::path::Path::new(path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(rendered) => {
            eprint!("{}", rendered);
            ExitCode::FAILURE
        }
    })
}

/// Load, namespace-resolve, type-check and run a file, returning the rendered (already
/// caret-annotated) error instead of printing it. Must be called on the big stack. The
/// shared core of `helix run` and `helix test`.
fn run_file_capture(path: &std::path::Path) -> Result<(), String> {
    // The module loader reads, lexes, parses, and namespaces the entry file plus
    // everything it imports into one statement list. (A single file passes through
    // unchanged.) Lex/parse/resolve errors come back already rendered.
    let loaded = module::load(path)?;
    // Errors render against the spans the loader produced, so a cross-module error
    // points at the dependency's own source and line (not the entry file).
    run_program(&loaded.stmts, &loaded.spans, loaded.multi_module)
}

/// Load, namespace-resolve and type-check a file WITHOUT running it, returning the
/// rendered error. Must be called on the big stack.
///
/// This is deliberately [`run_file_capture`] with the execution removed: the same
/// loader, the same `types::check`, the same `render_err`. Writing a second front end
/// for `helix check` would let the two drift, and a checker that disagrees with the
/// runtime is worse than no checker.
fn check_file_capture(path: &std::path::Path) -> Result<(), String> {
    let loaded = module::load(path)?;
    types::check(&loaded.stmts)
        .map(|_| ())
        .map_err(|e| render_err(e, &loaded.spans, loaded.multi_module))
}

/// `helix test [path]` — discover and run test files (any file named `*_test.helix`
/// under `path`, default the current directory), each in isolation through the normal
/// pipeline. A file passes if it runs to completion without raising — `assert`,
/// `assert_eq`, and `assert_close` raise on failure. Reports per-file results and a
/// summary, exiting non-zero if any file failed. The built-in test runner: no framework
/// to install, no config — name a file `*_test.helix` and it runs.
fn cli_test(args: &[String]) -> ExitCode {
    use std::path::PathBuf;
    // Whether the root was named explicitly on the command line (vs. defaulting to
    // the current directory) — an explicit path that doesn't exist is a user error.
    let explicit = args.get(2).cloned();
    let root = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    run_on_big_stack(move || {
        // A path the user named explicitly but which doesn't exist is an error, not
        // an empty (successful) run — otherwise a typo'd path silently "passes".
        if explicit.is_some() && !root.exists() {
            eprintln!("error: no such file or directory: {}", root.display());
            return ExitCode::FAILURE;
        }
        let mut files = Vec::new();
        if root.is_file() {
            files.push(root.clone());
        } else {
            collect_test_files(&root, &mut files);
        }
        files.sort();
        // Display paths relative to the search root (its parent when the root is a single
        // file, so the file's own name still shows).
        let base = if root.is_file() { root.parent().unwrap_or(&root) } else { root.as_path() };
        // Doc examples count as tests, so "nothing to run" must account for them — a
        // library of documented modules with no `*_test.helix` beside them is a real
        // project shape, and reporting "no tests found" over a dozen live examples would
        // be the same lie this pass is removing everywhere else.
        if files.is_empty() && !any_doc_examples(&root) {
            println!(
                "no tests found (looked for `*_test.helix`, and for `>>>` doc examples, under {})",
                root.display()
            );
            return ExitCode::SUCCESS;
        }
        if !files.is_empty() {
            println!("running {} test file{}", files.len(), plural(files.len()));
        }
        let mut failed = 0usize;
        for f in &files {
            let shown = f.strip_prefix(base).unwrap_or(f).display();
            // Per file, so one file's assertions can't vouch for the next one's.
            crate::interp::ASSERTIONS_RUN.store(0, std::sync::atomic::Ordering::Relaxed);
            match run_file_capture(f) {
                Ok(())
                    if crate::interp::ASSERTIONS_RUN
                        .load(std::sync::atomic::Ordering::Relaxed)
                        == 0 =>
                {
                    // Ran clean and checked nothing. Reporting `ok` here is how a whole
                    // suite of `fn test_*` definitions that nobody calls reads as green.
                    failed += 1;
                    println!("  FAIL  {shown}");
                    println!("        this file ran to completion without asserting anything");
                    let looks_like_fn_tests =
                        std::fs::read_to_string(f).map(|s| s.contains("fn test_")).unwrap_or(false);
                    if looks_like_fn_tests {
                        println!(
                            "        it defines `fn test_…`, but `helix test` runs a file \
                             top to bottom — nothing calls them."
                        );
                        println!(
                            "        call them (`test_parses()`), or assert at the top level."
                        );
                    } else {
                        println!(
                            "        add an `assert`, `assert_eq`, or `assert_close` at the \
                             top level."
                        );
                    }
                }
                Ok(()) => println!("  ok    {shown}"),
                Err(rendered) => {
                    failed += 1;
                    println!("  FAIL  {shown}");
                    // Indent the rendered error so it reads as detail under the failure.
                    for line in rendered.lines() {
                        println!("        {line}");
                    }
                }
            }
        }
        // Then the documented examples. `docs/comments-and-docs.md` promises that a
        // documented example is executed and must still say what it says — a promise that
        // until now held only for Helix's OWN source, checked by a `cargo test` a user of
        // the language cannot run. For a library author it was decoration.
        let (doc_ok, doc_failed, skipped) = run_doc_examples(&root, base);
        // A doc example is a test: it counts in the same totals, so the summary never
        // reads `0 passed` over a screen of green.
        let passed = (files.len() - failed) + doc_ok;
        println!();
        if skipped > 0 {
            // Never silently: a runner that quietly checks less than you think is the
            // failure this whole commit is about.
            println!(
                "note: skipped doc examples in {skipped} file{} with top-level statements \
                 — running those would re-run the script's side effects",
                plural(skipped)
            );
        }
        let failed = failed + doc_failed;
        if failed == 0 {
            println!("{passed} passed");
            ExitCode::SUCCESS
        } else {
            println!("{passed} passed, {failed} failed");
            ExitCode::FAILURE
        }
    })
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Is there at least one `>>>` example anywhere under `root`? Cheap text scan, used only
/// to decide whether "no tests found" is honest.
fn any_doc_examples(root: &std::path::Path) -> bool {
    let mut sources = Vec::new();
    if root.is_file() {
        sources.push(root.to_path_buf());
    } else {
        collect_helix_files(root, &mut sources);
    }
    sources.iter().any(|p| {
        std::fs::read_to_string(p)
            .map(|s| !doctest::doc_examples_in(&s).is_empty())
            .unwrap_or(false)
    })
}

/// Run every `>>>` example in the `##` doc comments of the modules under `root`, on all
/// three engines, comparing against the output written beneath it. Returns
/// `(passed, failed, skipped)`.
///
/// Only files that are **definitions only** are used — the same rule that makes a file
/// importable (ADR 0019). Two reasons, and both matter: a module has no top-level side
/// effects, so running it to set up an example cannot re-send an email or rewrite a file;
/// and its own output is empty, so the example's output needs no baseline subtraction.
/// A script with top-level statements is skipped and counted, never silently passed over.
///
/// Each example runs as a synthesized file written BESIDE its source, so the module's own
/// relative imports resolve exactly as they normally would, and is removed immediately
/// after. The three engines must agree before the value is compared at all — an example
/// that diverges is a defect in the language, not in the documentation.
fn run_doc_examples(root: &std::path::Path, base: &std::path::Path) -> (usize, usize, usize) {
    let mut sources = Vec::new();
    if root.is_file() {
        sources.push(root.to_path_buf());
    } else {
        collect_helix_files(root, &mut sources);
    }
    sources.sort();

    let (mut ok, mut failed, mut skipped) = (0usize, 0usize, 0usize);
    for path in &sources {
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        let examples = doctest::doc_examples_in(&src);
        if examples.is_empty() {
            continue;
        }
        if !is_definitions_only(&src) {
            skipped += 1;
            continue;
        }
        let dir = path.parent().unwrap_or(std::path::Path::new("."));
        let shown = path.strip_prefix(base).unwrap_or(path).display().to_string();
        for (i, ex) in examples.iter().enumerate() {
            let where_ = format!("{shown}:{}", ex.line);
            let tmp = dir.join(format!(".helix_doc_{}_{i}.helix", std::process::id()));
            if std::fs::write(&tmp, doctest::example_program(&src, ex)).is_err() {
                skipped += 1;
                continue;
            }
            let results: Vec<(&str, String)> = [
                ("tree-walker", vec![("HELIX_NOVM", "1")]),
                ("vm", vec![("HELIX_NOJIT", "1")]),
                ("jit", vec![]),
            ]
            .iter()
            .map(|(engine, env)| (*engine, run_example_once(&tmp, env)))
            .collect();
            let _ = std::fs::remove_file(&tmp);

            // THE ORACLE FIRST: three engines must agree before the value means anything.
            if let Some((e, _)) = results.iter().find(|(_, o)| *o != results[0].1) {
                failed += 1;
                println!("  FAIL  {where_} (doc)");
                println!("        engines disagree: {} vs {}", results[0].0, e);
                println!("        {}: {}", results[0].0, results[0].1);
                println!("        {}: {}", e, results.iter().find(|(n, _)| n == e).unwrap().1);
                continue;
            }
            let got = results[0].1.trim_end();
            let want = ex.expect.join("\n");
            if !ex.expect.is_empty() && got != want.trim_end() {
                failed += 1;
                println!("  FAIL  {where_} (doc)");
                println!("        code:     {}", ex.code.join(" ; "));
                println!("        expected: {}", want.trim_end());
                println!("        got:      {got}");
            } else {
                ok += 1;
                println!("  ok    {where_} (doc)");
            }
        }
    }
    (ok, failed, skipped)
}

/// Run one synthesized example file in a child `helix` and return what it produced:
/// stdout, or — when stdout is empty and it failed — the first line of stderr, so an
/// example may document an error the same way it documents a value.
fn run_example_once(path: &std::path::Path, env: &[(&str, &str)]) -> String {
    let Ok(exe) = std::env::current_exe() else { return String::new() };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("run").arg(path).stdin(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            if stdout.is_empty() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                stderr.trim().lines().next().unwrap_or("").to_string()
            } else {
                stdout
            }
        }
        Err(e) => format!("error: could not run the example: {e}"),
    }
}

/// Does this file contain only definitions — no top-level statement that would *run*?
/// The same property that makes a file importable, checked here by parsing rather than
/// by pattern-matching text. A file that does not parse has no runnable examples either,
/// so it answers `false` and is reported as skipped.
fn is_definitions_only(src: &str) -> bool {
    match lexer::lex(src).and_then(parser::parse) {
        Ok(stmts) => !stmts.iter().any(|s| matches!(s, ast::Stmt::Expr(_))),
        Err(_) => false,
    }
}

/// Recursively collect every `.helix` file under `dir`, skipping hidden directories.
fn collect_helix_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_helix_files(&path, out);
        } else if name.ends_with(".helix") {
            out.push(path);
        }
    }
}

/// Recursively collect `*_test.helix` files under `dir`, skipping hidden directories
/// (`.git`, etc.). Unreadable directories are silently skipped.
fn collect_test_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue; // hidden files/dirs (.git, .helix caches, …)
        }
        let path = entry.path();
        if path.is_dir() {
            collect_test_files(&path, out);
        } else if name.ends_with("_test.helix") {
            out.push(path);
        }
    }
}

/// Strip internal module prefixes (`m<N>$`) from a rendered error so users never
/// see namespacing artifacts. A no-op for single-file programs (no such prefixes).
fn strip_mangling(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'm' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < chars.len() && chars[j] == '$' {
                i = j + 1; // skip `m<digits>$`
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Type-check and run an already-loaded program. On failure returns the rendered,
/// caret-annotated error (with namespacing prefixes stripped for multi-file runs).
fn run_program(program: &[ast::Stmt], spans: &[module::Span], multi: bool) -> Result<(), String> {
    // The inferred receiver types feed the compiler so it can route
    // receiver-polymorphic methods (DataFrame/Tensor column-verbs) correctly.
    let types = types::check(program).map_err(|e| render_err(e, spans, multi))?;

    // The tree-walker now runs only under `HELIX_NOVM=1` (A/B benchmarking and the
    // engine-agreement oracle). `try` used to force the whole program here — and with
    // it lose the VM/JIT/memoization and the heap-recursion depth limit — but error
    // recovery is now native in the VM (`Op::TryBegin`/`TryOk`/`TryErr` + the handler
    // unwind), so a `try` anywhere no longer demotes the program.
    if std::env::var_os("HELIX_NOVM").is_some() {
        // Already on the 2 GiB-stack thread (every caller wraps the pipeline in
        // `run_on_big_stack`), so the tree-walker's native-stack recursion has
        // headroom; run it directly rather than nesting another big-stack thread.
        let mut interp = Interp::new();
        return interp.run(program).map_err(|e| render_err(e, spans, multi));
    }

    // The VM is the sole automatic engine: the compiler is *total* for any
    // type-checked program (no `Unsupported` fallback), so there is no silent
    // tree-walker path. The VM recurses on the heap, so it runs on the main thread.
    match bytecode::compile_with_types(program, Some(types)) {
        Ok(prog) => {
            // `HELIX_NOJIT=1` disables native compilation (keeps the VM) for A/B.
            let jit = if std::env::var_os("HELIX_NOJIT").is_some() {
                None
            } else {
                jit::build(
                    program,
                    &prog.reduce_loops,
                    &prog.map_kernels,
                    &prog.filter_kernels,
                    &prog.fused_kernels,
                    &prog.scan_loops,
                )
            };
            vm::run(&prog, jit.as_ref()).map_err(|e| render_err(e, spans, multi))?
        }
        // Unreachable for a type-checked program (the compiler is total). Surface it
        // as an internal error rather than silently falling back to the tree-walker.
        Err(_) => {
            return Err(
                "internal error: the compiler could not lower a type-checked program (please report)\n"
                    .to_string(),
            )
        }
    }
    Ok(())
}

/// Render a runtime/type error against the file it came from. The error's (global)
/// line is mapped back to its owning module's source and local line, so a cross-module
/// error shows the right file and caret. Module-namespacing prefixes are stripped for
/// multi-file programs so users never see the internal `m<N>$` names.
fn render_err(mut e: HelixError, spans: &[module::Span], multi: bool) -> String {
    let (src, filename, local_line) = module::locate(spans, e.line);
    e.line = local_line;
    let r = e.render(src, filename);
    if multi { strip_mangling(&r) } else { r }
}

fn repl() -> ExitCode {
    println!(
        "Helix {} — interactive session. Type an expression and press Enter; Ctrl-D to exit.",
        env!("CARGO_PKG_VERSION")
    );
    let mut interp = Interp::new();
    // A persistent checker keeps the type env in lock-step with the value env
    // across REPL lines (so `pi`, prior `x = …`, and `fn` defs stay typed).
    let mut checker = types::Checker::new();
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        print!("helix> ");
        let _ = io::stdout().flush();
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!();
                return ExitCode::SUCCESS;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("error reading input: {}", e);
                return ExitCode::FAILURE;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        eval_repl_line(&mut interp, &mut checker, trimmed);
    }
}

fn eval_repl_line(interp: &mut Interp, checker: &mut types::Checker, src: &str) {
    let tokens = match lexer::lex(src) {
        Ok(t) => t,
        Err(e) => {
            eprint!("{}", e.render(src, "<repl>"));
            return;
        }
    };
    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            eprint!("{}", e.render(src, "<repl>"));
            return;
        }
    };
    for stmt in &program {
        // Type-check each statement before executing it; on a type error, print
        // and skip execution (mirrors the parse-error early return).
        if let Err(e) = checker.exec_stmt(stmt) {
            eprint!("{}", e.render(src, "<repl>"));
            break;
        }
        match interp.exec(stmt) {
            Ok(outcome) => {
                // Auto-echo the value of bare expressions, like a scientist's
                // notebook — rich (table/color) on a terminal, plain when piped.
                if outcome.is_expr && !matches!(outcome.value, Value::Unit) {
                    println!("{}", render::render_echo(&outcome.value));
                }
            }
            Err(e) => {
                eprint!("{}", e.render(src, "<repl>"));
                break;
            }
        }
    }
}

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
#[cfg(feature = "bio")]
mod bio;
/// The engine-less twin (ADR 0032): same names, same signatures, the clean
/// "rebuild with --features bio" error. The registry, checker, and describe
/// never notice the difference.
#[cfg(not(feature = "bio"))]
mod bio {
    use crate::error::HelixError;
    use crate::value::Value;

    pub fn read_fasta(_path: &str, line: usize, col: usize) -> Result<Value, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }

    pub fn read_fastq(_path: &str, line: usize, col: usize) -> Result<Value, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }
}
mod bioio;
mod bundle;
mod bytecode;
mod capability;
mod chart;
mod dataframe;
mod docs;
mod effects;
mod visit;
mod doctest;
mod error;
mod filelock;
mod fmt;
mod framefmt;
#[cfg(feature = "bio")]
mod gff;
#[cfg(not(feature = "bio"))]
mod gff {
    use crate::backend::Df;
    use crate::error::HelixError;

    pub fn read_gff(_path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }
}
mod hbc;
#[cfg_attr(not(feature = "http"), allow(dead_code))]
mod cookiejar;
mod http;
mod interp;
mod jit;
mod apisearch;
mod syntaxdocs;
mod envdocs;
mod regexes;
mod climain;
mod subprocess;
mod db;
mod pg;
mod jitexplain;
mod json;
mod lattice;
mod lexer;
mod ufcs;
mod managed;
mod module;
mod namespace;
mod net;
mod parser;
mod pkg;
mod python;
mod registry;
mod render;
mod report;
mod rng;
#[cfg(feature = "bio")]
mod sam;
#[cfg(not(feature = "bio"))]
mod sam {
    use crate::backend::Df;
    use crate::error::HelixError;

    pub fn read_sam(_path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }

    pub fn read_bam(_path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }

    pub fn read_bam_region(
        _path: &str,
        _region: &str,
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }
}
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
#[cfg(feature = "bio")]
mod vcf;
#[cfg(not(feature = "bio"))]
mod vcf {
    use crate::backend::Df;
    use crate::error::HelixError;

    pub fn read_vcf(_path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }

    pub fn read_bcf(_path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }

    pub fn read_vcf_region(
        _path: &str,
        _region: &str,
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError> {
        Err(crate::bioio::no_bio(line, col))
    }
}
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

use std::io::IsTerminal;

fn main() -> ExitCode {
    let code = run_cli();
    // THE LAST LINE A PROGRAM PRINTS MUST REACH THE DEVICE, OR THE PROGRAM FAILED.
    //
    // Rust flushes stdout as the process exits and DISCARDS the result, so a program
    // whose output never landed still exited 0. `helix run prog.helix > /dev/full`
    // printing one line reported success while writing nothing: the line sat in the
    // line buffer, the exit-time flush failed, and nobody asked. Enough output to
    // overflow the buffer DID fail correctly, which is the worst shape for a bug --
    // it works on the big case and lies on the small one.
    //
    // Flushing here, rather than in `print`, keeps `print` free of a per-call flush:
    // stdout is line-buffered, so the flush is usually a no-op, and a sink that must
    // stream (`emit`, `write`) still flushes on its own for latency, not correctness.
    if let Err(e) = std::io::Write::flush(&mut std::io::stdout()) {
        eprintln!("error: could not write to stdout: {e}");
        return ExitCode::from(1);
    }
    code
}

fn run_cli() -> ExitCode {
    install_robustness_hooks();
    // Before any work: a DataFrame engine this build does not have is an error, not
    // a silently different answer (see `backend::check_engine_selection`).
    if let Some(msg) = backend::check_engine_selection() {
        eprintln!("error: {msg}");
        return ExitCode::FAILURE;
    }
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
    // A malformed `HELIX_CAP` is refused here rather than silently meaning `off` — see
    // `install_from_env`. Exit 2 is "the invocation is wrong", matching the CLI's other
    // usage failures.
    if let Err(msg) = capability::install_from_env() {
        eprint!("{msg}");
        return ExitCode::from(2);
    }
    // A standalone executable built with `helix build` carries its program appended to
    // this binary. If we are such an artifact, run the embedded program and ignore the
    // command line entirely (the args belong to the user's program, not to `helix`).
    // A plain `helix` binary has no overlay, so this returns `None` and the CLI runs.
    if let Some(emb) = bundle::embedded() {
        return run_on_big_stack(move || run_embedded(emb));
    }
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        // NO ARGS WITH NO TERMINAL IS NOT A REPL REQUEST. A session with nothing to read
        // from cannot do anything, and starting one has two costs a user actually paid: a
        // pipeline or a script invocation HANGS waiting on a terminal that will never
        // arrive, and — the case that prompted this — a `helix build` artifact that has been
        // `strip`ped silently becomes an interactive session with EXIT CODE 0.
        //
        // That last one is the important half. The program is appended after the executable
        // image with a magic trailer, and `strip` rewrites the file and discards everything
        // past it, so the bundle loses its payload and falls back to plain `helix`. Exiting
        // 0 from a REPL nobody asked for turns "your program is gone" into "your program
        // printed nothing", which is the harder failure to diagnose by an order of magnitude.
        //
        // EXPLICIT `helix repl` STILL WORKS WITHOUT A TERMINAL, because feeding it lines is
        // a real thing to do. Only the implicit form refuses.
        None if !std::io::stdin().is_terminal() => {
            eprintln!(
                "error: `helix` with no arguments starts an interactive session, and there \
                 is no terminal here.\n\
                 \n\
                 help: to run a program, name it: `helix run <script>`.\n\
                 help: to read lines from a pipe deliberately, ask for it: `helix repl`.\n\
                 help: to find your way around: `helix help`, `helix doc [Type]`, \
                 `helix describe <name>`.\n\
                 help: IF THIS BINARY WAS BUILT WITH `helix build`, it has been stripped — \
                 the program is appended after the executable image and `strip` discards it. \
                 Rebuild and do not strip.\n"
            );
            ExitCode::from(2)
        }
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
            Some(path) => run_file_with_args(path, &args[3..]),
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
        // `helix effects <script> [--json]` — the authority and reproducibility closure of
        // every function, over the call graph.
        Some("effects") => run_effects(&args),
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
        Some("describe") => cli_describe(&args),
        // `helix jit-explain <script>` — which numeric kernels the JIT compiled.
        Some("jit-explain") => cli_jit_explain(&args),
        // `helix search <term>` — find a capability by what it does.
        Some("search") => cli_search(&args),
        // Shorthand: `helix script.helix [args…]` runs a file directly, arguments and all.
        Some(path) => run_file_with_args(path, &args[2..]),
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
        // The whole reference as Markdown, generated from the verified docs
        // table — docs/reference.md is this output committed, and a gate test
        // regenerates and diffs it so the two can never drift.
        Some("--markdown") => {
            print!("{}", reference_markdown());
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
                    println!("{} methods ({}):", ty, methods.len());
                    let mut names: Vec<&str> = methods.to_vec();
                    names.sort_unstable();
                    for m in names {
                        match crate::docs::method_doc(ty, m) {
                            Some(d) => {
                                println!("  {:<28} {}", d.sig, d.doc);
                                if !d.notes.is_empty() {
                                    println!("  {:<28} NOTE: {}", "", d.notes);
                                }
                            }
                            None => println!("  {m}"),
                        }
                    }
                    println!("\nUniversal (any value): {}", registry::UNIVERSAL_METHODS.join(", "));
                    println!("`helix describe <name>` gives one name's full entry as JSON.");
                    ExitCode::SUCCESS
                }
                None => {
                    // REVERSE LOOKUP: not a type — is it a METHOD or a BUILTIN? This is
                    // the question a user actually arrives with ("is there a scan, and
                    // how do I call it?"), and the project's own history shows the cost
                    // of not answering it: months spent designing around a "missing"
                    // `scan` that was one `helix doc Array` away. Owners are reported
                    // exhaustively (`mean` lives on Array, Tensor AND GroupBy; `max` is
                    // also a free function) — no metadata is invented: name, owners,
                    // effect and an example receiver all exist in the registry today,
                    // while signatures do not (see docs/dx-plan.md, do-later).
                    let mut found = false;
                    for (ty, methods) in tables {
                        if methods.contains(&query) {
                            let eff = capability::method_effect_of(query).label();
                            let recv = suggest::receiver_for(ty);
                            match crate::docs::method_doc(ty, query) {
                                Some(d) => {
                                    let _ = recv;
                                    println!(
                                        "`{}` on {ty} (effect: {eff}): {} — e.g. `{}` => {}",
                                        d.sig,
                                        d.doc,
                                        d.example,
                                        if d.example_out.is_empty() { "…" } else { d.example_out },
                                    );
                                    if !d.notes.is_empty() {
                                        println!("  NOTE: {}", d.notes);
                                    }
                                }
                                None => println!(
                                    "`{query}` is a method on {ty} (effect: {eff}) — e.g. \
                                     `{recv}.{query}(...)`; full list: `helix doc {ty}`"
                                ),
                            }
                            found = true;
                        }
                    }
                    if registry::UNIVERSAL_METHODS.contains(&query) {
                        println!(
                            "`{query}` is a universal method (any value) — e.g. `x.{query}()`"
                        );
                        found = true;
                    }
                    if let Some(b) = registry::lookup(query) {
                        let eff = capability::effect_of(b.path).label();
                        println!(
                            "`{query}` is a free function (effect: {eff}, category: {}) — \
                             see `helix doc builtins`",
                            registry::category_of(b.path)
                        );
                        found = true;
                    }
                    if found {
                        return ExitCode::SUCCESS;
                    }
                    // Unknown everywhere: the same suggester every "is not defined"
                    // error routes through (foreign aliases first, then one-edit typos),
                    // then the original unknown-type wording.
                    let known: Vec<&str> = tables.iter().map(|(t, _)| *t).collect();
                    eprintln!(
                        "error: unknown type `{}`. Try one of: {} (or `builtins`).",
                        query,
                        known.join(", ")
                    );
                    if let Some(h) = suggest::hint(query, suggest::Site::Function, &[]) {
                        eprintln!("help: {h}");
                    }
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
/// Render a checker [`types::Type`] for the catalog — recursive where `Display` is flat,
/// because "Array<Float>" vs "Array<Record{…}>" is exactly the information a caller
/// chaining methods needs. `Num` renders as the honest "Int|Float".
fn render_type(t: &types::Type) -> String {
    use types::Type;
    match t {
        Type::Array(el) => format!("Array<{}>", render_type(el)),
        Type::Tuple(ts) => {
            let inner: Vec<String> = ts.iter().map(render_type).collect();
            format!("Tuple<{}>", inner.join(", "))
        }
        Type::Record(fields) => {
            let inner: Vec<String> =
                fields.iter().map(|(n, ft)| format!("{n}: {}", render_type(ft))).collect();
            format!("Record{{{}}}", inner.join(", "))
        }
        Type::Num => "Int|Float".to_string(),
        other => other.to_string(),
    }
}

/// The return type of `name` at arity `k`, from the checker's own tables, or `None`
/// when it genuinely depends on the inputs. The `Unknown`-vector probe answers the
/// input-independent case; otherwise concrete palettes are tried, and only UNANIMITY
/// across the palettes the checker accepts is reported — a guess is worse than a null.
fn probe_returns(name: &str, k: usize) -> Option<String> {
    use types::Type;
    if let Some(t) = types::probe_builtin(name, &vec![Type::Unknown; k])
        && !matches!(t, Type::Unknown)
    {
        return Some(render_type(&t));
    }
    let palettes =
        [Type::Float, Type::Int, Type::String, Type::Array(Box::new(Type::Float))];
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for p in palettes {
        if let Some(t) = types::probe_builtin(name, &vec![p; k])
            && !matches!(t, Type::Unknown)
        {
            seen.insert(render_type(&t));
        }
    }
    (seen.len() == 1).then(|| seen.into_iter().next().unwrap())
}

/// The generated stdlib reference: every builtin (grouped by category) and
/// every method (grouped by type), each with its signature, doc line, notes,
/// and executed example. Deterministic ordering, so the committed file diffs
/// cleanly against a regeneration.
fn reference_markdown() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "# The Helix reference");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "> Generated by `helix doc --markdown` from the docs table (src/docs.rs) — every \
         example with an output is EXECUTED by the gate. Do not edit by hand; regenerate \
         with `helix doc --markdown > docs/reference.md`."
    );
    let _ = writeln!(s);
    let entry = |s: &mut String, d: &docs::DocEntry, differentiable: bool| {
        let _ = writeln!(s, "### `{}`", d.sig);
        let _ = writeln!(s);
        let mut doc = d.doc.to_string();
        if differentiable {
            doc.push_str(" *(differentiable)*");
        }
        let _ = writeln!(s, "{doc}");
        if !d.notes.is_empty() {
            let _ = writeln!(s);
            let _ = writeln!(s, "**Note:** {}", d.notes);
        }
        let _ = writeln!(s);
        let _ = writeln!(s, "```");
        let _ = writeln!(s, ">>> {}", d.example);
        if !d.example_out.is_empty() {
            for l in d.example_out.split("\n") {
                let _ = writeln!(s, "{l}");
            }
        }
        let _ = writeln!(s, "```");
        let _ = writeln!(s);
    };
    // Builtins by category, both levels sorted.
    let mut cats: Vec<&str> =
        registry::BUILTINS.iter().map(|b| registry::category_of(b.path)).collect();
    cats.sort_unstable();
    cats.dedup();
    let _ = writeln!(s, "## Free functions");
    let _ = writeln!(s);
    for cat in cats {
        let mut names: Vec<&str> = registry::BUILTINS
            .iter()
            .filter(|b| registry::category_of(b.path) == cat)
            .map(|b| b.path)
            .collect();
        names.sort_unstable();
        let _ = writeln!(s, "## {cat}");
        let _ = writeln!(s);
        for n in names {
            if let Some(d) = docs::builtin_doc(n) {
                entry(&mut s, d, autodiff::differentiable_builtin(n));
            }
        }
    }
    // THE ENVIRONMENT first of all, because the capability sandbox lives here and a
    // security feature that can only be found by grepping the compiler is one nobody uses.
    let _ = writeln!(s, "## Environment");
    let _ = writeln!(s);
    for e in envdocs::ENV {
        let _ = writeln!(s, "### `{}`", e.name);
        let _ = writeln!(s);
        let _ = writeln!(s, "{}", e.doc);
        let _ = writeln!(s);
        let _ = writeln!(s, "- **Values:** {}", e.values);
        let _ = writeln!(s, "- **Unset:** {}", e.default);
        if !e.notes.is_empty() {
            let _ = writeln!(s);
            let _ = writeln!(s, "**Note:** {}", e.notes);
        }
        let _ = writeln!(s);
    }

    // LANGUAGE FORMS lead, because they are what a reader needs before any API: the
    // reference documented every callable and none of the syntax.
    let _ = writeln!(s, "## Language forms");
    let _ = writeln!(s);
    for f in syntaxdocs::SYNTAX {
        let _ = writeln!(s, "### `{}`", f.name);
        let _ = writeln!(s);
        let _ = writeln!(s, "{}", f.doc);
        let _ = writeln!(s);
        let _ = writeln!(s, "`{}`", f.form);
        if !f.notes.is_empty() {
            let _ = writeln!(s);
            let _ = writeln!(s, "**Note:** {}", f.notes);
        }
        let _ = writeln!(s);
        let _ = writeln!(s, "```");
        for l in f.example.split('\n') {
            let _ = writeln!(s, ">>> {l}");
        }
        for l in f.example_out.split('\n') {
            let _ = writeln!(s, "{l}");
        }
        let _ = writeln!(s, "```");
        let _ = writeln!(s);
    }
    for (ty, ms) in registry::type_method_tables() {
        let mut names: Vec<&str> = ms.to_vec();
        names.sort_unstable();
        let _ = writeln!(s, "## {ty} methods");
        let _ = writeln!(s);
        for n in names {
            if let Some(d) = docs::method_doc(ty, n) {
                entry(&mut s, d, false);
            }
        }
    }
    s
}

/// One language form as JSON. Kept beside `enrich` because the two shapes deliberately
/// share field names (`doc`, `example`, `example_out`, `notes`): a consumer that already
/// renders an API entry renders a form with no new code, and only `form` is new.
fn syntax_json(s: &syntaxdocs::SyntaxDoc) -> serde_json::Value {
    let mut e = serde_json::json!({
        "kind": "syntax",
        "name": s.name,
        "form": s.form,
        "doc": s.doc,
        "example": s.example,
        "example_out": s.example_out,
    });
    if !s.notes.is_empty() {
        e["notes"] = serde_json::Value::String(s.notes.to_string());
    }
    e
}

/// Fold a docs-table entry into a describe JSON object (absent fields stay
/// absent — an empty string is not information).
fn enrich(entry: &mut serde_json::Value, doc: Option<&'static docs::DocEntry>) {
    if let Some(d) = doc {
        entry["sig"] = serde_json::Value::String(d.sig.to_string());
        entry["doc"] = serde_json::Value::String(d.doc.to_string());
        entry["example"] = serde_json::Value::String(d.example.to_string());
        if !d.example_out.is_empty() {
            entry["example_out"] = serde_json::Value::String(d.example_out.to_string());
        }
        if !d.notes.is_empty() {
            entry["notes"] = serde_json::Value::String(d.notes.to_string());
        }
    }
}

/// `helix describe <name>` — the one entry (or every type's entry for a shared
/// method name), as JSON. Unknown names route through the same suggester every
/// "is not defined" error uses.
fn cli_describe_one(query: &str) -> ExitCode {
    use crate::{capability, registry};
    let mut found: Vec<serde_json::Value> = Vec::new();
    if let Some(b) = registry::lookup(query) {
        let mut entry = serde_json::json!({
            "kind": "builtin",
            "name": b.path,
            "pure": b.pure,
            "effect": capability::effect_of(b.path).label(),
            "category": registry::category_of(b.path),
        });
        enrich(&mut entry, docs::builtin_doc(b.path));
        if autodiff::differentiable_builtin(b.path) {
            entry["differentiable"] = serde_json::Value::Bool(true);
        }
        found.push(entry);
    }
    for (ty, ms) in registry::type_method_tables() {
        if ms.contains(&query) {
            let mut entry = serde_json::json!({
                "kind": "method",
                "on": ty,
                "name": query,
                "effect": capability::method_effect_of(query).label(),
            });
            enrich(&mut entry, docs::method_doc(ty, query));
            found.push(entry);
        }
    }
    if registry::UNIVERSAL_METHODS.contains(&query) {
        found.push(serde_json::json!({ "kind": "universal_method", "name": query }));
    }
    // A LANGUAGE FORM. `match`, `try` and `missing` are not builtins, so every one of
    // them answered "is not a builtin, method, or type name" — the reader who typed the
    // thing they saw in a program got told it does not exist.
    if let Some(s) = syntaxdocs::syntax_doc(query) {
        found.push(syntax_json(s));
    }
    // AN ENVIRONMENT VARIABLE. Same reason: a reader who has seen `HELIX_CAP` in a
    // deployment script and asks about it should not be told it does not exist.
    if let Some(e) = envdocs::env_doc(query) {
        found.push(serde_json::json!({
            "kind": "env",
            "name": e.name,
            "values": e.values,
            "default": e.default,
            "doc": e.doc,
            "notes": e.notes,
        }));
    }
    // A receiver TYPE name (`DataFrame`, `Array`, `Dna`, …): the whole method table with
    // each method's signature, doc, effect and example — the machine-readable half of
    // `helix doc <Type>`, which prints for a human and cannot be parsed.
    //
    // This was the ONE shape unavailable as JSON. `helix describe` dumps everything
    // (120 KB) and `helix describe <name>` answers about a name you already know; the
    // question "what can I do with a DataFrame?" — the one you ask BEFORE you know any
    // names, and the one this project's costliest mistake was months of not asking about
    // `scan` — had a human-readable answer only.
    //
    // Type names are capitalised and method names are not, so a collision is not
    // reachable today; this folds into `found` anyway, so if one ever appears the caller
    // gets both entries instead of one silently winning.
    if let Some((ty, ms)) = registry::type_method_tables().into_iter().find(|(t, _)| *t == query) {
        let methods: Vec<serde_json::Value> = ms
            .iter()
            .map(|m| {
                let mut e = serde_json::json!({
                    "name": m,
                    "effect": capability::method_effect_of(m).label(),
                });
                enrich(&mut e, docs::method_doc(ty, m));
                e
            })
            .collect();
        found.push(serde_json::json!({
            "kind": "type",
            "name": ty,
            "method_count": methods.len(),
            "methods": methods,
            // Available on EVERY receiver, so they are held once rather than repeated
            // into each table (registry.rs) — and would otherwise look absent here.
            "universal_methods": registry::UNIVERSAL_METHODS,
        }));
    }
    if found.is_empty() {
        eprintln!(
            "error: `{query}` is not a builtin, method, type name, language form, or              environment variable."
        );
        if let Some(h) = suggest::hint(query, suggest::Site::Function, &[]) {
            eprintln!("help: {h}");
        }
        // `helix describe dataframe` reaches the BUILTIN and never lands here; this
        // catches `Dataframe`/`DATAFRAME`, where the reader clearly meant the type.
        if let Some((ty, _)) = registry::type_method_tables()
            .into_iter()
            .find(|(t, _)| t.eq_ignore_ascii_case(query))
        {
            eprintln!("help: did you mean the type `{ty}`? `helix describe {ty}` lists its methods.");
        }
        eprintln!(
            "help: `helix describe <Type>` lists a type's methods as JSON, `helix doc <Type>` prints \
             them for a human, and `helix describe` alone dumps everything."
        );
        return ExitCode::FAILURE;
    }
    match serde_json::to_string_pretty(&serde_json::Value::Array(found)) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: could not serialize: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `helix jit-explain <script> [--json]` — which numeric kernel sites the compiler
/// offered to the JIT, where they are, and which of them got native code.
///
/// Compiles the program and builds the JIT; it does NOT run it. Answering "is my hot
/// loop compiled?" must not require executing a program that reads files or opens
/// sockets — and the answer is a property of compilation, not of a particular run.
fn cli_jit_explain(args: &[String]) -> ExitCode {
    let json = args.iter().skip(2).any(|a| a == "--json");
    let Some(p) = args.iter().skip(2).find(|a| !a.starts_with('-')) else {
        eprintln!("error: `helix jit-explain` needs a script path, e.g. `helix jit-explain hot.helix`");
        return ExitCode::FAILURE;
    };
    if let Some(bad) = args.iter().skip(2).find(|a| a.starts_with('-') && a.as_str() != "--json") {
        eprintln!("error: unknown option `{bad}` for `helix jit-explain` (the only flag is `--json`)");
        return ExitCode::FAILURE;
    }
    let path = match resolve_script(p) {
        Ok(path) => path,
        Err(msg) => {
            eprint!("{msg}");
            return ExitCode::FAILURE;
        }
    };
    let shown = path.display().to_string();
    run_on_big_stack(move || {
        let mut loaded = match module::load(&path) {
            Ok(l) => l,
            Err(rendered) => {
                eprint!("{rendered}");
                return ExitCode::FAILURE;
            }
        };
        let types = match types::check(&loaded.stmts) {
            Ok(t) => t,
            Err(e) => {
                eprint!("{}", render_err(e, &loaded.spans, loaded.multi_module));
                return ExitCode::FAILURE;
            }
        };
        ufcs::resolve_by_type(&mut loaded.stmts, &types);
        let Ok(prog) = bytecode::compile_with_types(&loaded.stmts, Some(types)) else {
            eprintln!("internal error: the compiler could not lower a type-checked program (please report)");
            return ExitCode::FAILURE;
        };
        // The same switch the run path honours, so this reports what a run WOULD do
        // rather than what an unconfigured build could do.
        let nojit = std::env::var_os("HELIX_NOJIT").is_some();
        let jit = if nojit {
            None
        } else {
            jit::build(
                &loaded.stmts,
                &prog.reduce_loops,
                &prog.map_kernels,
                &prog.filter_kernels,
                &prog.fused_kernels,
                &prog.scan_loops,
            )
        };
        // Three states, kept apart so the report never blames a shape for something
        // else: the switch, an absent codegen backend, and a real per-site refusal.
        // `jit.is_some()` alone conflates all three — and would have told every reader
        // on aarch64 or macOS that their loops are shaped wrong.
        let engine = if !cfg!(feature = "jit") || nojit {
            jitexplain::Engine::Off
        } else if jit.is_none() {
            jitexplain::Engine::NothingBuilt
        } else {
            jitexplain::Engine::Live
        };
        let sites = jitexplain::sites(&prog, jit.as_ref());
        // Whole compiled functions are a SECOND family, invisible to a `TryJit*` walk.
        let fns = jitexplain::functions(&prog, jit.as_ref());
        if json {
            let doc =
                jitexplain::to_json(&shown, &sites, &fns, engine, &loaded.spans, loaded.multi_module);
            match serde_json::to_string_pretty(&doc) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("error: could not serialize: {e}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            print!(
                "{}",
                jitexplain::render(&shown, &sites, &fns, engine, &loaded.spans, loaded.multi_module)
            );
        }
        ExitCode::SUCCESS
    })
}

/// `helix search <term> [--json]` — find a capability by what it DOES.
///
/// `doc` and `describe` both need a name; `describe` alone is 120 KB. So the only way to
/// ask "is there anything here for repeated headers?" was to dump the catalog and grep —
/// which is what two field reports did, and what this project's costliest mistake was
/// months of not doing. This searches names, signatures, docs AND notes, because the
/// words a reader has ("repeated header", "group by") are rarely the names they need
/// (`get_all`, `frequencies`).
fn cli_search(args: &[String]) -> ExitCode {
    let json = args.iter().skip(2).any(|a| a == "--json");
    let terms: Vec<&String> = args.iter().skip(2).filter(|a| !a.starts_with('-')).collect();
    if let Some(bad) = args.iter().skip(2).find(|a| a.starts_with('-') && a.as_str() != "--json") {
        eprintln!("error: unknown option `{bad}` for `helix search` (the only flag is `--json`)");
        return ExitCode::FAILURE;
    }
    let Some(query) = terms.first() else {
        eprintln!("error: `helix search` needs a term, e.g. `helix search header`");
        eprintln!("help: it looks at names, signatures, docs and notes — a plain word works best.");
        return ExitCode::FAILURE;
    };
    let hits = apisearch::search(query);
    if json {
        match serde_json::to_string_pretty(&apisearch::to_json(query, &hits)) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("error: could not serialize: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        print!("{}", apisearch::render(query, &hits));
    }
    // Finding nothing is an ANSWER, not a failure: a script asking "does this exist?"
    // should read the count, and a shell pipeline should not abort on a negative result.
    ExitCode::SUCCESS
}

fn cli_describe(args: &[String]) -> ExitCode {
    use crate::{capability, registry};
    // `helix describe <name>` — ONE name's full entry (the 45 KB dump made
    // every lookup a filtering job, so the field wrote a Python script to do
    // what this argument now does).
    if let Some(query) = args.get(2) {
        return cli_describe_one(query);
    }
    let builtins: Vec<serde_json::Value> = registry::BUILTINS
        .iter()
        .map(|b| {
            // Arity by probing the checker with `Unknown` argument vectors: the accepted
            // lengths ARE the signature the checker enforces. A builtin whose arm never
            // checks `args.len()` accepts every probe — reported as `signatures: null`
            // (the checker does not constrain it; fabricating "0..=8" would be a lie),
            // with the return type still reported when it is arity-independent.
            let accepted: Vec<usize> = (0..=8)
                .filter(|&k| {
                    types::probe_builtin(b.path, &vec![types::Type::Unknown; k]).is_some()
                })
                .collect();
            let (signatures, loose_returns) = if accepted.len() == 9 {
                (serde_json::Value::Null, probe_returns(b.path, 1))
            } else {
                let sigs: Vec<serde_json::Value> = accepted
                    .iter()
                    .map(|&k| {
                        serde_json::json!({
                            "args": k,
                            "returns": probe_returns(b.path, k),
                        })
                    })
                    .collect();
                (serde_json::Value::Array(sigs), None)
            };
            let mut entry = serde_json::json!({
                "name": b.path,
                "pure": b.pure,
                "effect": capability::effect_of(b.path).label(),
                "category": registry::category_of(b.path),
                "signatures": signatures,
            });
            if let Some(r) = loose_returns {
                entry["returns"] = serde_json::Value::String(r);
            }
            enrich(&mut entry, docs::builtin_doc(b.path));
            if autodiff::differentiable_builtin(b.path) {
                entry["differentiable"] = serde_json::Value::Bool(true);
            }
            entry
        })
        .collect();
    let mut methods = serde_json::Map::new();
    for (ty, ms) in registry::type_method_tables() {
        let arr: Vec<serde_json::Value> = ms
            .iter()
            .map(|&m| {
                let mut entry = serde_json::json!({
                    "name": m,
                    "effect": capability::method_effect_of(m).label(),
                });
                enrich(&mut entry, docs::method_doc(ty, m));
                entry
            })
            .collect();
        methods.insert(ty.to_string(), serde_json::Value::Array(arr));
    }
    let doc = serde_json::json!({
        "helix_version": env!("CARGO_PKG_VERSION"),
        "builtins": builtins,
        "methods": methods,
        "universal_methods": registry::UNIVERSAL_METHODS,
        // The language FORMS, which have no name in any registry and were therefore absent
        // from every machine-readable view of Helix until now.
        "syntax": syntaxdocs::SYNTAX.iter().map(syntax_json).collect::<Vec<_>>(),
        // The ENVIRONMENT, including the capability sandbox — a complete security feature
        // that could previously be discovered only by grepping the compiler.
        "environment": envdocs::ENV.iter().map(|e| serde_json::json!({
            "kind": "env",
            "name": e.name,
            "values": e.values,
            "default": e.default,
            "doc": e.doc,
            "notes": e.notes,
        })).collect::<Vec<_>>(),
        // Read by the source but deliberately NOT configuration. Listed so the drift
        // guard can tell "decided" from "forgotten".
        "environment_internal": envdocs::INTERNAL.iter().map(|(n, why)| serde_json::json!({
            "name": n,
            "reason": why,
        })).collect::<Vec<_>>(),
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
    let mut program = match parser::parse(tokens) {
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
        // `helix eval` has no file and no project, so it has no place in an archive
        // either -- this program is never bundled. The name is what a bundle WOULD call
        // it, kept honest rather than left blank.
        key: filename.to_string(),
    }];
    match run_program(&mut program, &spans, false) {
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
    let mut lint = false;
    // Scanned BEFORE the option loop, not during it. A caller that asked for JSON must
    // get JSON for every way the command can fail, including the failures that happen
    // before any file is opened — otherwise a tool that parses stdout hits a decode
    // error and learns nothing about why. Deciding it inside the loop would leave
    // `helix check --oops --json` reporting in prose because the bad flag came first.
    let json = args.iter().skip(2).any(|a| a == "--json");
    // A command-level failure (bad flag, no paths, unreadable path) in the same envelope
    // as a diagnostic, so a consumer parses ONE shape.
    let bail = |rendered: String, file: Option<&str>| -> ExitCode {
        if !json {
            eprint!("{rendered}");
            return ExitCode::FAILURE;
        }
        let d = serde_json::json!({
            "severity": "error",
            "file": file.unwrap_or(""),
            "rendered": rendered,
        });
        let doc = match file {
            Some(f) => serde_json::json!({
                "ok": false, "helix_version": env!("CARGO_PKG_VERSION"),
                "checked": 0, "failed": 1,
                "files": [{ "file": f, "ok": false, "diagnostics": [d] }],
            }),
            // Not about any one file: `files` stays empty and the problem is reported
            // at the top level rather than invented onto a path that was never opened.
            None => serde_json::json!({
                "ok": false, "helix_version": env!("CARGO_PKG_VERSION"),
                "checked": 0, "failed": 1,
                "files": [], "diagnostics": [d],
            }),
        };
        match serde_json::to_string_pretty(&doc) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: could not serialize: {e}"),
        }
        ExitCode::FAILURE
    };
    for a in args.iter().skip(2) {
        if a == "--lint" {
            lint = true;
            continue;
        }
        if a == "--json" {
            continue;
        }
        if a.starts_with('-') {
            return bail(
                format!("error: unknown option `{a}` for `helix check` (the flags are `--lint` and `--json`)\n"),
                None,
            );
        }
        paths.push(a);
    }
    if paths.is_empty() {
        return bail(
            "error: `helix check` needs at least one script path, e.g. `helix check main.helix`\n"
                .to_string(),
            None,
        );
    }
    // Resolve every path BEFORE checking any of them, so a typo in the last argument is
    // reported immediately rather than after the first 148 files have been checked.
    let mut resolved = Vec::with_capacity(paths.len());
    for p in paths {
        match resolve_script(p) {
            Ok(path) => resolved.push(path),
            // No line or column: the file was never opened, so reporting one would be a
            // fabrication. `rendered` still carries the full message and its help line.
            Err(msg) => return bail(msg, Some(p)),
        }
    }
    // One big-stack thread for the whole batch: the loader and the checker both recurse
    // over the AST, and spawning that thread per file would dominate the run.
    run_on_big_stack(move || {
        let mut failed = 0usize;
        let mut files_json: Vec<serde_json::Value> = Vec::new();
        // Every file linted in this invocation, canonical path, so a module imported by
        // two entries is reported once.
        let mut linted: std::collections::HashSet<String> = std::collections::HashSet::new();
        for path in &resolved {
            let shown = path.display().to_string();
            if json {
                let mut diags: Vec<serde_json::Value> = Vec::new();
                let mut graph = None;
                let ok = match check_file_structured(path) {
                    Ok(loaded) => {
                        graph = Some(loaded);
                        true
                    }
                    Err(d) => {
                        diags.push(diag_json("error", &shown, &d));
                        false
                    }
                };
                // A lint is a NOTE, not a failure: it never changes `ok` or the exit
                // code, exactly as in the human output. `file` is per-NOTE now, not the
                // entry: a note about an imported module names that module.
                if lint && let Some(loaded) = &graph {
                    for (file, src) in lint_units(loaded, &shown, &mut linted) {
                        for note in lint_source(&file, &src) {
                            diags.push(serde_json::json!({
                                "severity": "note", "file": file, "rendered": note
                            }));
                        }
                    }
                }
                if !ok {
                    failed += 1;
                }
                files_json.push(serde_json::json!({
                    "file": shown, "ok": ok, "diagnostics": diags
                }));
                continue;
            }
            match check_file_capture(path) {
                Ok(loaded) => {
                    println!("ok   {shown}");
                    if lint {
                        for (file, src) in lint_units(&loaded, &shown, &mut linted) {
                            for note in lint_source(&file, &src) {
                                println!("lint {note}");
                            }
                        }
                    }
                }
                Err(rendered) => {
                    failed += 1;
                    println!("FAIL {shown}");
                    eprint!("{rendered}");
                }
            }
        }
        if json {
            let doc = serde_json::json!({
                "ok": failed == 0,
                "helix_version": env!("CARGO_PKG_VERSION"),
                "checked": resolved.len(),
                "failed": failed,
                "files": files_json,
            });
            match serde_json::to_string_pretty(&doc) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("error: could not serialize: {e}");
                    return ExitCode::FAILURE;
                }
            }
            return if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE };
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

/// `helix effects <script> [--json]` — what each function REACHES.
///
/// Helix already classifies every builtin along two independent axes and guards both
/// exhaustively; this propagates them over the call graph and shows the path that
/// introduced each one. "not reproducible" is not actionable on its own — "not
/// reproducible: `now`, via report -> stamp" is.
///
/// Deliberately NOT part of `check`. That contract — never rejects a runnable program — is
/// what makes the edit/run loop usable, and this answers a different question rather than
/// the same one more harshly.
fn run_effects(args: &[String]) -> ExitCode {
    let json = args.iter().any(|a| a == "--json");
    let Some(path) = args.iter().skip(2).find(|a| !a.starts_with("--")) else {
        eprintln!("error: `helix effects` needs a script path, e.g. `helix effects main.helix`");
        return ExitCode::from(2);
    };
    let path = match resolve_script(path) {
        Ok(p) => p,
        Err(msg) => {
            eprint!("{msg}");
            return ExitCode::FAILURE;
        }
    };
    run_on_big_stack(move || {
        // A PROGRAM `check` REJECTS GETS NO VERDICT. An unresolvable callee contributes no
        // effects, so a file with a misspelled builtin read as pure (field build, 1.29a); a
        // report is only meaningful for a program that would run, and the refusal is
        // `check`'s own sentence.
        let loaded = match check_file_capture(&path) {
            Ok(l) => l,
            Err(rendered) => {
                eprint!("{rendered}");
                return ExitCode::FAILURE;
            }
        };
        let mut all = effects::closure(&loaded.stmts);
        // THE NAMES A READER SEES ARE THE ONES THEY WROTE. A multi-file program
        // namespaces every top-level name (`m8$version`), which is an internal spelling
        // and appears in no source file; error messages already strip it, and a report is
        // not a different kind of output. JSON gets the same treatment for the same
        // reason — a name in a machine-readable report is still a name someone greps for.
        if loaded.multi_module {
            for f in &mut all {
                f.name = strip_mangling(&f.name);
                for reach in [&mut f.does, &mut f.carries] {
                    for (_, path) in &mut reach.effects {
                        for step in path.iter_mut() {
                            *step = strip_mangling(step);
                        }
                    }
                    for (who, path) in [&mut reach.nondeterministic, &mut reach.unknown].into_iter().flatten() {
                        *who = strip_mangling(who);
                        for step in path.iter_mut() {
                            *step = strip_mangling(step);
                        }
                    }
                }
            }
        }
        if json {
            let reach_json = |r: &effects::Reach| {
                serde_json::json!({
                    "effects": r.effects.iter().map(|(e, p)| serde_json::json!({
                        "effect": e.label(),
                        "via": p,
                    })).collect::<Vec<_>>(),
                    "nondeterministic_via": r.nondeterministic.as_ref().map(|(n, p)| {
                        serde_json::json!({ "name": n, "via": p })
                    }),
                    "unknown_via": r.unknown.as_ref().map(|(n, p)| {
                        serde_json::json!({ "name": n, "via": p })
                    }),
                })
            };
            // The top level is what the function DOES (the keys a consumer already reads);
            // `carries` is the second bucket, and `deterministic` is false while any callee
            // is unknown — the failure direction an audit tool must have.
            let out: Vec<serde_json::Value> = all
                .iter()
                .map(|f| {
                    let mut v = reach_json(&f.does);
                    v["name"] = serde_json::json!(f.name);
                    v["deterministic"] = serde_json::json!(f.deterministic());
                    v["carries"] = reach_json(&f.carries);
                    v
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            return ExitCode::SUCCESS;
        }
        if all.is_empty() {
            println!("no functions in {}", path.display());
            return ExitCode::SUCCESS;
        }
        let opts = render::RenderOpts::auto();
        let sep = report::Report::sep(&opts);
        let mut r = report::Report::new(
            "effects",
            format!("{} function(s) in {}", all.len(), path.display()),
        );
        let via = |p: &[String]| p.join(" -> ");
        for f in &all {
            // What the function DOES when called.
            let labels: Vec<&str> = f.does.effects.iter().map(|(e, _)| e.label()).collect();
            let authority = match (labels.is_empty(), f.does.unknown.is_some()) {
                (true, false) => "no authority".to_string(),
                (true, true) => "unknown authority".to_string(),
                (false, false) => report::Report::list(&opts, &labels),
                (false, true) => format!("{} + unknown authority", report::Report::list(&opts, &labels)),
            };
            let repro = if f.does.nondeterministic.is_some() {
                "not reproducible"
            } else if f.does.unknown.is_some() {
                "reproducibility unknown"
            } else {
                "reproducible"
            };
            let mut notes: Vec<String> =
                f.does.effects.iter().map(|(e, p)| format!("{} via {}", e.label(), via(p))).collect();
            if let Some((who, p)) = &f.does.nondeterministic {
                notes.push(format!("`{who}` via {}", via(p)));
            }
            if let Some((who, p)) = &f.does.unknown {
                notes.push(format!("calls `{who}` via {}", via(p)));
            }
            let value = format!("{authority}{sep}{repro}");
            r = if notes.is_empty() {
                r.field_owned(f.name.clone(), value)
            } else {
                r.note_owned(f.name.clone(), value, notes.join("; "))
            };
            // What the values it builds or hands on CAN do — its own row, because a
            // constructor that returns closures does nothing itself (field build, 1.47).
            if !f.carries.is_empty() {
                let labels: Vec<&str> = f.carries.effects.iter().map(|(e, _)| e.label()).collect();
                let mut parts: Vec<String> = Vec::new();
                if !labels.is_empty() {
                    parts.push(report::Report::list(&opts, &labels));
                }
                if f.carries.unknown.is_some() {
                    parts.push("unknown authority".to_string());
                }
                if f.carries.nondeterministic.is_some() {
                    parts.push("not reproducible".to_string());
                }
                let mut notes: Vec<String> =
                    f.carries.effects.iter().map(|(e, p)| format!("{} via {}", e.label(), via(p))).collect();
                if let Some((who, p)) = &f.carries.nondeterministic {
                    notes.push(format!("`{who}` via {}", via(p)));
                }
                if let Some((who, p)) = &f.carries.unknown {
                    notes.push(format!("calls `{who}` via {}", via(p)));
                }
                r = r.note_owned(format!("{} carries", f.name), parts.join(sep), notes.join("; "));
            }
        }
        r.print(&opts);
        ExitCode::SUCCESS
    })
}

/// `helix build <script> [-o name]` — bundle a program and everything it imports into a
/// standalone executable (see `src/bundle.rs`). Runs on the big stack: the build path loads and
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
    // Optional `-o <name>` / `--output <name>`, and `--runtime <path>`.
    let mut out: Option<String> = None;
    let mut runtime: Option<String> = None;
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
            // WHICH RUNTIME TO EMBED. Without this the artifact is a copy of whatever
            // `helix` you happened to invoke — and a field report shipped a 120 MB web
            // server because that binary was the GATE build: every feature linked in plus
            // debug symbols, for a program that touches no DataFrame, no genomics reader
            // and no JIT kernel. The same program on a `--no-default-features` release
            // runtime is 6.7 MB, which is smaller than the equivalent Go binary.
            //
            // The size was always a choice; there was simply no way to make it.
            "--runtime" => match args.get(i + 1) {
                Some(p) => {
                    runtime = Some(p.clone());
                    i += 2;
                }
                None => {
                    eprintln!(
                        "error: `--runtime` needs a path to a `helix` binary to embed the \
                         program into"
                    );
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
        match bundle::build(
            std::path::Path::new(&entry),
            out.as_deref().map(std::path::Path::new),
            runtime.as_deref().map(std::path::Path::new),
        ) {
            Ok(built) => {
                let opts = render::RenderOpts::auto();
                let headline = format!(
                    "built standalone executable: {} ({})",
                    built.path.display(),
                    report::bytes(built.bytes)
                );
                let mut r = report::Report::new("build", headline)
                    .field("program", entry.clone())
                    .field(
                        "modules",
                        if built.modules == 1 {
                            "1".to_string()
                        } else {
                            format!("{} archived", built.modules)
                        },
                    )
                    .field(
                        "runtime",
                        built.runtime.clone().unwrap_or_else(|| {
                            format!("helix {} (this interpreter)", env!("CARGO_PKG_VERSION"))
                        }),
                    );
                // A 2.3 GB ARTIFACT NEEDS AN EXPLANATION, NOT JUST A NUMBER.
                //
                // `helix build` embeds the binary that invoked it, so running it from a
                // debug build ships an unstripped, un-LTO'd interpreter with full debug
                // info — about 2.3 GB, of which the program is a couple of hundred bytes.
                // A field report already shipped a 120 MB web server this way, from the
                // gate build; debug is twenty times worse again.
                //
                // Said only when it is CERTAIN: with no `--runtime` the runtime is this
                // binary, so `debug_assertions` is exact. A size threshold would be a
                // heuristic, and a heuristic that cries wolf on a large release build is
                // worse than silence.
                if built.debug_runtime {
                    r = r.note(
                        "warning",
                        "this artifact embeds a DEBUG interpreter",
                        "almost all of that size is debug info, not your program. Build the \
                         runtime with `cargo build --release` and pass `--runtime`, or run \
                         `helix build` from a release binary.",
                    );
                }
                r = r.gap();
                // A ceiling that is enforced but invisible invites the question "did that
                // actually get in?" every time someone ships. Say it once, here.
                r = match &built.capabilities {
                    Some(c) => {
                        let mut g: Vec<&str> = Vec::new();
                        if c.fs_read() && c.fs_write() {
                            g.push("fs: all");
                        } else if c.fs_read() {
                            g.push("fs: read");
                        } else if c.fs_write() {
                            g.push("fs: write");
                        }
                        if c.net_on() {
                            g.push("net");
                        }
                        if c.process_on() {
                            g.push("process");
                        }
                        let shown = if g.is_empty() {
                            "nothing granted".to_string()
                        } else {
                            report::Report::list(&opts, &g)
                        };
                        r.note("allows", shown, "baked in from `[capabilities]`; the artifact enforces this with no environment variable")
                    }
                    None => r,
                };
                // WHICH RUNTIME DOES THIS PROGRAM NEED? `--runtime` made the size a
                // choice; without this it was not an INFORMED one -- the only way to find
                // out whether a program touches a DataFrame, a genomics reader or the
                // HTTP client was to build against a smaller runtime and see what failed
                // at run time, on someone else's machine.
                //
                // This does NOT pick a runtime. The build has one binary to copy and
                // cannot produce a smaller one; substituting a guess would be worse than
                // saying nothing.
                r = if built.features.is_empty() {
                    // NAME WHAT ELSE THAT FLAG DROPS. `--no-default-features` also
                    // removes `jit` and `mimalloc`, which change speed rather than
                    // answers -- so "would serve this program" is true and, left alone,
                    // reads as "costs nothing". Someone shipping a hot loop deserves to
                    // know before they find out from a benchmark.
                    r.note(
                        "needs",
                        "no optional feature",
                        format!(
                            "`--no-default-features` would serve this program; that also \
                             drops {}, which change speed, not answers",
                            registry::PERFORMANCE_FEATURES.join(" and ")
                        ),
                    )
                } else {
                    r.field("needs", report::Report::list(&opts, &built.features))
                };
                r.print(&opts);
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
        let mut loaded = match module::load(&entry_path) {
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
        ufcs::resolve_by_type(&mut loaded.stmts, &types);
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
    // The stack size is shared with shard workers (`serve::eval_stack_size`) so the
    // primary and its shards can never diverge on recursion depth: 128 MiB in release
    // (~6x headroom over MAX_CALL_DEPTH at the measured ~1 KiB/frame), 1 GiB in debug
    // (~25x fatter frames), `HELIX_STACK_MB` to override. Small sizes matter beyond
    // ulimits: a thread-stack reservation is committed memory under strict overcommit.
    // If the OS refuses the thread, fail with a clean error rather than aborting the
    // process (the previous `.expect` turned constrained memory into a crash for
    // every program).
    std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .stack_size(crate::serve::eval_stack_size())
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
         helix check <script>…    type-check without running (`--json` for tools)\n    \
         helix effects <script>   what each function reaches (authority + reproducibility)\n    \
         helix fmt <script>…      format (no options; `--check` reports instead of writing)\n    \
         helix build <script>     bundle a program + its runtime into one executable\n    \
         helix emit-hbc <script>  compile to a .hbc bytecode container (for ctype's hvm)\n    \
         helix repl               start an interactive session\n    \
         helix new <name>         create a helix.toml in the current directory\n    \
         helix add <name> ...     add a dependency (--path <dir> | --url <tarball>)\n    \
         helix sync               resolve dependencies and write helix.lock\n    \
         helix verify             check the project matches helix.lock (no build)\n    \
         helix test [path]        run *_test.helix files (`--engines` cross-checks all 3)\n    \
         helix doc [Type]         list a type's methods (Array/String/Dna/…) or `builtins`\n    \
         helix search <term>      find a capability by what it does (names, docs, notes)\n    \
         helix describe [what]    the API as JSON — a name, a Type, or everything\n    \
         helix jit-explain <s>    which numeric kernels the JIT compiled, and where\n    \
         helix version            show the version\n    \
         helix help               show this help\n\n\
         The default `helix` is a self-contained binary. A build with the `python`\n\
         feature adds CPython interop (see docs/python-interop.md).\n\n\
         `helix build` EMBEDS YOUR SOURCE beside the interpreter; it does not compile it\n\
         and does not obfuscate it, so `strings` recovers the program from the artifact.\n\
         That is what makes it one file to copy with nothing installed, and it is worth\n\
         knowing before shipping one to someone. `helix emit-hbc` is the compiling path.",
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

/// `helix run <script> [args…]` — the script's own command line is `argv` (ADR 0037 D1).
fn run_file_with_args(path: &str, argv: &[String]) -> ExitCode {
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
    let argv: Vec<String> = argv.to_vec();
    let tool = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tool".to_string());
    // The whole pipeline runs on the big stack (see `run_on_big_stack`) so the
    // front-end's AST recursion can't overflow before the depth guard fires.
    run_on_big_stack(move || match run_file_capture_args(Entry::File(std::path::Path::new(path)), &argv, &tool) {
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
    run_file_capture_args(Entry::File(path), &[], "tool")
}

/// What the run pipeline was handed. See `run_file_capture_args`.
pub(crate) enum Entry<'a> {
    File(&'a std::path::Path),
    /// `(modules, entry index)`, as read from this executable's own overlay.
    Archive(Vec<(String, String)>, usize),
}

/// Run a program built into this executable.
///
/// A shard worker re-enters here rather than re-reading a file, because a bundled
/// program has no file to re-read.
pub(crate) fn run_archive_capture(modules: Vec<(String, String)>, entry: usize) -> Result<(), String> {
    run_file_capture_args(Entry::Archive(modules, entry), &[], "tool")
}

fn run_embedded(emb: bundle::Embedded) -> ExitCode {
    // THE ARTIFACT ENFORCES WHAT IT DECLARED. A bundled program has no manifest to read —
    // it may be the only file on the machine — so the ceiling travels inside it and is
    // installed here, before a single statement runs. Without this a `[capabilities]`
    // block governed `helix run` and evaporated at `helix build`, which is the worse half:
    // the artifact is the thing that reaches production.
    if let Some(caps) = emb.capabilities.clone() {
        capability::install_ceiling(caps);
    }
    // The program's own arguments, not `helix`'s: argv[0] is this artifact's name.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let tool = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "tool".to_string());
    serve::set_rerun(serve::Rerun::Archive(emb.modules.clone(), emb.entry));
    match run_file_capture_args(Entry::Archive(emb.modules, emb.entry), &argv, &tool) {
        Ok(()) => ExitCode::SUCCESS,
        Err(rendered) => {
            eprint!("{rendered}");
            ExitCode::FAILURE
        }
    }
}

/// The run pipeline, with the script's own arguments bound to its `fn main` (ADR 0037).
///
/// Binding is a DESUGAR: argv becomes literal expressions and a `main(…)` call is appended
/// to the statement list, so the type checker validates the call like any other and all
/// three engines run identical code. No new evaluator path means no new axis to keep in
/// agreement — the failure this project keeps paying for elsewhere.
fn run_file_capture_args(
    entry: Entry<'_>,
    argv: &[String],
    tool: &str,
) -> Result<(), String> {
    // The module loader reads, lexes, parses, and namespaces the entry file plus
    // everything it imports into one statement list. (A single file passes through
    // unchanged.) Lex/parse/resolve errors come back already rendered.
    // A file on disk and a built program's archive go through the SAME pipeline from
    // here: the same loader, the same `fn main` binding, the same `--help` answered from
    // the declaration, the same `run_program`. A bundled program with its own front end
    // would be free to drift from the one every test exercises.
    let file_path = match &entry {
        Entry::File(p) => Some(p.to_path_buf()),
        Entry::Archive(..) => None,
    };
    let (loaded, archive_src) = match entry {
        Entry::File(p) => (module::load(p)?, None),
        Entry::Archive(modules, i) => {
            let src = modules[i].1.clone();
            (module::load_archive(modules, i).map_err(|d| d.rendered)?, Some(src))
        }
    };
    let entry_prefix = loaded.entry_prefix.clone();
    let mut stmts = loaded.stmts;
    // The borrow of `stmts` ends with this match, so the call it produces can be pushed
    // afterwards. (`drop(sig)` to release it early is what clippy's `drop_non_drop`
    // catches, correctly: `MainSig` has no destructor and dropping it means nothing.)
    let appended = match climain::find(&stmts, entry_prefix.as_deref()) {
        Some(sig) => {
            // `--help` is answered from the DECLARATION, without running anything. A
            // script's top level is its program, so running it to print help would run
            // the tool — which is exactly what someone asking for help has not asked for.
            if argv.iter().any(|a| a == "--help" || a == "-h") {
                let src = match (&archive_src, &file_path) {
                    (Some(s), _) => s.clone(),
                    (None, Some(p)) => std::fs::read_to_string(p).unwrap_or_default(),
                    (None, None) => String::new(),
                };
                print!("{}", climain::help(&sig, &src, tool));
                return Ok(());
            }

            let args = climain::bind(&sig, argv)
                .map_err(|e| render_err(e, &loaded.spans, loaded.multi_module))?;
            Some(climain::call(sig.name, args, sig.line, sig.col))
        }
        // No `fn main`: arguments are REFUSED rather than discarded. Silently ignoring
        // them is what this whole change exists to end, and a program that cannot accept
        // an argument must say so instead of appearing to have accepted it.
        None if !argv.is_empty() => {
            return Err(format!(
                "error: this program takes no arguments, but {} {} given\n\
                 help: declare `fn main(…)` to accept a command line; its parameters \
                 become the arguments (ADR 0037).\n",
                argv.len(),
                if argv.len() == 1 { "was" } else { "were" }
            ));
        }
        None => None,
    };
    if let Some(call) = appended {
        stmts.push(call);
    }
    // Errors render against the spans the loader produced, so a cross-module error
    // points at the dependency's own source and line (not the entry file).
    run_program(&mut stmts, &loaded.spans, loaded.multi_module)
}

/// Load, namespace-resolve and type-check a file WITHOUT running it, returning the
/// rendered error. Must be called on the big stack.
///
/// This is deliberately [`run_file_capture`] with the execution removed: the same
/// loader, the same `types::check`, the same `render_err`. Writing a second front end
/// for `helix check` would let the two drift, and a checker that disagrees with the
/// runtime is worse than no checker.
/// One diagnostic as JSON: the STRUCTURE to act on and the rendered prose to show.
///
/// Both, deliberately. The structure is what a tool needs — a line, a column, a hint it
/// can surface next to the code. The prose is what actually repairs the mistake: a
/// 14-case sweep of what agents get wrong found eleven diagnostics whose help NAMES the
/// fix (`to_json(x)` answers "`to_json` is a method: `x.to_json()`"), so a machine
/// format that dropped it in favour of a code would be a downgrade dressed as an
/// upgrade. `rendered` is byte-identical to what the human output prints.
///
/// `line`/`col`/`message`/`hint` are absent when the failure has no position — a
/// missing file, an import cycle — rather than being faked with zeroes.
fn diag_json(severity: &str, file: &str, d: &module::Diag) -> serde_json::Value {
    let mut v = serde_json::json!({
        "severity": severity,
        "file": d.filename.clone().unwrap_or_else(|| file.to_string()),
        "rendered": d.rendered,
    });
    if let Some(e) = &d.err {
        v["message"] = serde_json::Value::String(e.message.clone());
        v["line"] = serde_json::json!(e.line);
        v["col"] = serde_json::json!(e.col);
        if let Some(h) = &e.hint {
            v["hint"] = serde_json::Value::String(h.clone());
        }
    }
    v
}

/// `check_file_capture`'s structured twin: the same two phases, keeping the diagnostic
/// whole instead of rendering it. The rendering is identical because `Diag` carries the
/// rendered text produced by the very same call.
fn check_file_structured(path: &std::path::Path) -> Result<module::Loaded, module::Diag> {
    let loaded = module::load_diag(path)?;
    if let Some(e) = climain_violation(&loaded.stmts, loaded.entry_prefix.as_deref()) {
        let (src, filename, local) = module::locate(&loaded.spans, e.line);
        let mut e = e;
        e.line = local;
        let rendered = e.render(src, filename);
        return Err(module::Diag { rendered, filename: Some(filename.to_string()), err: Some(e) });
    }
    // The loaded graph is handed back so `--lint` can walk the imports (`lint_units`);
    // the combinator form could not, because the error arm borrows `loaded` while the ok
    // arm has to move it.
    if let Err(e) = types::check(&loaded.stmts) {
        // The checker reports a GLOBAL line across concatenated modules; map it back to
        // the file and local line a reader can open, exactly as `render_err` does.
        let (src, filename, local_line) = module::locate(&loaded.spans, e.line);
        let mut e = e;
        e.line = local_line;
        let rendered = if loaded.multi_module {
            // The multi-module rewrite prefixes every imported name with `m<N>$` so two
            // modules can define `double`. That prefix is an implementation detail, and
            // the human output has always stripped it — but the STRUCTURED fields are a
            // second copy of the same text, and stripping only the rendered half would
            // hand a tool `m0$double` while showing the reader `double`. Two spellings of
            // one name in one document is exactly the kind of drift this project treats
            // as a bug; caught by asking the JSON what it said about an imported symbol.
            e.message = strip_mangling(&e.message);
            e.hint = e.hint.as_deref().map(strip_mangling);
            strip_mangling(&e.render(src, filename))
        } else {
            e.render(src, filename)
        };
        return Err(module::Diag { rendered, filename: Some(filename.to_string()), err: Some(e) });
    }
    Ok(loaded)
}

fn check_file_capture(path: &std::path::Path) -> Result<module::Loaded, String> {
    let loaded = module::load(path)?;
    if let Some(e) = climain_violation(&loaded.stmts, loaded.entry_prefix.as_deref()) {
        return Err(render_err(e, &loaded.spans, loaded.multi_module));
    }
    if let Err(e) = types::check(&loaded.stmts) {
        return Err(render_err(e, &loaded.spans, loaded.multi_module));
    }
    // Handed back for `--lint`'s import traversal — see `lint_units`.
    Ok(loaded)
}

/// The (display name, source) of every file `--lint` should examine for one entry: the
/// entry itself and every module it transitively imports, in dependency order.
///
/// WHY THE IMPORTS. `--lint` read only the file it was handed. In a project whose entry
/// point imports a library — which is every project with a library — the library was
/// never linted by any command that exists, and `helix check --lint app.helix` printing
/// `ok` read as "the project is clean" while saying nothing about most of it. A field
/// report measured the cost: an O(n^2) accumulation lived in an imported training loop
/// for a whole release cycle, found only by copying the tree and linting the copy file
/// by file. The loader already holds every module's source — it must, to render an error
/// against the right file — so this traversal was there for the taking.
///
/// THE ENTRY KEEPS THE NAME THE USER TYPED. Inside the loader a module's filename is the
/// canonicalized absolute path, but a lint note is read by a human looking for a file to
/// open, and `app.helix` has always been what this printed. Imported modules are shown
/// relative to the working directory when they are under it, absolute when they are not
/// (the stdlib, a `HELIX_PATH` root), which is the honest answer in both cases.
///
/// `seen` deduplicates across the whole invocation, keyed by the canonical path rather
/// than the display name: `helix check --lint a.helix b.helix` where both import
/// `lib.helix` reports that library's notes once, not twice.
fn lint_units(
    loaded: &module::Loaded,
    shown: &str,
    seen: &mut std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let cwd = std::env::current_dir().ok();
    // The loader emits spans in post-order with the entry LAST (see `load_diag`), which
    // is also the order a reader wants: a library's notes before the file that imports it.
    let entry = loaded.spans.len().saturating_sub(1);
    let mut out = Vec::new();
    for (i, sp) in loaded.spans.iter().enumerate() {
        if !seen.insert(sp.filename.clone()) {
            continue;
        }
        let name = if i == entry {
            shown.to_string()
        } else {
            match &cwd {
                Some(c) => std::path::Path::new(&sp.filename)
                    .strip_prefix(c)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| sp.filename.clone()),
                None => sp.filename.clone(),
            }
        };
        out.push((name, sp.source.clone()));
    }
    out
}

/// ADR 0037 D6: a `fn main` parameter that cannot be built from a command-line string is
/// refused AHEAD OF RUNNING, by `helix check` and by the run path alike.
///
/// The alternative is a tool that builds, installs, ships, and fails on its first real
/// invocation — the argument is the same one that puts `helix check` in front of every
/// run in the first place.
fn climain_violation(
    stmts: &[ast::Stmt],
    entry_prefix: Option<&str>,
) -> Option<crate::error::HelixError> {
    let sig = climain::find(stmts, entry_prefix)?;
    let bad = climain::unbindable_param(&sig)?;
    Some(
        crate::error::HelixError::new(
            format!("`main`'s parameter `{bad}` cannot be built from a command-line argument"),
            sig.line,
            sig.col,
        )
        .hint(
            "a command line carries text: `main` takes `Int`, `Float`, `String` or `Bool`. \
             Take a path and read the data inside `main`."
                .to_string(),
        ),
    )
}

/// `helix test [path]` — discover and run test files (any file named `*_test.helix`
/// under `path`, default the current directory), each in isolation through the normal
/// pipeline. A file passes if it runs to completion without raising — `assert`,
/// `assert_eq`, and `assert_close` raise on failure. Reports per-file results and a
/// summary, exiting non-zero if any file failed. The built-in test runner: no framework
/// to install, no config — name a file `*_test.helix` and it runs.
fn cli_test(args: &[String]) -> ExitCode {
    use std::path::PathBuf;
    // EVERY path argument is a root — `helix test a.helix b.helix` runs both, exactly
    // as `helix check a b` checks both. It used to take args.get(2) alone and silently
    // drop the rest while printing "running 1 test file": anyone verifying two modules
    // in one command believed both passed when only the first ran — the worst shape a
    // test runner can have, found in the field by the physics-library build.
    // `--json`: machine-readable results (one JSON document on stdout) —
    // agents were scraping `"N passed"` with a regex, which is fragile and
    // loses per-example detail (field review §3.5).
    let json = args[2..].iter().any(|a| a == "--json");
    // `--engines`: after a file passes, run it again under the bytecode VM and the
    // tree-walker and require byte-identical output. Opt-in because it costs two extra
    // child processes per file; worth it in CI, where a divergence is the most expensive
    // class of bug this language can have.
    let engines = args[2..].iter().any(|a| a == "--engines");
    const FLAGS: [&str; 2] = ["--json", "--engines"];
    if let Some(bad) =
        args[2..].iter().find(|a| a.starts_with('-') && !FLAGS.contains(&a.as_str()))
    {
        eprintln!(
            "error: unknown option `{bad}` for `helix test` (the flags are `--json` and `--engines`)"
        );
        return ExitCode::FAILURE;
    }
    let explicit: Vec<PathBuf> =
        args[2..].iter().filter(|a| !FLAGS.contains(&a.as_str())).map(PathBuf::from).collect();
    let roots: Vec<PathBuf> = if explicit.is_empty() {
        vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
    } else {
        explicit.clone()
    };
    let named_explicitly = !explicit.is_empty();
    run_on_big_stack(move || {
        // A path the user named explicitly but which doesn't exist is an error, not
        // an empty (successful) run — otherwise a typo'd path silently "passes".
        if named_explicitly
            && let Some(missing) = roots.iter().find(|r| !r.exists())
        {
            eprintln!("error: no such file or directory: {}", missing.display());
            if json {
                // A stdout-only JSON consumer still gets a document, never
                // empty input (sweep lens-3 observation).
                let doc = serde_json::json!({
                    "helix_version": env!("CARGO_PKG_VERSION"),
                    "error": format!("no such file or directory: {}", missing.display()),
                    "passed": 0, "failed": 0, "events": [],
                });
                if let Ok(s) = serde_json::to_string_pretty(&doc) {
                    println!("{s}");
                }
            }
            return ExitCode::FAILURE;
        }
        let mut files = Vec::new();
        let mut skipped_dirs = Vec::new();
        for root in &roots {
            collect_root(root, &mut files, &mut skipped_dirs);
        }
        dedup_by_canonical(&mut files);
        // Say what was NOT walked. A test runner that quietly narrows its own scope is
        // the failure this project already paid for once, so the skip is never silent —
        // and the note names the way to override it.
        if !skipped_dirs.is_empty() && !json {
            skipped_dirs.sort();
            skipped_dirs.dedup();
            eprintln!(
                "note: did not descend into {} — name one explicitly to run tests inside it",
                skipped_dirs.join(", ")
            );
        }
        // Display paths relative to the search root when there is ONE root (its parent
        // when that root is a single file, so the file's own name still shows); with
        // several roots, paths display as given — the empty-prefix strip below is a
        // deliberate no-op then.
        let base: PathBuf = if let [only] = roots.as_slice() {
            if only.is_file() {
                only.parent().unwrap_or(only).to_path_buf()
            } else {
                only.clone()
            }
        } else {
            PathBuf::new()
        };
        let root_shown = roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        run_test_roots(roots, files, base, root_shown, json, engines)
    })
}

/// Collect the test files one root contributes: a directory's `*_test.helix` set, or
/// the named file itself — except a documented, definitions-only module, which tests
/// through its doc examples exactly as its directory run would.
fn collect_root(
    root: &std::path::Path,
    files: &mut Vec<std::path::PathBuf>,
    skipped: &mut Vec<String>,
) {
    {
        if root.is_file() {
            // Naming a file must mean what naming its directory means. A definitions-only
            // module carrying `## >>>` examples is a DOC MODULE: the directory run tests
            // it through its examples and never demands assertions, so the file run must
            // not either — before this, the same command that PASSED a module's two
            // examples also FAILed it for asserting nothing, in one output, and an agent
            // narrowing from directory to file to iterate faster was punished for it.
            // A `*_test.helix` named directly keeps the assert-or-fail contract, exactly
            // as the directory run applies it to collected test files.
            let is_test_file = root
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_test.helix"));
            let doc_module = !is_test_file
                && std::fs::read_to_string(root).is_ok_and(|src| {
                    !doctest::doc_examples_in(&src).is_empty() && is_definitions_only(&src)
                });
            if !doc_module {
                files.push(root.to_path_buf());
            }
        } else {
            skipped.extend(collect_test_files(root, files));
        }
        files.sort();
    }
}

/// The run half of `cli_test`, over an already-collected file set and its roots.
fn run_test_roots(
    roots: Vec<std::path::PathBuf>,
    files: Vec<std::path::PathBuf>,
    base: std::path::PathBuf,
    root_shown: String,
    json: bool,
    engines: bool,
) -> ExitCode {
    {
        let base = base.as_path();
        // In JSON mode every print below is replaced by an event here; the one
        // final document carries version, totals, and the events in run order.
        let mut ev: Vec<serde_json::Value> = Vec::new();
        // Doc examples count as tests, so "nothing to run" must account for them — a
        // library of documented modules with no `*_test.helix` beside them is a real
        // project shape, and reporting "no tests found" over a dozen live examples would
        // be the same lie this pass is removing everywhere else.
        // Provenance first, on EVERY path: the field ran a full round of results
        // against a stale binary on PATH and only caught it a session later. One
        // line prevents it.
        if !json {
            println!("helix {}", env!("CARGO_PKG_VERSION"));
        }
        if files.is_empty() && !roots.iter().any(|r| any_doc_examples(r)) {
            if json {
                emit_test_json(0, 0, 0, ev);
            } else {
                println!(
                    "no tests found (looked for `*_test.helix`, and for `>>>` doc examples, under {root_shown})"
                );
            }
            return ExitCode::SUCCESS;
        }
        if !json && !files.is_empty() {
            println!("running {} test file{}", files.len(), plural(files.len()));
        }
        let mut failed = 0usize;
        for f in &files {
            let shown = f.strip_prefix(base).unwrap_or(f).display();
            // Per file, so one file's assertions can't vouch for the next one's.
            crate::interp::ASSERTIONS_RUN.store(0, std::sync::atomic::Ordering::Relaxed);
            // Capture the file's own prints: in --json they become the event's
            // `output` field (a test that prints used to corrupt the one-JSON-
            // document contract — the sweep's finding); in prose they indent
            // under the result line instead of interleaving above it.
            crate::interp::capture_begin();
            let file_result = run_file_capture(f);
            let file_output = crate::interp::capture_take();
            match file_result {
                Ok(())
                    if crate::interp::ASSERTIONS_RUN
                        .load(std::sync::atomic::Ordering::Relaxed)
                        == 0 =>
                {
                    // Ran clean and checked nothing. Reporting `ok` here is how a whole
                    // suite of `fn test_*` definitions that nobody calls reads as green.
                    failed += 1;
                    let looks_like_fn_tests =
                        std::fs::read_to_string(f).map(|s| s.contains("fn test_")).unwrap_or(false);
                    if json {
                        let mut e = serde_json::json!({
                            "kind": "file", "file": shown.to_string(), "status": "fail",
                            "detail": "ran to completion without asserting anything",
                        });
                        if !file_output.is_empty() {
                            e["output"] = serde_json::Value::String(file_output.clone());
                        }
                        ev.push(e);
                    } else {
                        println!("  FAIL  {shown}");
                        println!("        this file ran to completion without asserting anything");
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
                }
                Ok(()) => {
                    // A test that passes on one engine has proved less than it looks
                    // like it has. Only a file that already PASSED is cross-checked:
                    // a failing test disagrees with itself before it can disagree
                    // across engines, and re-running it three times would bury the
                    // real failure under a diff.
                    let divergence = if engines { engine_divergence(f) } else { None };
                    match divergence {
                        Some(report) => {
                            failed += 1;
                            if json {
                                let mut e = serde_json::json!({
                                    "kind": "file", "file": shown.to_string(),
                                    "status": "fail", "detail": report,
                                    "engine_divergence": true,
                                });
                                if !file_output.is_empty() {
                                    e["output"] = serde_json::Value::String(file_output.clone());
                                }
                                ev.push(e);
                            } else {
                                println!("  FAIL  {shown}");
                                for line in report.lines() {
                                    println!("        {line}");
                                }
                            }
                        }
                        None => {
                            if json {
                                let mut e = serde_json::json!({
                                    "kind": "file", "file": shown.to_string(), "status": "ok",
                                });
                                if engines {
                                    e["engines_agree"] = serde_json::Value::Bool(true);
                                }
                                if !file_output.is_empty() {
                                    e["output"] = serde_json::Value::String(file_output.clone());
                                }
                                ev.push(e);
                            } else if engines {
                                println!("  ok    {shown}   (3 engines agree)");
                            } else {
                                println!("  ok    {shown}");
                            }
                        }
                    }
                }
                Err(rendered) => {
                    failed += 1;
                    if json {
                        let mut e = serde_json::json!({
                            "kind": "file", "file": shown.to_string(), "status": "fail",
                            "detail": rendered,
                        });
                        if !file_output.is_empty() {
                            e["output"] = serde_json::Value::String(file_output.clone());
                        }
                        ev.push(e);
                    } else {
                        println!("  FAIL  {shown}");
                        // Indent the rendered error so it reads as detail under the failure.
                        for line in rendered.lines() {
                            println!("        {line}");
                        }
                    }
                }
            }
            if !json && !file_output.is_empty() {
                for line in file_output.lines() {
                    println!("        | {line}");
                }
            }
        }
        // Then the documented examples. `docs/comments-and-docs.md` promises that a
        // documented example is executed and must still say what it says — a promise that
        // until now held only for Helix's OWN source, checked by a `cargo test` a user of
        // the language cannot run. For a library author it was decoration.
        // Collect ACROSS roots then dedup, so overlapping roots (a directory plus a
        // file inside it, the same directory twice) count each example once — the
        // per-root loop this replaces re-ran them, and only the file list deduped.
        let mut doc_sources = Vec::new();
        for root in &roots {
            if root.is_file() {
                doc_sources.push(root.clone());
            } else {
                collect_helix_files(root, &mut doc_sources);
            }
        }
        dedup_by_canonical(&mut doc_sources);
        let (doc_ok, doc_failed, skipped) = run_doc_examples(doc_sources, base, json, &mut ev);
        // A doc example is a test: it counts in the same totals, so the summary never
        // reads `0 passed` over a screen of green.
        let passed = (files.len() - failed) + doc_ok;
        let failed = failed + doc_failed;
        if json {
            emit_test_json(passed, failed, skipped, ev);
            return if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
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
        if failed == 0 {
            println!("{passed} passed");
            ExitCode::SUCCESS
        } else {
            println!("{passed} passed, {failed} failed");
            ExitCode::FAILURE
        }
    }
}

/// The one `--json` document: totals plus events in run order. Exit codes are
/// identical to the prose mode's — the document is a different VIEW, never a
/// different verdict.
fn emit_test_json(passed: usize, failed: usize, doc_files_skipped: usize, ev: Vec<serde_json::Value>) {
    let doc = serde_json::json!({
        "helix_version": env!("CARGO_PKG_VERSION"),
        "passed": passed,
        "failed": failed,
        "doc_files_skipped": doc_files_skipped,
        "events": ev,
    });
    match serde_json::to_string_pretty(&doc) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: could not serialize test results: {e}"),
    }
}

/// `helix check --lint` — the traps the field corpus actually hit, as advisory
/// notes with the fix in the message (field review §3.6). Textual and
/// deliberately narrow: each pattern below has essentially no false-positive
/// reading, and a lint that cries wolf trains people to ignore lints. Never an
/// error, never an exit-code change.
fn lint_source(shown: &str, src: &str) -> Vec<String> {
    use crate::ast::{BinOp, Expr};
    let mut notes: Vec<(usize, String)> = Vec::new();
    // Links of a `let … in let … in` chain already claimed by the note on its head, so one
    // long head produces one note rather than one per level.
    let mut seen_chain: std::collections::HashSet<*const Expr> = std::collections::HashSet::new();
    // The AST lints walk the real tree (src/visit.rs) — no textual guessing.
    // A parse failure returns no notes: `check` already reported it properly.
    if let Ok(toks) = crate::lexer::lex(src)
        && let Ok(stmts) = crate::parser::parse(toks)
    {
        for s in &stmts {
            crate::visit::walk_stmt(s, &mut |e| {
                match e {
                    // `xs.reduce(dict(), …)` — the fold that stands in for
                    // `to_dict()`, with the last-wins rule attached so a
                    // mechanical migration cannot invert a first-wins fold.
                    // A DICT ACCUMULATED INSIDE A RECORD, which ADR 0029's
                    // amortized-linear guarantee still does not reach — and which is the
                    // shape AGENTS.md teaches, because `mut` is top-level only and a fold
                    // carrying two values has to carry them in a record.
                    //
                    // THE ARRAY CASE USED TO BE HERE TOO AND IS NOW A FIX RATHER THAN A
                    // DIAGNOSTIC. `ArrayData::Shared` gives `concat` an append-only buffer,
                    // so an array in a record field is linear: measured 2.2x and 3.3x per
                    // 4x the input where it was 9.3x and 27.9x, and 36 ms where it was
                    // 2,591 ms at n=160,000. Keeping the note would be a checker
                    // contradicting the runtime, which trains people to ignore it.
                    //
                    // `Dict::insert` has no such buffer: it clones the whole `BTreeMap` per
                    // call, and `Op::InsertIntoLocal` only rescues the bare-local spelling
                    // (reading `a.d` clones the `Rc` while the record still holds one, so
                    // `Rc::get_mut` always fails). Measured per 4x the input: 15.4x then
                    // 16.6x, and **71 seconds** at n=128,000 against 4 ms for the array. It
                    // is now the worst remaining cliff of this family, so the diagnostic
                    // ADR 0026 requires stays until that fix lands.
                    Expr::Method { name, args, line, .. }
                        if name == "reduce"
                            && matches!(args.first(), Some(Expr::Record(fs))
                                if fs.iter().any(|(_, v)| matches!(v, Expr::Call { name: n, args: a, .. } if n == "dict" && a.is_empty()))) =>
                    {
                        notes.push((*line, format!(
                            "{shown}:{line}: this fold accumulates a DICT inside a record, \
                             which ADR 0029's amortized-linear guarantee does not reach — \
                             `insert` clones the whole map per step, and the take-append-store \
                             that rescues `acc = acc.insert(…)` only fires when the \
                             accumulator IS the local, so through a field the fold is O(n^2). \
                             Measured per 4x the input: 16.6x, and 71 s at n=128,000. (An \
                             ARRAY in a record field is linear as of 0.7.1 — this is the dict \
                             half only.) Fine for tens or hundreds of steps; for thousands, \
                             fold the dict on its own and carry the rest beside it."
                        )));
                    }
                    Expr::Method { name, args, line, .. } if name == "reduce" => {
                        if matches!(args.first(),
                            Some(Expr::Call { name: n, args: a, .. }) if n == "dict" && a.is_empty())
                        {
                            notes.push((*line, format!(
                                "{shown}:{line}: `reduce(dict(), …)` — `to_dict()` builds a \
                                 Dict from pairs. NOTE: to_dict is LAST-wins on duplicate \
                                 keys; a first-wins fold keeps its reduce."
                            )));
                        }
                    }
                    // `0 - x` / `0.0 - x` — the pre-autodiff-unary-minus idiom.
                    Expr::Binary { op: BinOp::Sub, left, line, .. }
                        if matches!(&**left, Expr::Int(0)) || matches!(&**left, Expr::Float(f) if *f == 0.0) =>
                    {
                        notes.push((*line, format!(
                            "{shown}:{line}: `0 - x` — unary minus (`-x`) works everywhere \
                             now, tracked values included."
                        )));
                    }
                    // A `let` head past ~6 bindings: `do {{ }}` (or a `where`
                    // clause on the fn) reads better than a long preamble.
                    //
                    // NOT a `do` block, which is the form this advises — the two lower to
                    // the same node, and without `from_do` the rule fired on blocks that
                    // already were `do { … }` and told them to become one.
                    //
                    // The CHAIN is counted, not one node of it: `let a = … in let b = … in`
                    // is the same long head written as a nest, and it was invisible to a
                    // rule that looked at a single `Let`. `seen_chain` keeps the note on
                    // the head — the walk is pre-order, so the outermost link is reached
                    // first and claims the ones below it.
                    Expr::Let { bindings, body, from_do: false }
                        if !seen_chain.contains(&(e as *const Expr)) =>
                    {
                        let mut n = bindings.len();
                        let mut cur = body.as_ref();
                        while let Expr::Let { bindings: bs, body: b2, from_do: false } = cur {
                            seen_chain.insert(cur as *const Expr);
                            n += bs.len();
                            cur = b2.as_ref();
                        }
                        if n > 6 {
                            let at = bindings
                                .iter()
                                .find_map(|(_, v)| crate::visit::expr_pos(v))
                                .or_else(|| crate::visit::expr_pos(body))
                                .map(|(l, _)| l)
                                .unwrap_or(1);
                            notes.push((at, format!(
                                "{shown}:{at}: a `let` head with {n} bindings — `do {{ … }}` \
                                 (sequential bindings) or a `where` clause reads better past ~6."
                            )));
                        }
                    }
                    _ => {}
                }
            });
        }
    }
    // The doc-example lint is TEXTUAL on purpose: comments exist only in
    // source, so text is the correct representation, not a workaround.
    let lines: Vec<&str> = src.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if let Some(rest) = t.strip_prefix("export fn ") {
            let mut j = i;
            let mut has_example = false;
            // Any comment line continues the doc block — the doctest extractor
            // tolerates a plain `#` between the example and the fn, so the
            // lint must too (the sweep caught it contradicting `helix test`).
            //
            // BUT THE EXAMPLE ITSELF HAS TO BE ON A `##` LINE, because that is the only
            // kind `doctest::doc_examples_in` reads. Counting a `>>>` on a plain `#` line
            // made this rule satisfiable by an example that never runs — the exact
            // failure it exists to prevent, and invisible, because the lint went green.
            // Tolerating `#` BETWEEN the example and the `fn` is a different thing and
            // stays: that is prose, and the extractor skips it too.
            while j > 0 && lines[j - 1].trim_start().starts_with('#') {
                if lines[j - 1].trim_start().starts_with("##")
                    && lines[j - 1].contains(">>>")
                {
                    has_example = true;
                }
                j -= 1;
            }
            if !has_example {
                let name = rest.split(['(', ' ']).next().unwrap_or("?");
                notes.push((i + 1, format!(
                    "{shown}:{}: `export fn {name}` has no `>>>` doc example — the house \
                     standard is executable docs. The example must be on a `##` line: \
                     `helix test` reads only those, so a `>>>` under a plain `#` is a \
                     comment nothing runs.",
                    i + 1
                )));
            }
        }
    }
    // Source order, numerically — a lexicographic sort put :10 before :1.
    notes.sort_by_key(|(l, _)| *l);
    notes.into_iter().map(|(_, n)| n).collect()
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
fn run_doc_examples(
    mut sources: Vec<std::path::PathBuf>,
    base: &std::path::Path,
    json: bool,
    ev: &mut Vec<serde_json::Value>,
) -> (usize, usize, usize) {
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
                if json {
                    ev.push(serde_json::json!({
                        "kind": "doc", "file": shown, "line": ex.line, "status": "fail",
                        "code": ex.code.join(" ; "),
                        "detail": format!("engines disagree: {} vs {}", results[0].0, e),
                    }));
                } else {
                    println!("  FAIL  {where_} (doc)");
                    println!("        engines disagree: {} vs {}", results[0].0, e);
                    println!("        {}: {}", results[0].0, results[0].1);
                    println!("        {}: {}", e, results.iter().find(|(n, _)| n == e).unwrap().1);
                }
                continue;
            }
            let got = results[0].1.trim_end();
            let want = ex.expect.join("\n");
            // TRAILING WHITESPACE IS OFF BOTH SIDES, PER LINE. An expectation is written
            // in a `##` comment, where trailing spaces are invisible and get stripped by
            // every formatter, so it cannot carry them; comparing them made any padded
            // line — a fixed-width column, which is what this language's own report and
            // log functions emit — impossible to expect. Only the last line used to be
            // reached, because the trim was on the whole string.
            //
            // Leading whitespace stays significant: indentation is structure, and
            // `doc_examples_in` already strips exactly the `>>>` line's indent so nested
            // output keeps its shape.
            let norm = |t: &str| {
                t.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
            };
            if !ex.expect.is_empty() && norm(got) != norm(want.trim_end()) {
                failed += 1;
                if json {
                    ev.push(serde_json::json!({
                        "kind": "doc", "file": shown, "line": ex.line, "status": "fail",
                        "code": ex.code.join(" ; "),
                        "expected": want.trim_end(),
                        "got": got,
                    }));
                } else {
                    println!("  FAIL  {where_} (doc)");
                    println!("        code:     {}", ex.code.join(" ; "));
                    // Multi-line values keep the detail indent on every line.
                    let indented = |label: &str, text: &str| {
                        for (i, l) in text.lines().enumerate() {
                            if i == 0 {
                                println!("        {label} {l}");
                            } else {
                                println!("        {:width$} {l}", "", width = label.len());
                            }
                        }
                    };
                    indented("expected:", want.trim_end());
                    indented("got:     ", got);
                    if ex.expect.iter().any(|l| l.trim_start().starts_with("...")) {
                        println!(
                            "        note: a doc example is ONE line — there is no `...` \
                             continuation. Write the call on a single line."
                        );
                    }
                }
            } else {
                ok += 1;
                if json {
                    ev.push(serde_json::json!({
                        "kind": "doc", "file": shown, "line": ex.line, "status": "ok",
                        "code": ex.code.join(" ; "),
                    }));
                } else {
                    // The engine count is stated because it is not the default anyone
                    // would assume: a bare `ok` beside a suite's `(3 engines agree)` reads
                    // as "this one ran on one engine", which is how a field build came to
                    // file a gap that was never there. `--engines` does not gate this —
                    // a doc example has no cheap single-engine mode worth having.
                    println!("  ok    {where_} (doc, 3 engines agree)");
                }
            }
        }
    }
    (ok, failed, skipped)
}

/// Run `path` under every engine and report the first disagreement, or `None`.
///
/// **This is the capability no other test runner can offer**, because no other language
/// ships three implementations of itself that must agree byte-for-byte. `pytest`,
/// `jest` and `cargo test` can each tell you a test passed; none can tell you it passes
/// *the same way* under three independent evaluators. Helix's whole correctness story is
/// that agreement (`docs/execution-engine.md`), and until now only the compiler's own
/// suite could reach it — a user's tests ran on one engine and the axis was invisible to
/// them, which is exactly the shape ADR 0036 spent a release paying for on the DataFrame
/// backends.
///
/// Each engine runs in a CHILD process. Running three times in-process would share the
/// JIT, the memo tables, the module line map and the capability authority, so the second
/// run would not be a clean one — and a differential oracle that contaminates its own
/// control column proves nothing.
fn engine_divergence(path: &std::path::Path) -> Option<String> {
    let mut runs: Vec<(&str, std::process::Output)> = Vec::new();
    for (name, env) in TEST_ENGINES {
        let exe = std::env::current_exe().ok()?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("run").arg(path).stdin(std::process::Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        runs.push((name, cmd.output().ok()?));
    }
    let (base_name, base) = &runs[0];
    for (name, out) in &runs[1..] {
        // Exit status, stdout AND stderr: an engine that reaches the same value by a
        // different error text has still diverged, and error text is half of what this
        // language pins.
        let same = out.status.code() == base.status.code()
            && out.stdout == base.stdout
            && out.stderr == base.stderr;
        if !same {
            let show = |o: &std::process::Output| {
                format!(
                    "exit {:?}\n{}{}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            };
            // TWO causes, and this cannot tell them apart, so it must not pretend to.
            // An engine bug is the one worth reporting; a test that is not a pure
            // function of its input (reads a clock, a file it also writes, unseeded
            // randomness) disagrees with ITSELF and would look identical here. Naming
            // only the first was wrong the first time this fired — on a deliberately
            // non-deterministic test, where "this is a Helix bug" was a false accusation.
            return Some(format!(
                "the same file produced different output under different engines.\n\
                 Either an engine is wrong — a Helix bug, please report it — or this test \
                 is not a pure\nfunction of its input (a clock, a file it also writes, \
                 unseeded randomness).\nRun it twice on ONE engine to tell which.\n\
                 --- {base_name} ---\n{}\n--- {name} ---\n{}",
                show(base).trim_end(),
                show(out).trim_end()
            ));
        }
    }
    None
}

/// The engines a test can be cross-checked on, default first.
const TEST_ENGINES: [(&str, &[(&str, &str)]); 3] = [
    ("jit", &[]),
    ("vm", &[("HELIX_NOJIT", "1")]),
    ("walker", &[("HELIX_NOVM", "1")]),
];

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

/// Directories a build tool wrote, which a *discovered* walk must not descend into.
///
/// `helix test` in this very repository ran four failing `*_test.helix` files out of
/// `target/` — scratch left by earlier builds — and reported them among the results. The
/// same shape hits any project that also holds a `node_modules` or a `__pycache__`.
///
/// The list is deliberately SHORT, and the asymmetry is the reason: running an extra test
/// is visible noise, while skipping a real one is silence — and a suite that silently
/// does not run is the exact failure this project already paid for once (the `native-df`
/// campaign, 28 tests executed by nothing while the docs said otherwise). So only names
/// that are unambiguously machine-generated qualify. `dist`, `build` and `venv` are NOT
/// here: each is plausibly somebody's own directory, and a wrong skip hides their tests.
///
/// Two rules keep this from ever being a silent loss: a skipped directory is REPORTED,
/// and naming one explicitly (`helix test target/`) still runs it, because the check
/// applies when descending, never to the root the caller asked for.
fn is_generated_dir(name: &str) -> bool {
    matches!(name, "target" | "node_modules" | "__pycache__")
}

/// Recursively collect every `.helix` file under `dir`, skipping hidden directories.
fn collect_helix_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    collect_by_suffix(dir, ".helix", &mut std::collections::HashSet::new(), out, &mut Vec::new());
}

/// Recursively collect `*_test.helix` files under `dir`, skipping hidden directories
/// (`.git`, etc.). Unreadable directories are silently skipped. Returns the generated
/// directories that were not descended into, so the caller can say so.
fn collect_test_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> Vec<String> {
    let mut skipped = Vec::new();
    collect_by_suffix(dir, "_test.helix", &mut std::collections::HashSet::new(), out, &mut skipped);
    skipped
}

/// The shared walker behind the two collectors above. `seen` holds the canonical
/// path of every directory already entered, so a symlinked directory cycle
/// terminates: without it, one self-loop made `helix test` count the same test once
/// per traversal depth (41 times, bounded only by the OS path limit) and two loops
/// recursed forever — a test runner that lies about its count or never returns, from
/// an ordinary filesystem accident. A directory that cannot be canonicalized (racing
/// deletion, dangling symlink) is skipped like an unreadable one.
fn collect_by_suffix(
    dir: &std::path::Path,
    suffix: &str,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
    out: &mut Vec<std::path::PathBuf>,
    skipped: &mut Vec<String>,
) {
    let Ok(canon) = std::fs::canonicalize(dir) else { return };
    if !seen.insert(canon) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue; // hidden files/dirs (.git, .helix caches, …)
        }
        let path = entry.path();
        if path.is_dir() {
            // Applied only when DESCENDING: the root the caller named is already inside
            // the walk, so `helix test target/` still tests `target/`.
            if is_generated_dir(&name) {
                skipped.push(path.display().to_string());
                continue;
            }
            collect_by_suffix(&path, suffix, seen, out, skipped);
        } else if name.ends_with(suffix) {
            out.push(path);
        }
    }
}

/// Drop later duplicates of the same on-disk file, keyed by canonical path, keeping
/// first-seen order. Adjacent-only `dedup()` missed interleaved walks (`helix test
/// t1 t1` with two test files collects a,b,a,b) and spelling variants (`./t1` vs
/// `t1`); doc examples had no cross-root dedup at all, so a directory root plus a
/// file inside it re-ran and re-counted the same examples, inflating the pass total.
fn dedup_by_canonical(files: &mut Vec<std::path::PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    files.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
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
fn run_program(program: &mut [ast::Stmt], spans: &[module::Span], multi: bool) -> Result<(), String> {
    // Publish "which file is this line in?" for `source_path`, which a builtin cannot work
    // out for itself: it is dispatched by name and receives only its call position.
    module::set_file_lines(
        spans.iter().map(|s| (s.start_line, s.filename.clone())).collect(),
    );
    // The inferred receiver types feed the compiler so it can route
    // receiver-polymorphic methods (DataFrame/Tensor column-verbs) correctly.
    let types = types::check(program).map_err(|e| render_err(e, spans, multi))?;
    // The receiver decides where it is known (src/ufcs.rs); every engine below runs
    // the same rewritten program, the JIT included.
    ufcs::resolve_by_type(program, &types);

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
    // The banner carries the map. Bare `helix` is where a new user (or agent) lands
    // first, and the project's own history proves the cost of not pointing from here:
    // its heaviest user spent months believing `scan` didn't exist while
    // `helix doc Array` would have printed it. Line 1 stays exactly as it was
    // (external scrapers key on it); the three pointer lines are copied verbatim
    // from `print_help()` so the two surfaces cannot drift.
    println!(
        "Helix {} — interactive session. Type an expression and press Enter; Ctrl-D to exit.\n    \
         helix help               commands and usage\n    \
         helix doc [Type]         list a type's methods (Array/String/Dna/…) or `builtins`\n    \
         helix describe [what]    the API as JSON — a name, a Type, or everything",
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

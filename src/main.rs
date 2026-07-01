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
mod error;
mod gff;
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
    // Return freed memory to the OS promptly (mimalloc `purge_delay = 0`) instead of
    // its default ~10 ms hold. Helix processes are typically short-lived (CLI,
    // serverless), so they exit before that delay ever fires — leaving freed pages
    // resident and inflating peak RSS by tens of MB. Immediate purging keeps the
    // allocator's wall-time win while cutting peak RSS to ~system-allocator levels on
    // the data workloads (measured: VCF read 1.48x->1.08x, group-by 1.77x->1.46x).
    // `15` is `mi_option_purge_delay` in mimalloc v3 (the version the crate builds);
    // the enum's `deprecated_*` placeholders keep that index stable across v3 releases.
    #[cfg(feature = "mimalloc")]
    unsafe {
        libmimalloc_sys::mi_option_set(15, 0);
    }
    // The bytecode VM — the default engine — recurses on the *heap* (frames in a
    // `Vec`), so it runs on the ordinary main-thread stack. Only the tree-walker
    // recurses on the native stack, and it is now reached just as a rare
    // compile-fallback or under `HELIX_NOVM` / the REPL; those paths spawn a
    // big-stack thread on demand (see `run_on_big_stack`). So the process no longer
    // reserves 2 GiB up front for every invocation.
    run()
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
        // `helix build <script> [-o name]` — bundle a program into a standalone exe.
        Some("build") => run_build(&args),
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

/// `helix build <script> [-o name]` — bundle a single-file program into a standalone
/// executable (see `src/bundle.rs`). Runs on the big stack: the build path loads and
/// type-checks the program, both of which recurse over the AST.
fn run_build(args: &[String]) -> ExitCode {
    let entry = match args.get(2) {
        Some(p) if !p.starts_with('-') => p.clone(),
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
         helix <script.helix>     run a script (shorthand)\n    \
         helix run <script>       run a script\n    \
         helix eval \"<code>\"       run a one-liner\n    \
         helix build <script>     bundle a program into a standalone executable\n    \
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

fn run_file(path: &str) -> ExitCode {
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
        if files.is_empty() {
            println!("no tests found (looked for `*_test.helix` under {})", root.display());
            return ExitCode::SUCCESS;
        }
        println!("running {} test file{}", files.len(), if files.len() == 1 { "" } else { "s" });
        // Display paths relative to the search root (its parent when the root is a single
        // file, so the file's own name still shows).
        let base = if root.is_file() { root.parent().unwrap_or(&root) } else { root.as_path() };
        let mut failed = 0usize;
        for f in &files {
            let shown = f.strip_prefix(base).unwrap_or(f).display();
            match run_file_capture(f) {
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
        let passed = files.len() - failed;
        println!();
        if failed == 0 {
            println!("{passed} passed");
            ExitCode::SUCCESS
        } else {
            println!("{passed} passed, {failed} failed");
            ExitCode::FAILURE
        }
    })
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

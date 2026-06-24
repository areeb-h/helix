//! Helix — a modern scientific programming language.
//!
//! Phase 1: a tree-walking interpreter for the core language.
//!
//! Usage:
//!     helix path/to/script.helix      run a file
//!     helix                           start the REPL

mod ast;
mod bio;
mod bytecode;
mod dataframe;
mod error;
mod http;
mod interp;
mod jit;
mod json;
mod lexer;
mod managed;
mod module;
mod net;
mod parser;
mod python;
mod stats;
mod tensor;
mod token;
mod types;
mod value;
mod vcf;
mod vm;

use std::io::{self, Write};
use std::process::ExitCode;

use error::HelixError;
use interp::Interp;
use value::Value;

fn main() -> ExitCode {
    // The bytecode VM — the default engine — recurses on the *heap* (frames in a
    // `Vec`), so it runs on the ordinary main-thread stack. Only the tree-walker
    // recurses on the native stack, and it is now reached just as a rare
    // compile-fallback or under `HELIX_NOVM` / the REPL; those paths spawn a
    // big-stack thread on demand (see `run_on_big_stack`). So the process no longer
    // reserves 2 GiB up front for every invocation.
    run()
}

fn run() -> ExitCode {
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
        // `helix python <…>` — manage CPython runtimes for interop.
        Some("python") => run_python_cli(&args),
        // Shorthand: `helix script.helix` runs a file directly.
        Some(path) => run_file(path),
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
fn run_eval(code: &str) -> ExitCode {
    let tokens = match lexer::lex(code) {
        Ok(t) => t,
        Err(e) => {
            eprint!("{}", e.render(code, "<eval>"));
            return ExitCode::FAILURE;
        }
    };
    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            eprint!("{}", e.render(code, "<eval>"));
            return ExitCode::FAILURE;
        }
    };
    match run_program(&program, code, "<eval>", false) {
        Ok(()) => ExitCode::SUCCESS,
        Err(rendered) => {
            eprint!("{}", rendered);
            ExitCode::FAILURE
        }
    }
}

/// Run `f` on a thread with a 2 GiB stack, for the tree-walker's native-stack
/// recursion (a soft `MAX_CALL_DEPTH` still turns runaway recursion into a clean
/// error first). Used only on the tree-walker paths — never for the VM.
fn run_on_big_stack<F>(f: F) -> ExitCode
where
    F: FnOnce() -> ExitCode + Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024 * 1024)
        .spawn(f)
        .expect("failed to spawn interpreter thread")
        .join()
        .unwrap_or(ExitCode::FAILURE)
}

fn print_help() {
    println!(
        "Helix {} — a scientific programming language\n\n\
         USAGE:\n    \
         helix <script.helix>     run a script (shorthand)\n    \
         helix run <script>       run a script\n    \
         helix eval \"<code>\"       run a one-liner\n    \
         helix repl               start an interactive session\n    \
         helix version            show the version\n    \
         helix help               show this help\n\n\
         The default `helix` is a self-contained binary. A build with the `python`\n\
         feature adds CPython interop (see docs/python-interop.md).",
        env!("CARGO_PKG_VERSION")
    );
}

fn run_file(path: &str) -> ExitCode {
    // The module loader reads, lexes, parses, and namespaces the entry file plus
    // everything it imports into one statement list. (A single file passes through
    // unchanged.) Lex/parse/resolve errors come back already rendered.
    let loaded = match module::load(std::path::Path::new(path)) {
        Ok(l) => l,
        Err(rendered) => {
            eprint!("{}", rendered);
            return ExitCode::FAILURE;
        }
    };
    // The entry source, for rendering type/runtime errors. For a multi-file program
    // the caret may point into a dependency while showing the entry file — a known
    // v1 limitation; the message and line:col are still accurate.
    let src = std::fs::read_to_string(path).unwrap_or_default();
    match run_program(&loaded.stmts, &src, path, loaded.multi_module) {
        Ok(()) => ExitCode::SUCCESS,
        Err(rendered) => {
            eprint!("{}", rendered);
            ExitCode::FAILURE
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
fn run_program(program: &[ast::Stmt], src: &str, filename: &str, multi: bool) -> Result<(), String> {
    // The inferred receiver types feed the compiler so it can route
    // receiver-polymorphic methods (DataFrame/Tensor column-verbs) correctly.
    let types = types::check(program).map_err(|e| render_err(e, src, filename, multi))?;

    // The tree-walker runs in two cases: `HELIX_NOVM=1` (A/B benchmarking and engine
    // agreement), and any program that uses `try`, since error recovery is currently
    // implemented in the tree-walker but not the bytecode VM. The tree-walker recurses
    // on the native stack, so it runs on a big-stack thread (scoped, to borrow
    // `program`/`src` without cloning).
    if std::env::var_os("HELIX_NOVM").is_some() || bytecode::uses_try(program) {
        return std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024 * 1024)
                .spawn_scoped(scope, || {
                    let mut interp = Interp::new();
                    interp.run(program).map_err(|e| render_err(e, src, filename, multi))
                })
                .expect("failed to spawn interpreter thread")
                .join()
                .unwrap_or_else(|_| Err("the interpreter thread panicked".to_string()))
        });
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
                jit::build(program, &prog.reduce_loops)
            };
            vm::run(&prog, jit.as_ref()).map_err(|e| render_err(e, src, filename, multi))?
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

/// Render a runtime/type error, stripping module-namespacing prefixes for
/// multi-file programs so users never see the internal `m<N>$` names.
fn render_err(e: HelixError, src: &str, filename: &str, multi: bool) -> String {
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
                // Auto-echo the value of bare expressions, like a scientist's notebook.
                if outcome.is_expr && !matches!(outcome.value, Value::Unit) {
                    println!("{}", outcome.value);
                }
            }
            Err(e) => {
                eprint!("{}", e.render(src, "<repl>"));
                break;
            }
        }
    }
}

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
mod interp;
mod jit;
mod lexer;
mod parser;
mod tensor;
mod token;
mod types;
mod value;
mod vm;

use std::io::{self, Write};
use std::process::ExitCode;

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
        Some("--help") | Some("-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--version") | Some("-V") => {
            println!("helix {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(path) => run_file(path),
        // The REPL drives the tree-walker line by line, so give it the big stack.
        None => run_on_big_stack(repl),
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
         USAGE:\n    helix <script.helix>   run a script\n    helix                  start the REPL\n\n\
         OPTIONS:\n    -h, --help     show this help\n    -V, --version  show the version",
        env!("CARGO_PKG_VERSION")
    );
}

fn run_file(path: &str) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read `{}`: {}", path, e);
            return ExitCode::FAILURE;
        }
    };
    match run_source(&src, path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(rendered) => {
            eprint!("{}", rendered);
            ExitCode::FAILURE
        }
    }
}

/// Lex, parse, and run a whole source string. On failure returns the rendered
/// (caret-annotated) error ready to print.
fn run_source(src: &str, filename: &str) -> Result<(), String> {
    let tokens = lexer::lex(src).map_err(|e| e.render(src, filename))?;
    let program = parser::parse(tokens).map_err(|e| e.render(src, filename))?;
    // Static type check before any execution — a type error prevents side effects.
    // The inferred receiver types feed the compiler so it can route
    // receiver-polymorphic methods (DataFrame/Tensor column-verbs) correctly.
    let types = types::check(&program).map_err(|e| e.render(src, filename))?;

    // `HELIX_NOVM=1` forces the tree-walker — kept only for A/B benchmarking and to
    // confirm the two engines agree. It recurses on the native stack, so it runs on
    // a big-stack thread (scoped, to borrow `program`/`src` without cloning).
    if std::env::var_os("HELIX_NOVM").is_some() {
        return std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024 * 1024)
                .spawn_scoped(scope, || {
                    let mut interp = Interp::new();
                    interp.run(&program).map_err(|e| e.render(src, filename))
                })
                .expect("failed to spawn interpreter thread")
                .join()
                .unwrap_or_else(|_| Err("the interpreter thread panicked".to_string()))
        });
    }

    // The VM is the sole automatic engine: the compiler is *total* for any
    // type-checked program (no `Unsupported` fallback), so there is no silent
    // tree-walker path. The VM recurses on the heap, so it runs on the main thread.
    match bytecode::compile_with_types(&program, Some(types)) {
        Ok(prog) => {
            // `HELIX_NOJIT=1` disables native compilation (keeps the VM) for A/B.
            let jit = if std::env::var_os("HELIX_NOJIT").is_some() {
                None
            } else {
                jit::build(&program, &prog.reduce_loops)
            };
            vm::run(&prog, jit.as_ref()).map_err(|e| e.render(src, filename))?
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

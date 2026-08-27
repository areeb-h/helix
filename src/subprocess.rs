//! `run(program, args?)` — subprocess execution, argv only (ADR 0037 D3).
//!
//! **There is no string form, and that is the design.** Every documented shell-injection
//! bug is a program that took the opt-in: Python's own `subprocess` docs put the escaping
//! burden on the caller when `shell=True`, and the one place a caller forgets is the one
//! that matters. Here the safe form is the ONLY form, so injection through this surface is
//! not discouraged — it is unrepresentable. `run("sh", ["-c", user_input])` is of course
//! still possible; what is gone is the version where a metacharacter in ordinary data
//! becomes a command by accident.
//!
//! **A non-zero exit is an error.** Python's `subprocess.run` defaults `check=False`, so
//! the easy spelling ignores a failed child — the counter-example ADR 0037 cites. Here a
//! failed child raises, and inspecting it instead of propagating uses the mechanism the
//! language already has: `try run(…)` yields `{ok, value, error}`, which is also what
//! `assert_error` reads.
//!
//! **The capability is honest about its ceiling.** `run` holds `Effect::Process`, and
//! ADR 0037 states plainly what that grant means: a subprocess is a **boundary exit, not
//! confinement**. Deno's documentation concedes the same — a child "runs as a separate
//! program with its own permissions" — so granting `run` on a shell, or on `helix`
//! itself, is granting everything. Saying so in the ADR and the docs beats implying a
//! guarantee the process model cannot keep.

use crate::error::HelixError;
use crate::value::Value;
use std::rc::Rc;

/// `run(program, args?)` — execute a program with an argv list and capture its output.
///
/// Returns `{status, stdout, stderr}` on success. Raises when the child exits non-zero,
/// when it is killed by a signal, or when the program cannot be started at all — three
/// different failures that a caller reading only an exit code would conflate.
pub fn run(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let err = |m: String| HelixError::new(m, line, col);

    let Some(Value::Str(program)) = args.first() else {
        return Err(err("`run` takes a program name and an optional array of arguments".to_string())
            .hint("e.g. `run(\"samtools\", [\"sort\", \"-o\", out, input])`.".to_string()));
    };
    if args.len() > 2 {
        return Err(err(format!("`run` takes 1 or 2 arguments, got {}", args.len()))
            .hint("the arguments go in ONE array: `run(\"git\", [\"status\", \"--short\"])`.".to_string()));
    }

    // Arguments are a LIST of values, never a line of text to be re-parsed. Each element
    // reaches the child as exactly one argv entry, whatever it contains.
    let argv: Vec<String> = match args.get(1) {
        None | Some(Value::Missing) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter_values()
            .map(|v| match v {
                Value::Str(s) => Ok(s.to_string()),
                Value::Int(i) => Ok(i.to_string()),
                Value::Float(f) => Ok(crate::value::fmt_float(f)),
                Value::Bool(b) => Ok(b.to_string()),
                other => Err(err(format!(
                    "a `run` argument must be a string or number, not {}",
                    crate::value::with_article(other.type_name())
                ))
                .hint("convert it first — an argument reaches the program as text.".to_string())),
            })
            .collect::<Result<_, _>>()?,
        Some(other) => {
            return Err(err(format!(
                "`run`'s arguments must be an array, not {}",
                crate::value::with_article(other.type_name())
            ))
            .hint("pass them as a list: `run(\"git\", [\"status\"])`.".to_string()));
        }
    };

    let out = std::process::Command::new(program.as_str())
        .args(&argv)
        // The child must never inherit this process's stdin: a program that decides to
        // prompt would otherwise silently consume the Helix program's own input, or hang
        // forever in a pipeline with nothing to read.
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            err(format!("could not run `{program}`: {e}")).hint(
                "check the program is installed and on PATH; `run` takes a program name, not a shell line."
                    .to_string(),
            )
        })?;

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let record = |status: i64| {
        Value::Record(Rc::new(vec![
            (crate::symbol::Symbol::intern("status"), Value::Int(status)),
            (crate::symbol::Symbol::intern("stdout"), Value::Str(Rc::new(stdout.clone()))),
            (crate::symbol::Symbol::intern("stderr"), Value::Str(Rc::new(stderr.clone()))),
        ]))
    };

    match out.status.code() {
        Some(0) => Ok(record(0)),
        Some(code) => {
            // The child's OWN stderr is the useful part of this failure, so it goes in
            // the message rather than being left in a record the raise discards. Trimmed
            // to the first lines: a compiler that printed 400 lines of errors should not
            // bury the fact that it was `cc` that failed.
            let detail: String = stderr.lines().take(3).collect::<Vec<_>>().join("\n");
            let mut e = err(format!("`{program}` exited with status {code}"));
            if !detail.trim().is_empty() {
                e = e.hint(detail);
            }
            Err(e)
        }
        // Killed by a signal: `code()` is None on Unix. Reporting "status missing" would
        // be useless, and reporting 0 would be a lie.
        None => Err(err(format!("`{program}` was killed by a signal"))
            .hint("the child did not exit normally — it was terminated from outside.".to_string())),
    }
}

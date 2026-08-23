//! Builtins: printing, assertions, and process-facing odds and ends — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.


use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

pub(super) fn call(name: &str, args: Vec<Value>, line: usize, col: usize) -> Called {
    if !matches!(name, "print" | "emit" | "write" | "elog" | "read_int" | "sleep" | "assert" | "raise" | "source_path" | "assert_eq" | "assert_close" | "clock_monotonic") {
        return Called::Not(args);
    }
    Called::Done(dispatch(name, args, line, col))
}

fn dispatch(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
                "print" => {
                    // Rich rendering on a terminal (tables, color, elision, grouped
                    // numbers); byte-identical to the plain `display_value` join when
                    // piped/redirected. A DataFrame argument still materializes here, so
                    // a failed query is a real error (non-zero exit), never a swallowed
                    // placeholder printed as if the program succeeded.
                    println!("{}", crate::render::render_print(&args, line, col)?);
                    Ok(Value::Unit)
                }
                // `emit(x)` — write one value as a single PLAIN line and FLUSH immediately, so a
                // downstream consumer sees it now rather than at exit. `print` is block-buffered
                // when piped (and rich-formats for a terminal); `emit` is the streaming sink —
                // one value per line, machine-readable (pair with `x.to_json()` for NDJSON).
                "emit" => {
                    arity(name, &args, 1, line, col)?;
                    use std::io::Write;
                    let s = crate::value::display_value(&args[0], line, col)?;
                    let mut out = std::io::stdout().lock();
                    // Errors writing to a closed pipe are the consumer's business, not a Helix
                    // runtime error (a piped reader exiting first is normal); ignore them.
                    let _ = writeln!(out, "{s}");
                    let _ = out.flush();
                    Ok(Value::Unit)
                }
                // `write(x)` — the inline sibling of `emit`: same plain, flush-now streaming sink,
                // but with NO trailing newline, so tokens flow across one line like a live chat UI
                // (`for t in stream: write(t)`). `emit` frames one value per line; `write` frames
                // nothing — the program decides where breaks go (a final `emit("")` ends the line).
                "write" => {
                    arity(name, &args, 1, line, col)?;
                    use std::io::Write;
                    let s = crate::value::display_value(&args[0], line, col)?;
                    let mut out = std::io::stdout().lock();
                    let _ = write!(out, "{s}");
                    let _ = out.flush();
                    Ok(Value::Unit)
                }
                // `elog(x)` — `emit` for the **stderr** channel: one value per line, flushed now.
                // Lets a program stream results on stdout while sending progress/logs to stderr, so
                // the two don't interleave when stdout is piped (`helix run x.helix | consumer`).
                "elog" => {
                    arity(name, &args, 1, line, col)?;
                    use std::io::Write;
                    let s = crate::value::display_value(&args[0], line, col)?;
                    let mut err = std::io::stderr().lock();
                    let _ = writeln!(err, "{s}");
                    let _ = err.flush();
                    Ok(Value::Unit)
                }
                // `read_int()` — read one line from stdin and parse it as an integer. The
                // console-input primitive (the companion to `print`/`emit`). Returns
                // `missing` on end-of-input or a non-numeric line, so a program can detect
                // "no more input" without crashing (ADR 0001). Non-deterministic, so it is
                // outside the differential oracle.
                "read_int" => {
                    arity(name, &args, 0, line, col)?;
                    use std::io::BufRead;
                    let mut buf = String::new();
                    match std::io::stdin().lock().read_line(&mut buf) {
                        Ok(0) => Ok(Value::Missing), // EOF
                        Ok(_) => Ok(buf.trim().parse::<i64>().map(Value::Int).unwrap_or(Value::Missing)),
                        Err(_) => Ok(Value::Missing),
                    }
                }
                // `sleep(ms)` — pause the program for `ms` milliseconds (wall clock). The
                // pacing primitive for a paced loop (`emit(frame)` then `sleep(16)` ≈ 60 fps);
                // pairs with `clock_monotonic()`. A non-deterministic effect, so (like `print`/
                // `emit`) it lives outside the differential oracle. Fractional ms are honoured.
                "sleep" => {
                    arity(name, &args, 1, line, col)?;
                    let ms = match &args[0] {
                        Value::Int(n) => *n as f64,
                        Value::Float(f) => *f,
                        other => {
                            return Err(type_err("sleep", "a number of milliseconds", other, line, col))
                        }
                    };
                    if !ms.is_finite() || ms < 0.0 {
                        return Err(HelixError::new(
                            "`sleep` needs a non-negative, finite number of milliseconds",
                            line,
                            col,
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_secs_f64(ms / 1000.0));
                    Ok(Value::Unit)
                }
                "assert" => {
                    if args.is_empty() || args.len() > 2 {
                        return Err(HelixError::new(
                            format!("`assert` takes 1 or 2 arguments, got {}", args.len()),
                            line,
                            col,
                        ));
                    }
                    match &args[0] {
                        Value::Bool(true) => Ok(Value::Unit),
                        Value::Bool(false) | Value::Missing => {
                            // A `missing` condition is not provably true → a failed assertion.
                            let msg = match args.get(1) {
                                Some(Value::Str(s)) => format!("assertion failed: {s}"),
                                Some(other) => format!(
                                    "assertion failed: {}",
                                    crate::value::display_value(other, line, col)?
                                ),
                                None => "assertion failed".to_string(),
                            };
                            Err(HelixError::new(msg, line, col))
                        }
                        other => Err(type_err("assert", "a boolean condition", other, line, col)),
                    }
                }
                // `raise(message[, help])` — a library reporting its CALLER's mistake. The only
                // mechanism before this was `assert`, which hard-codes "assertion failed: " and
                // cannot carry a `help:` line, so `route.go("admin")` came back as
                // "assertion failed: route path must start with '/'" — which reads as a broken
                // library rather than a rejected argument. ADR 0004 leaves open whether user
                // errors can be as instructive as the interpreter's own; they can now.
                //
                // Caught by `try` like any other error, because it IS one — the same
                // `HelixError` the runtime raises everywhere else, with no special variant.
                "raise" => {
                    if args.is_empty() || args.len() > 2 {
                        return Err(HelixError::new(
                            format!("`raise` takes 1 or 2 arguments, got {}", args.len()),
                            line,
                            col,
                        ));
                    }
                    let text = |v: &Value| match v {
                        Value::Str(s) => Ok(s.to_string()),
                        other => crate::value::display_value(other, line, col),
                    };
                    let e = HelixError::new(text(&args[0])?, line, col);
                    Err(match args.get(1) {
                        Some(h) => e.hint(text(h)?),
                        None => e,
                    })
                }
                // `source_path(rel)` — `rel` against the directory of the file this call is
                // WRITTEN in, so a package can read the data it ships no matter where the
                // process was started. Every path used to resolve against the CWD, which meant
                // a library could not ship a scoring matrix, a codon table or a reference panel
                // at all: running the same program from a subdirectory broke it.
                //
                // Already-absolute input is returned untouched, so wrapping a user-supplied
                // path is harmless.
                "source_path" => {
                    arity(name, &args, 1, line, col)?;
                    let Value::Str(rel) = &args[0] else {
                        return Err(type_err("source_path", "a string path", &args[0], line, col));
                    };
                    let p = std::path::Path::new(rel.as_str());
                    if p.is_absolute() {
                        return Ok(Value::Str(rel.clone()));
                    }
                    // `line` is the GLOBAL line in the flattened program, which is exactly what
                    // identifies the module it came from.
                    let dir = crate::module::file_of_line(line)
                        .and_then(|f| std::path::Path::new(&f).parent().map(|d| d.to_path_buf()));
                    let Some(dir) = dir else {
                        return Err(HelixError::new(
                            "`source_path` needs a source file to resolve against",
                            line,
                            col,
                        )
                        .hint(
                            "it answers \"where is the file I am written in?\", so it has no \
                             meaning in the REPL — pass an absolute path instead.",
                        ));
                    };
                    Ok(Value::Str(dir.join(p).to_string_lossy().into_owned().into()))
                }
                "assert_eq" => {
                    arity(name, &args, 2, line, col)?;
                    if values_equal(&args[0], &args[1]) {
                        Ok(Value::Unit)
                    } else {
                        let a = crate::value::display_value(&args[0], line, col)?;
                        let b = crate::value::display_value(&args[1], line, col)?;
                        Err(HelixError::new(format!("assertion failed: {a} != {b}"), line, col))
                    }
                }
                "assert_close" => {
                    if args.len() < 2 || args.len() > 3 {
                        return Err(HelixError::new(
                            format!("`assert_close` takes 2 or 3 arguments, got {}", args.len()),
                            line,
                            col,
                        ));
                    }
                    let num = |v: &Value| -> Result<f64, HelixError> {
                        match v {
                            Value::Int(i) => Ok(*i as f64),
                            Value::Float(f) => Ok(*f),
                            other => Err(type_err("assert_close", "a number", other, line, col)),
                        }
                    };
                    let (a, b) = (num(&args[0])?, num(&args[1])?);
                    // Default tolerance suits the f64 round-off of typical scientific compute.
                    let tol = match args.get(2) {
                        Some(v) => num(v)?,
                        None => 1e-9,
                    };
                    if (a - b).abs() <= tol {
                        Ok(Value::Unit)
                    } else {
                        Err(HelixError::new(
                            format!("assertion failed: {a} is not within {tol} of {b}"),
                            line,
                            col,
                        ))
                    }
                }
                "clock_monotonic" => {
                    arity(name, &args, 0, line, col)?;
                    use std::sync::OnceLock;
                    use std::time::Instant;
                    static START: OnceLock<Instant> = OnceLock::new();
                    Ok(Value::Float(START.get_or_init(Instant::now).elapsed().as_secs_f64()))
                }
        _ => Err(HelixError::new(
            format!("internal: `{name}` routed to the wrong builtin module"),
            line,
            col,
        )),
    }
}

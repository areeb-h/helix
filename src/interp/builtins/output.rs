//! Builtins: printing, assertions, and process-facing odds and ends — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.


use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

/// The in-process capture sink: when armed (by the test runner ONLY — never
/// by shards or ordinary runs), print/emit/write land here instead of stdout,
/// so a test's own output cannot corrupt `helix test --json`'s one-document
/// contract, and prose mode can indent it under the file's result line. The
/// relaxed AtomicBool is the fast path — ordinary programs pay one predictable
/// branch, never a lock.
pub(crate) static CAPTURING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static CAPTURE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub(crate) fn capture_begin() {
    if let Ok(mut b) = CAPTURE.lock() {
        b.clear();
    }
    CAPTURING.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn capture_take() -> String {
    CAPTURING.store(false, std::sync::atomic::Ordering::Relaxed);
    match CAPTURE.lock() {
        Ok(mut b) => std::mem::take(&mut *b),
        Err(_) => String::new(),
    }
}

/// True and captured, or false and the caller writes to the real stream.
fn captured(s: &str) -> bool {
    if !CAPTURING.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    if let Ok(mut b) = CAPTURE.lock() {
        b.push_str(s);
        true
    } else {
        false
    }
}

/// Capture one line: `s`, then a newline.
///
/// Split out from [`captured`] because the old spelling -- `captured(&format!("{s}\n"))`
/// -- built a whole second copy of EVERY line a program printed, purely to hand it to a
/// function whose first act is to return `false` when capture is off. Capture is off for
/// every run except `helix check`'s, so that allocation was pure waste on the only path
/// that is ever hot. Appending the newline separately also drops the copy on the capturing
/// path, where the old form allocated too.
fn captured_line(s: &str) -> bool {
    if !CAPTURING.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    if let Ok(mut b) = CAPTURE.lock() {
        b.push_str(s);
        b.push('\n');
        true
    } else {
        false
    }
}

/// Report a failed write to a standard stream.
///
/// A write that fails is a condition of the ENVIRONMENT -- a full disk, an exceeded
/// quota, an I/O error -- not a defect in the program and not a defect in the runtime.
/// The two spellings this replaces were both wrong, in opposite directions.
///
/// `print` used `println!`, which PANICS on a failed write. `helix run prog.helix >
/// /dev/full` reached the user as
///
/// ```text
/// error: internal error (.../stdio.rs:1166): failed printing to stdout:
///        No space left on device (os error 28)
/// help: this is a bug in Helix; please report it with the program that triggered it.
/// ```
///
/// with exit 134 and a core dump. ADR 0024 says user input never aborts the host, and a
/// disk that filled up under a correct program is exactly that; the help text also sent
/// its author to a bug tracker that cannot help them.
///
/// `emit` / `write` / `elog` did `let _ = ...` instead, on the stated grounds that
/// "errors writing to a closed pipe are the consumer's business". That was true when it
/// was written and is obsolete now: `main.rs` restores SIGPIPE to `SIG_DFL`, so a closed
/// pipe kills the process by signal like any other Unix tool and never returns an `Err`
/// here. That arm could therefore only ever swallow a REAL failure -- reporting success
/// for output that never landed.
fn wrote(r: std::io::Result<()>, stream: &str, line: usize, col: usize) -> Result<(), HelixError> {
    r.map_err(|e| HelixError::new(format!("could not write to {stream}: {e}"), line, col))
}

#[inline]
pub(super) fn a_print(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // Rich rendering on a terminal (tables, color, elision, grouped
        // numbers); byte-identical to the plain `display_value` join when
        // piped/redirected. A DataFrame argument still materializes here, so
        // a failed query is a real error (non-zero exit), never a swallowed
        // placeholder printed as if the program succeeded.
        let s = crate::render::render_print(&args, line, col)?;
        if !captured_line(&s) {
            use std::io::Write;
            // Not `println!` -- see `wrote`. Taking the lock once for both writes also
            // skips the `format_args!` machinery a `println!` would run per call.
            let mut out = std::io::stdout().lock();
            wrote(
                out.write_all(s.as_bytes()).and_then(|()| out.write_all(b"\n")),
                "stdout",
                line,
                col,
            )?;
        }
        Ok(Value::Unit)
    
}

// `emit(x)` — write one value as a single PLAIN line and FLUSH immediately, so a
// downstream consumer sees it now rather than at exit. `print` is block-buffered
// when piped (and rich-formats for a terminal); `emit` is the streaming sink —
// one value per line, machine-readable (pair with `x.to_json()` for NDJSON).

#[inline]
pub(super) fn a_emit(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        use std::io::Write;
        let s = crate::value::display_value(&args[0], line, col)?;
        if captured_line(&s) {
            return Ok(Value::Unit);
        }
        let mut out = std::io::stdout().lock();
        wrote(writeln!(out, "{s}").and_then(|()| out.flush()), "stdout", line, col)?;
        Ok(Value::Unit)
    
}

// `write(x)` — the inline sibling of `emit`: same plain, flush-now streaming sink,
// but with NO trailing newline, so tokens flow across one line like a live chat UI
// (`for t in stream: write(t)`). `emit` frames one value per line; `write` frames
// nothing — the program decides where breaks go (a final `emit("")` ends the line).

#[inline]
pub(super) fn a_write(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        use std::io::Write;
        let s = crate::value::display_value(&args[0], line, col)?;
        if captured(&s) {
            return Ok(Value::Unit);
        }
        let mut out = std::io::stdout().lock();
        wrote(write!(out, "{s}").and_then(|()| out.flush()), "stdout", line, col)?;
        Ok(Value::Unit)
    
}

// `elog(x)` — `emit` for the **stderr** channel: one value per line, flushed now.
// Lets a program stream results on stdout while sending progress/logs to stderr, so
// the two don't interleave when stdout is piped (`helix run x.helix | consumer`).

#[inline]
pub(super) fn a_elog(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        use std::io::Write;
        let s = crate::value::display_value(&args[0], line, col)?;
        let mut err = std::io::stderr().lock();
        wrote(writeln!(err, "{s}").and_then(|()| err.flush()), "stderr", line, col)?;
        Ok(Value::Unit)
    
}

// `read_int()` — read one line from stdin and parse it as an integer. The
// console-input primitive (the companion to `print`/`emit`). Returns
// `missing` on end-of-input or a non-numeric line, so a program can detect
// "no more input" without crashing (ADR 0001). Non-deterministic, so it is
// outside the differential oracle.

#[inline]
pub(super) fn a_read_int(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_sleep(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_assert(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_raise(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_source_path(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_assert_eq(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 2, line, col)?;
        if values_equal(&args[0], &args[1]) {
            Ok(Value::Unit)
        } else {
            let a = crate::value::display_value(&args[0], line, col)?;
            let b = crate::value::display_value(&args[1], line, col)?;
            Err(HelixError::new(format!("assertion failed: {a} != {b}"), line, col))
        }
    
}

/// `assert_error(try expr)` / `assert_error(try expr, "substring")` — the expression
/// must have FAILED, and its message must say what you expected.
///
/// It takes the record `try` already produces (`{ok, value, error}`) rather than a
/// callback, so it needs no new evaluator machinery and composes with a feature that is
/// already there: `assert_error(try parse_it(s), "not a number")`.
///
/// The point is the failure message, not the check. The idiom this replaces —
/// `r = try f()` then `assert(r.error.contains("…"))` — is only three lines, but when it
/// fails it says `assertion failed` and NOTHING about what the error actually was, so a
/// wrong-message failure tells you nothing you can act on. In a language that pins error
/// text as part of its contract (`tests/compat/` freezes stderr for 119 programs), the
/// message is the thing under test, and an assertion about it has to show it.
pub(super) fn a_assert_error(
    name: &str,
    args: Vec<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    if args.is_empty() || args.len() > 2 {
        return Err(HelixError::new(
            format!("`{name}` takes a `try` result and an optional substring, got {} arguments", args.len()),
            line,
            col,
        ));
    }
    let Value::Record(fields) = &args[0] else {
        return Err(type_err(name, "a `try` result", &args[0], line, col)
            .hint("pass the whole expression: `assert_error(try risky(x), \"why\")`."));
    };
    let get = |k: &str| fields.iter().find(|(n, _)| n.as_str() == k).map(|(_, v)| v);
    let (Some(ok), Some(err)) = (get("ok"), get("error")) else {
        return Err(HelixError::new(
            format!("`{name}` needs a `try` result — a record with `ok` and `error`"),
            line,
            col,
        )
        .hint("pass the whole expression: `assert_error(try risky(x), \"why\")`."));
    };
    if matches!(ok, Value::Bool(true)) {
        // Naming the value it produced instead is what turns "this did not fail" into
        // something you can act on without re-running it by hand.
        let got = get("value")
            .map(|v| crate::value::display_value(v, line, col))
            .transpose()?
            .unwrap_or_else(|| "nothing".to_string());
        return Err(HelixError::new(
            format!("assertion failed: expected an error, but it succeeded with {got}"),
            line,
            col,
        ));
    }
    let Some(want) = args.get(1) else {
        return Ok(Value::Unit);
    };
    let Value::Str(want) = want else {
        return Err(type_err(name, "a string to look for", want, line, col));
    };
    let actual = match err {
        Value::Str(s) => s.to_string(),
        other => crate::value::display_value(other, line, col)?,
    };
    if actual.contains(want.as_str()) {
        Ok(Value::Unit)
    } else {
        // BOTH sides, always: the expected substring is useless without the message it
        // was not found in.
        Err(HelixError::new(
            format!("assertion failed: expected an error containing `{want}`, but it said: {actual}"),
            line,
            col,
        ))
    }
}

#[inline]
pub(super) fn a_assert_close(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
/// `type_of(v)` — the value's type name, as a String.
///
/// The vocabulary is `Value::type_name()`, which is ALREADY the vocabulary of every
/// diagnostic in the language ("found a value of type String"). A second set of names
/// for one concept is the drift this project keeps removing, so there is no second set.
///
/// **Why this is not a nit.** Without it, the only way to ask "is this a record?" is to
/// attempt a field access and catch the failure — and a field report measured that at
/// **3.801 µs against 0.104 µs for a plain lookup, 36×**. Run once per interpolated hole
/// in a template renderer, it was 5.8× the cost of the entire render. `type_of(v) ==
/// "Record"` is one comparison. A language that can only discover a type by provoking an
/// error charges an exception for a question.
pub(super) fn a_type_of(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 1, line, col)?;
    Ok(Value::Str(std::rc::Rc::new(args[0].type_name().to_string())))
}

/// `has_feature(name)` — is this build's `name` capability compiled in?
///
/// ADR 0032 gates the BODY, not the name: `re_match` in an appliance build still exists,
/// type-checks and describes itself, and running it says what to rebuild with. What a
/// PROGRAM could not do was ask BEFORE calling, so a library that wanted to degrade
/// gracefully had to provoke the failure and catch it. That is the charge `type_of` above
/// was added to remove, and this is the same charge one level up: a question should not
/// cost an exception.
///
/// AN UNKNOWN NAME IS AN ERROR, NOT `false`. A typo answered with `false` would send a
/// program down its fallback path forever, on every build, with nothing to see — the exact
/// shape of a silent wrong answer. Every name Cargo.toml defines answers truthfully;
/// nothing else answers at all.
pub(super) fn a_has_feature(
    name: &str,
    args: Vec<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    arity(name, &args, 1, line, col)?;
    let want = match &args[0] {
        Value::Str(s) => (**s).clone(),
        other => {
            return Err(HelixError::new(
                format!("`has_feature` expects a feature name string, found {}", other.type_name()),
                line,
                col,
            )
            .hint("e.g. `has_feature(\"regex\")`."))
        }
    };
    // One arm per feature in Cargo.toml's [features], so the set here cannot drift from the
    // set that exists. `cfg!` is a compile-time constant, so this is a load, not a lookup.
    let on = match want.as_str() {
        "appliance" => cfg!(feature = "appliance"),
        "bio" => cfg!(feature = "bio"),
        "database" | "db" => cfg!(feature = "db"),
        "dataframes" => cfg!(feature = "dataframes"),
        "default" => cfg!(feature = "default"),
        "http" => cfg!(feature = "http"),
        "jit" => cfg!(feature = "jit"),
        "managed" => cfg!(feature = "managed"),
        "mimalloc" => cfg!(feature = "mimalloc"),
        "native-df" => cfg!(feature = "native-df"),
        "postgres" => cfg!(feature = "postgres"),
        "python" => cfg!(feature = "python"),
        "regex" => cfg!(feature = "regex"),
        _ => {
            return Err(HelixError::new(
                format!("`{want}` is not a build feature"),
                line,
                col,
            )
            .hint(
                "one of: appliance, bio, database, dataframes, default, http, jit, managed, \
                 mimalloc, native-df, postgres, python, regex.",
            ))
        }
    };
    Ok(Value::Bool(on))
}

/// `now()` — seconds since the Unix epoch, as a Float.
///
/// `clock_monotonic` measures elapsed time within ONE process, which is the right
/// primitive for a benchmark or a token bucket and useless for anything that must
/// outlive the process. Before this, **no absolute instant was expressible anywhere in
/// Helix**: not an expiry, not a timestamp in a log line, not a cache TTL that survives a
/// restart, not "rows since Monday". A field report hit it building sessions — a stolen
/// cookie could not be expired server-side across a restart, and every workaround was
/// worse.
///
/// A timestamp is DATA for a language that reads VCFs, serves HTTP and writes CSVs: a
/// pipeline that cannot record when it ran cannot be audited. Kept as a Float of epoch
/// seconds rather than waiting for a date TYPE (ADR 0030), because the float is what
/// removes the blocker and a type can wrap it later without changing this answer.
///
/// Effect: reading a clock is not fs/net authority, so it sits beside `clock_monotonic`
/// in the known-harmless set. It is `pure: false` because it is not referentially
/// transparent — two calls differ, which is the whole point.
pub(super) fn a_now(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 0, line, col)?;
    // A clock before 1970 is a misconfigured machine, not a Helix condition; report it
    // rather than panicking or silently answering 0 (ADR 0024: never abort the host).
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => Ok(Value::Float(d.as_secs_f64())),
        Err(_) => Err(HelixError::new(
            "the system clock is set before 1970, so `now()` has no answer",
            line,
            col,
        )
        .hint("check the machine's clock; `clock_monotonic()` measures elapsed time and is unaffected.")),
    }
}

pub(super) fn a_clock_monotonic(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 0, line, col)?;
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        Ok(Value::Float(START.get_or_init(Instant::now).elapsed().as_secs_f64()))
    
}

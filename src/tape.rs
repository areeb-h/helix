//! The input tape: what an app's `update` and `view` read from the world, recorded on one
//! machine and played back on another (session replay).
//!
//! An app is a fold of `update` over its events, and `std.ui` already records the events.
//! But `update` and `view` may also READ the world — `now()`, `read_dir`, `file_info`,
//! `http_get`, `run` — and a replay that reads the laptop's world instead of the phone's is
//! not a replay. So while `std.ui` runs an app's own code it opens the tape:
//!
//! - **Record:** every impure builtin (and every write method) runs as usual, and its result —
//!   or its error, message and hint — is appended, encoded losslessly (`Int` stays `Int`,
//!   bytes stay bytes, `missing` stays `missing`).
//! - **Replay:** the same calls do NOT run. Each returns the next recorded result, so a replay
//!   reaches no file, socket or process, and writes nothing. A call that does not match the
//!   recording — another builtin, or one too many — is a divergence, reported by `tape_end`.
//!
//! Randomness needs nothing: `random(n, seed)` is seed-threaded (ADR 0020), and a seed that
//! came from `now()` was recorded with it. Output (`print`, `elog`, …) is not taped: it does
//! not reach the model or the screen, and a replay that prints its log is easier to read.
//!
//! The tape lives on the evaluating thread, and is only ever open between `tape_begin()` and
//! `tape_end()`, which `std.ui` calls around `update` and `view`. The runtime's own sockets,
//! timers and clock are outside it.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use base64::Engine as _;
use serde_json::{json, Map, Value as J};

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::{DictKey, Value};

enum Tape {
    Off,
    Record(Vec<J>),
    Replay { queue: VecDeque<J>, diverged: Option<String> },
}

thread_local! {
    static TAPE: RefCell<Tape> = const { RefCell::new(Tape::Off) };
}

/// Builtins that are impure but stay OFF the tape: the program's own control flow
/// (`raise`, assertions), output that reaches neither model nor screen, and the session
/// machinery itself.
fn untaped(name: &str) -> bool {
    matches!(
        name,
        "raise" | "assert" | "assert_eq" | "assert_error" | "assert_close"
            | "print" | "emit" | "write" | "elog"
            | "tape_begin" | "tape_end" | "session_record_path" | "session_replay_path"
            | "dev_generation"
    )
}

/// True when the tape is open and this builtin's result must go through it.
pub fn takes_builtin(name: &str) -> bool {
    is_open() && crate::registry::is_impure_builtin(name) && !untaped(name)
}

/// True when the tape is open and this method writes (files or a database).
pub fn takes_method(name: &str) -> bool {
    use crate::capability::Effect;
    is_open() && matches!(crate::capability::method_effect_of(name), Effect::FsWrite | Effect::DbWrite)
}

fn is_open() -> bool {
    // Constant folding (ADR 0050) tries calls while the program loads and abandons any that
    // turn out impure. Those attempts are the compiler's, not the program's: they never reach
    // the world, and taping them would make a recording depend on what the folder tried.
    !crate::fold::sandbox_active() && TAPE.with(|t| !matches!(*t.borrow(), Tape::Off))
}

/// Run `call` through the tape under `label`: record what it returns, or return what was
/// recorded without running it.
pub fn through(
    label: &str,
    line: usize,
    col: usize,
    call: impl FnOnce() -> Result<Value, HelixError>,
) -> Result<Value, HelixError> {
    let replaying = TAPE.with(|t| matches!(*t.borrow(), Tape::Replay { .. }));
    if !replaying {
        let out = call();
        let entry = match &out {
            Ok(v) => match encode(v) {
                Ok(e) => json!({"fn": label, "ok": e}),
                Err(kind) => json!({"fn": label, "unrecordable": kind}),
            },
            Err(e) => json!({"fn": label, "err": e.message, "hint": e.hint}),
        };
        TAPE.with(|t| {
            if let Tape::Record(v) = &mut *t.borrow_mut() {
                v.push(entry);
            }
        });
        return out;
    }
    let next = TAPE.with(|t| match &mut *t.borrow_mut() {
        Tape::Replay { queue, diverged } => {
            // After a divergence nothing more is replayed; every later call says why.
            if let Some(w) = diverged {
                return Err(w.clone());
            }
            let e = queue.pop_front();
            let why = match &e {
                None => Some(format!("the app called `{label}`, but the recording has no more inputs for this step")),
                Some(e) if e.get("fn").and_then(J::as_str) != Some(label) => Some(format!(
                    "the app called `{label}` where the recording has `{}`",
                    e.get("fn").and_then(J::as_str).unwrap_or("?")
                )),
                _ => None,
            };
            if let Some(w) = why {
                *diverged = Some(w.clone());
                return Err(w);
            }
            Ok(e.unwrap())
        }
        _ => Err("tape closed".to_string()),
    });
    let entry = next.map_err(|w| HelixError::new(format!("replay diverged: {w}"), line, col))?;
    if let Some(ok) = entry.get("ok") {
        return decode(ok).map_err(|m| HelixError::new(format!("replay: bad recorded value for `{label}`: {m}"), line, col));
    }
    if let Some(kind) = entry.get("unrecordable").and_then(J::as_str) {
        return Err(HelixError::new(
            format!("replay: `{label}` returned a {kind} on the device, which session replay cannot record yet"),
            line,
            col,
        ));
    }
    let msg = entry.get("err").and_then(J::as_str).unwrap_or("recorded error").to_string();
    let mut err = HelixError::new(msg, line, col);
    if let Some(h) = entry.get("hint").and_then(J::as_str) {
        err = err.hint(h);
    }
    Err(err)
}

/// Open the tape for the whole program, before it runs, when a session is being recorded or
/// replayed. Without this, code an app runs before `ui.app` — first-run setup, a migration —
/// would read and write the replaying machine's world. `std.ui`'s `app()` closes it
/// (`tape_end()`) and records it as the session's `startup`; a replay reads that line here,
/// first, so `main`'s reads return what they returned on the device and its writes don't happen.
///
/// Called on the evaluating thread (the tape is thread-local), by `helix run` only.
pub fn open_for_program() {
    let next = if let Ok(path) = std::env::var("HELIX_REPLAY").map(std::path::PathBuf::from) {
        // The first line of a log written since the startup tape exists is `{"startup": …}`.
        // An older log, or one that can't be read, replays with an empty startup: a program
        // that reads nothing before `ui.app` replays exactly; one that does is told where.
        let startup = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.lines().next().map(str::to_string))
            .and_then(|l| serde_json::from_str::<J>(&l).ok())
            .and_then(|j| j.get("startup").and_then(J::as_str).map(str::to_string))
            .and_then(|t| serde_json::from_str::<J>(&t).ok())
            .and_then(|j| j.as_array().cloned())
            .unwrap_or_default();
        Tape::Replay { queue: startup.into(), diverged: None }
    } else if std::env::var_os("HELIX_RECORD").is_some_and(|p| !p.is_empty()) {
        Tape::Record(Vec::new())
    } else {
        return;
    };
    TAPE.with(|t| *t.borrow_mut() = next);
}

// --- The builtins. -------------------------------------------------------------------------

/// `tape_begin(recorded)` — open the tape. `missing` records; a string (what `tape_end`
/// returned when recording) replays it.
pub fn tape_begin(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.len() != 1 {
        return Err(HelixError::new(format!("`tape_begin` takes one argument, got {}", args.len()), line, col));
    }
    let next = match &args[0] {
        Value::Missing => Tape::Record(Vec::new()),
        Value::Str(s) => match serde_json::from_str::<J>(s) {
            Ok(J::Array(v)) => Tape::Replay { queue: v.into(), diverged: None },
            _ => return Err(HelixError::new("`tape_begin`: a recorded tape is a JSON array", line, col)),
        },
        other => {
            return Err(HelixError::new(
                format!("`tape_begin` takes `missing` (record) or a recorded tape string, got {}", other.type_name()),
                line,
                col,
            ))
        }
    };
    TAPE.with(|t| {
        let mut t = t.borrow_mut();
        if !matches!(*t, Tape::Off) {
            return Err(HelixError::new("`tape_begin`: the tape is already open", line, col));
        }
        *t = next;
        Ok(Value::Unit)
    })
}

/// `tape_end()` — close the tape. Recording: the tape, as a JSON string. Replaying: `""` when
/// every recorded input was used exactly as recorded, otherwise why it was not. `missing` when
/// no tape is open (so `ui.app` can close the program's startup tape, if there is one).
pub fn tape_end(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if !args.is_empty() {
        return Err(HelixError::new(format!("`tape_end` takes no arguments, got {}", args.len()), line, col));
    }
    let was = TAPE.with(|t| std::mem::replace(&mut *t.borrow_mut(), Tape::Off));
    let out = match was {
        Tape::Off => return Ok(Value::Missing),
        Tape::Record(v) => J::Array(v).to_string(),
        Tape::Replay { diverged: Some(w), .. } => w,
        Tape::Replay { queue, .. } if !queue.is_empty() => format!(
            "the recording has {} more input(s) for this step, starting with `{}`, that the app did not ask for",
            queue.len(),
            queue[0].get("fn").and_then(J::as_str).unwrap_or("?")
        ),
        Tape::Replay { .. } => String::new(),
    };
    Ok(Value::Str(Rc::new(out)))
}

// --- Lossless encoding. ----------------------------------------------------------------------
//
// JSON alone would turn `1.0` into `1` and bytes into text. Every value is tagged with the
// one-letter kind it came from; plain JSON strings and booleans stand for themselves, `null`
// is `missing`. A float that JSON cannot carry (NaN, ±inf) is spelled as a string.

fn encode(v: &Value) -> Result<J, String> {
    Ok(match v {
        Value::Missing => J::Null,
        Value::Unit => json!({"u": 1}),
        Value::Bool(b) => J::Bool(*b),
        Value::Str(s) => J::String(s.as_str().to_string()),
        Value::Int(n) => json!({"i": n}),
        Value::Float(f) if f.is_finite() => json!({"f": f}),
        Value::Float(f) => json!({"f": if f.is_nan() { "NaN" } else if *f > 0.0 { "inf" } else { "-inf" }}),
        Value::Bytes(b) => json!({"b": base64::engine::general_purpose::STANDARD.encode(b.as_slice())}),
        Value::Dna(s) => json!({"dna": s.as_str()}),
        Value::Rational(q) => json!({"q": q.to_string()}),
        Value::Array(a) => json!({"a": a.to_values().iter().map(encode).collect::<Result<Vec<_>, _>>()?}),
        Value::Tuple(xs) => json!({"t": xs.iter().map(encode).collect::<Result<Vec<_>, _>>()?}),
        Value::Record(fs) => {
            let mut pairs = Vec::with_capacity(fs.len());
            for (k, x) in fs.iter() {
                pairs.push(json!([k.as_str(), encode(x)?]));
            }
            json!({"r": pairs})
        }
        Value::Dict(d) => {
            let mut pairs = Vec::new();
            for (k, x) in d.map().iter() {
                pairs.push(json!([encode(&k.to_value())?, encode(x)?]));
            }
            json!({"d": pairs})
        }
        Value::Headers(hs) => json!({"h": hs.iter().map(|(k, x)| json!([k, x])).collect::<Vec<_>>()}),
        other => return Err(other.type_name().to_string()),
    })
}

fn decode(j: &J) -> Result<Value, String> {
    Ok(match j {
        J::Null => Value::Missing,
        J::Bool(b) => Value::Bool(*b),
        J::String(s) => Value::Str(Rc::new(s.clone())),
        J::Object(m) => decode_tagged(m)?,
        other => return Err(format!("untagged {other}")),
    })
}

fn decode_tagged(m: &Map<String, J>) -> Result<Value, String> {
    let (tag, x) = m.iter().next().ok_or("empty object")?;
    let list = |x: &J| -> Result<Vec<J>, String> { x.as_array().cloned().ok_or_else(|| format!("`{tag}` is not a list")) };
    Ok(match tag.as_str() {
        "u" => Value::Unit,
        "i" => Value::Int(x.as_i64().ok_or("bad int")?),
        "f" => Value::Float(match x {
            J::String(s) if s == "NaN" => f64::NAN,
            J::String(s) if s == "inf" => f64::INFINITY,
            J::String(s) if s == "-inf" => f64::NEG_INFINITY,
            _ => x.as_f64().ok_or("bad float")?,
        }),
        "b" => Value::Bytes(Rc::new(
            base64::engine::general_purpose::STANDARD.decode(x.as_str().ok_or("bad bytes")?).map_err(|e| e.to_string())?,
        )),
        "dna" => Value::Dna(Rc::new(x.as_str().ok_or("bad dna")?.to_string())),
        "q" => Value::Rational(Rc::new(x.as_str().ok_or("bad rational")?.parse().map_err(|_| "bad rational")?)),
        "a" => Value::array(list(x)?.iter().map(decode).collect::<Result<_, _>>()?),
        "t" => Value::Tuple(Rc::new(list(x)?.iter().map(decode).collect::<Result<_, _>>()?)),
        "r" => {
            let mut fs = Vec::new();
            for p in list(x)? {
                let k = p.get(0).and_then(J::as_str).ok_or("bad record key")?;
                fs.push((Symbol::intern(k), decode(p.get(1).ok_or("bad record value")?)?));
            }
            Value::Record(Rc::new(fs))
        }
        "d" => {
            let mut map = std::collections::BTreeMap::new();
            for p in list(x)? {
                let k = DictKey::from_value(&decode(p.get(0).ok_or("bad dict key")?)?)?;
                map.insert(k, decode(p.get(1).ok_or("bad dict value")?)?);
            }
            Value::dict(map)
        }
        "h" => {
            let mut hs = Vec::new();
            for p in list(x)? {
                let k = p.get(0).and_then(J::as_str).ok_or("bad header")?;
                let v = p.get(1).and_then(J::as_str).ok_or("bad header")?;
                hs.push((k.to_string(), v.to_string()));
            }
            Value::Headers(Rc::new(hs))
        }
        other => return Err(format!("unknown tag `{other}`")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(v: Value) -> Value {
        decode(&encode(&v).unwrap()).unwrap()
    }

    #[test]
    fn encoding_keeps_every_kind() {
        let rec = Value::Record(Rc::new(vec![
            (Symbol::intern("n"), Value::Int(3)),
            (Symbol::intern("x"), Value::Float(1.0)),
            (Symbol::intern("gone"), Value::Missing),
            (Symbol::intern("raw"), Value::Bytes(Rc::new(vec![0, 255, 10]))),
            (Symbol::intern("xs"), Value::array(vec![Value::Int(1), Value::Str(Rc::new("a".into()))])),
            (Symbol::intern("nan"), Value::Float(f64::NAN)),
            (Symbol::intern("t"), Value::Float(1795591454.1140141)),
        ]));
        let back = round(rec.clone());
        assert_eq!(format!("{back:?}"), format!("{rec:?}"));
        // `1.0` must come back a Float, not the Int a plain JSON round trip would make it.
        assert!(matches!(round(Value::Float(1.0)), Value::Float(f) if f == 1.0));
        assert!(matches!(round(Value::Int(1)), Value::Int(1)));
    }

    #[test]
    fn replay_returns_recorded_results_without_running() {
        tape_begin(&[Value::Missing], 0, 0).unwrap();
        through("now", 0, 0, || Ok(Value::Float(42.5))).unwrap();
        through("read_text", 0, 0, || Err(HelixError::new("no such file", 0, 0).hint("check the path"))).unwrap_err();
        let Value::Str(tape) = tape_end(&[], 0, 0).unwrap() else { panic!() };

        tape_begin(&[Value::Str(tape.clone())], 0, 0).unwrap();
        let t = through("now", 0, 0, || panic!("replay must not run the builtin")).unwrap();
        assert!(matches!(t, Value::Float(f) if f == 42.5));
        let e = through("read_text", 0, 0, || panic!("replay must not run the builtin")).unwrap_err();
        assert_eq!((e.message.as_str(), e.hint.as_deref()), ("no such file", Some("check the path")));
        assert!(matches!(tape_end(&[], 0, 0).unwrap(), Value::Str(s) if s.is_empty()));

        // Closing a closed tape answers `missing`: there was nothing to close.
        assert!(matches!(tape_end(&[], 0, 0).unwrap(), Value::Missing));

        // Asking for something else is a divergence, named.
        tape_begin(&[Value::Str(tape)], 0, 0).unwrap();
        through("http_get", 0, 0, || panic!()).unwrap_err();
        let Value::Str(why) = tape_end(&[], 0, 0).unwrap() else { panic!() };
        assert!(why.contains("`http_get`") && why.contains("`now`"), "{why}");
    }
}

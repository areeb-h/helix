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
            | "session_view_path" | "session_program" | "session_accept"
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

/// What identifies one call on the tape: its name and a fingerprint of everything it was
/// handed (the receiver too, for a method). A replay that asks for the same name with other
/// arguments is a divergence at THAT call, not some calls later.
pub struct Call<'a> {
    pub label: &'a str,
    pub recv: Option<&'a Value>,
    pub args: &'a [Value],
}

impl Call<'_> {
    /// The SHA-256 (128 bits, hex) of the arguments' lossless encoding. A value the tape
    /// cannot encode (a function, a frame) contributes its type name: two calls that differ
    /// only inside such a value are not told apart.
    fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let part = |v: &Value| encode(v).unwrap_or_else(|kind| json!({"type": kind}));
        let all = json!([self.recv.map(part), self.args.iter().map(part).collect::<Vec<_>>()]);
        let digest = Sha256::digest(all.to_string().as_bytes());
        digest[..16].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// How the call reads in a message: `name(arg, …)`, each argument shortened.
    fn shown(&self) -> String {
        // Long arguments keep both ends: a path differs in its last component, a URL in its
        // tail, so cutting only the end would hide exactly the part that changed.
        let short = |v: &Value| {
            let t = crate::value::display_value(v, 0, 0).unwrap_or_else(|_| v.type_name().to_string());
            let n = t.chars().count();
            if n <= 60 {
                return t;
            }
            let head: String = t.chars().take(20).collect();
            let tail: String = t.chars().skip(n - 36).collect();
            format!("{head}…{tail}")
        };
        let args: Vec<String> = self.recv.into_iter().chain(self.args.iter()).map(short).collect();
        format!("{}({})", self.label, args.join(", "))
    }
}

/// Run `call` through the tape: record what it returns, or return what was recorded without
/// running it.
pub fn through(
    c: Call<'_>,
    line: usize,
    col: usize,
    call: impl FnOnce() -> Result<Value, HelixError>,
) -> Result<Value, HelixError> {
    let fp = c.fingerprint();
    let replaying = TAPE.with(|t| matches!(*t.borrow(), Tape::Replay { .. }));
    if !replaying {
        let out = call();
        let entry = match &out {
            Ok(v) => match encode(v) {
                Ok(e) => json!({"fn": c.label, "args": fp, "ok": e}),
                Err(kind) => json!({"fn": c.label, "args": fp, "unrecordable": kind}),
            },
            Err(e) => json!({"fn": c.label, "args": fp, "err": e.message, "hint": e.hint}),
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
            let recorded = |e: &J| e.get("fn").and_then(J::as_str).unwrap_or("?").to_string();
            let why = match &e {
                None => Some(format!("the app called `{}`, and the recording has no more calls here", c.shown())),
                Some(e) if recorded(e) != c.label => {
                    Some(format!("the app called `{}` where the recording has a call to `{}`", c.shown(), recorded(e)))
                }
                Some(e) if e.get("args").and_then(J::as_str) != Some(fp.as_str()) => Some(format!(
                    "the app called `{}`, but on the device `{}` was called with other arguments",
                    c.shown(),
                    c.label
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
        return decode(ok).map_err(|m| HelixError::new(format!("replay: bad recorded value for `{}`: {m}", c.label), line, col));
    }
    if let Some(kind) = entry.get("unrecordable").and_then(J::as_str) {
        return Err(HelixError::new(
            format!("replay: `{}` returned a {kind} on the device, which session replay cannot record yet", c.label),
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
pub fn open_for_program(spans: &[crate::module::Span], argv: &[String]) {
    let replay = crate::devwatch::replay_path();
    if replay.is_none() && !crate::devwatch::recording() {
        return;
    }
    // Which program this session is of, so a replay can say whether it is replaying the
    // program that was recorded (reproducing a bug) or another one (testing a change).
    let program = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for s in spans {
            // Every module the program loaded, std.ui included: it is part of what ran. The
            // sources only, in load order, not their file names: a session is sent as
            // `inputs-<date>.helix` but ran as `app.helix`, and it is the same program.
            // (Which module an import names is in the importing source already.)
            h.update((s.source.len() as u64).to_le_bytes());
            h.update(s.source.as_bytes());
        }
        let sha: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        json!({"sha256": sha, "modules": spans.len(), "args": argv, "helix": env!("CARGO_PKG_VERSION")})
    };
    if let Ok(mut p) = PROGRAM.write() {
        *p = Some(program);
    }
    let next = if let Some(path) = replay.map(std::path::PathBuf::from) {
        // The first line of a session log is `{"session": 2, "startup": [ … ]}`. A file that
        // can't be read, or isn't a version-2 session, opens an empty startup tape; std.ui's
        // replay then refuses the file with the reason, before replaying anything.
        let startup = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.lines().next().map(str::to_string))
            .and_then(|l| serde_json::from_str::<J>(&l).ok())
            .filter(|j| j.get("session").and_then(J::as_i64) == Some(SESSION_VERSION))
            .and_then(|j| j.get("startup").and_then(J::as_array).cloned())
            .unwrap_or_default();
        Tape::Replay { queue: startup.into(), diverged: None }
    } else {
        Tape::Record(Vec::new())
    };
    TAPE.with(|t| *t.borrow_mut() = next);
}

/// The running program's identity, set by `open_for_program` in a recorded or replayed run.
/// A `RwLock`, not the tape's thread-local: it describes the process, and is read from
/// whichever thread evaluates.
static PROGRAM: std::sync::RwLock<Option<J>> = std::sync::RwLock::new(None);

/// `session_program()` — in a recorded or replayed run, `{sha256, modules, args, helix}`: a
/// fingerprint of every module the program loaded, the command line `main` was given, and
/// this Helix's version. `missing` otherwise.
pub fn session_program(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if !args.is_empty() {
        return Err(HelixError::new(format!("`session_program` takes no arguments, got {}", args.len()), line, col));
    }
    let p = PROGRAM.read().ok().and_then(|p| p.clone());
    match p {
        None => Ok(Value::Missing),
        Some(j) => crate::json::parse(&j.to_string())
            .map_err(|e| HelixError::new(format!("`session_program`: {e}"), line, col)),
    }
}

/// Why the open tape's replay diverged, if it did. A diverged call fails, and an app may catch
/// that failure (`try`) and raise its own error about it, which would hide the cause: so a
/// replayed program that fails reports this first (`run_file_capture_args`).
pub fn divergence() -> Option<String> {
    TAPE.with(|t| match &*t.borrow() {
        Tape::Replay { diverged, .. } => diverged.clone(),
        _ => None,
    })
}

/// The session-log format this build writes and reads (std.ui writes the header line).
pub const SESSION_VERSION: i64 = 2;

// --- The builtins. -------------------------------------------------------------------------

/// `tape_begin(recorded)` — open the tape. `missing` records; a tape `tape_end` returned
/// (as data, possibly read back from a log with `parse_json`) replays it.
pub fn tape_begin(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.len() != 1 {
        return Err(HelixError::new(format!("`tape_begin` takes one argument, got {}", args.len()), line, col));
    }
    let next = match &args[0] {
        Value::Missing => Tape::Record(Vec::new()),
        v @ Value::Array(_) => {
            // The tape is tagged JSON; a parse_json / to_json round trip leaves it intact,
            // because every kind is carried by its tag, not by how JSON spells numbers.
            let text = crate::json::stringify(v).map_err(|e| HelixError::new(format!("`tape_begin`: {e}"), line, col))?;
            match serde_json::from_str::<J>(&text) {
                Ok(J::Array(v)) => Tape::Replay { queue: v.into(), diverged: None },
                _ => return Err(HelixError::new("`tape_begin`: a recorded tape is a list", line, col)),
            }
        }
        other => {
            return Err(HelixError::new(
                format!("`tape_begin` takes `missing` (record) or a recorded tape (a list), got {}", other.type_name()),
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

/// `tape_end()` — close the tape. Recording: `{tape}`, the recorded calls as data. Replaying:
/// `{diverged}`, `""` when every recorded call was made exactly as recorded, else why not.
/// `missing` when no tape is open (so `ui.app` can close the program's startup tape, if any).
pub fn tape_end(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if !args.is_empty() {
        return Err(HelixError::new(format!("`tape_end` takes no arguments, got {}", args.len()), line, col));
    }
    let was = TAPE.with(|t| std::mem::replace(&mut *t.borrow_mut(), Tape::Off));
    let field = |k: &str, v: Value| Value::Record(Rc::new(vec![(Symbol::intern(k), v)]));
    Ok(match was {
        Tape::Off => Value::Missing,
        Tape::Record(v) => {
            let tape = crate::json::parse(&J::Array(v).to_string())
                .map_err(|e| HelixError::new(format!("`tape_end`: {e}"), line, col))?;
            field("tape", tape)
        }
        Tape::Replay { diverged, queue } => {
            let why = match (diverged, queue.front()) {
                (Some(w), _) => w,
                (None, Some(next)) => format!(
                    "the recording has {} more call(s) here, starting with `{}`, that the app did not make",
                    queue.len(),
                    next.get("fn").and_then(J::as_str).unwrap_or("?")
                ),
                (None, None) => String::new(),
            };
            field("diverged", Value::Str(Rc::new(why)))
        }
    })
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

    fn call<'a>(label: &'a str, args: &'a [Value]) -> Call<'a> {
        Call { label, recv: None, args }
    }

    fn tape_of(v: Value) -> Value {
        match v {
            Value::Record(fs) => fs[0].1.clone(),
            other => panic!("expected a record, got {other:?}"),
        }
    }

    fn diverged(v: Value) -> String {
        match tape_of(v) {
            Value::Str(s) => s.to_string(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    #[test]
    fn replay_returns_recorded_results_without_running() {
        let here = [Value::Str(Rc::new("a.txt".into()))];
        let there = [Value::Str(Rc::new("b.txt".into()))];
        tape_begin(&[Value::Missing], 0, 0).unwrap();
        through(call("now", &[]), 0, 0, || Ok(Value::Float(42.5))).unwrap();
        through(call("read_text", &here), 0, 0, || Err(HelixError::new("no such file", 0, 0).hint("check the path"))).unwrap_err();
        let tape = tape_of(tape_end(&[], 0, 0).unwrap());

        // The tape survives a trip through the log's JSON, as it does on disk.
        let tape = crate::json::parse(&crate::json::stringify(&tape).unwrap()).unwrap();

        tape_begin(std::slice::from_ref(&tape), 0, 0).unwrap();
        let t = through(call("now", &[]), 0, 0, || panic!("replay must not run the builtin")).unwrap();
        assert!(matches!(t, Value::Float(f) if f == 42.5));
        let e = through(call("read_text", &here), 0, 0, || panic!("replay must not run the builtin")).unwrap_err();
        assert_eq!((e.message.as_str(), e.hint.as_deref()), ("no such file", Some("check the path")));
        assert_eq!(diverged(tape_end(&[], 0, 0).unwrap()), "");

        // Closing a closed tape answers `missing`: there was nothing to close.
        assert!(matches!(tape_end(&[], 0, 0).unwrap(), Value::Missing));

        // Another builtin is a divergence, named.
        tape_begin(std::slice::from_ref(&tape), 0, 0).unwrap();
        through(call("http_get", &[]), 0, 0, || panic!()).unwrap_err();
        let why = diverged(tape_end(&[], 0, 0).unwrap());
        assert!(why.contains("`http_get()`") && why.contains("`now`"), "{why}");

        // The same builtin with other arguments is a divergence AT that call.
        tape_begin(std::slice::from_ref(&tape), 0, 0).unwrap();
        through(call("now", &[]), 0, 0, || panic!()).unwrap();
        through(call("read_text", &there), 0, 0, || panic!()).unwrap_err();
        let why = diverged(tape_end(&[], 0, 0).unwrap());
        assert!(why.contains("`read_text(b.txt)`") && why.contains("other arguments"), "{why}");

        // A call the recording has and the app no longer makes is named too.
        tape_begin(&[tape], 0, 0).unwrap();
        through(call("now", &[]), 0, 0, || panic!()).unwrap();
        let why = diverged(tape_end(&[], 0, 0).unwrap());
        assert!(why.contains("1 more call") && why.contains("`read_text`"), "{why}");
    }
}

//! The black box (ADR 0063 D2): an app session recorded so it replays EXACTLY.
//!
//! An app is a fold over events, so its events are most of what a session is. The rest is
//! what `update` and `view` asked the world while handling them: the time, a file, an HTTP
//! response. Those are the results of impure builtins, and every builtin call on the
//! tree-walker and the VM passes one dispatch point (`Interp::call_builtin`). So the tape
//! sits there:
//!
//! - RECORDING, an impure builtin runs as usual and its result is appended to the tape.
//! - REPLAYING, it does NOT run: the next result on the tape is returned instead. A replay
//!   therefore touches nothing — no file is read or written, no request is sent, no clock
//!   is asked — and needs no authority at all.
//!
//! Output that goes nowhere but the terminal (`print`, `elog`) and the program's own
//! refusals and checks (`raise`, `assert…`) are not inputs, so they run either way.
//!
//! The session file is JSON Lines, one entry per step, written as it happens so a crash
//! loses nothing:
//!
//! - `{"kind":"start", "app", "helix", "init"}` — the first model, as the app began;
//! - `{"kind":"event", "event", "effects"}` — one event and what `update` read;
//! - `{"kind":"view", "effects", "screen"}` — what `view` read, and a hash of the HTML it drew.
//!
//! Values are written in a tagged JSON that round-trips Helix's types (an Int stays an Int, a
//! Tuple a Tuple, a record keeps its field order). A value that cannot be written down — a
//! connection, a DataFrame, a function — is recorded as `{"opaque": "<type>"}`, and a replay
//! that needs it says so instead of guessing.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Write;
use std::rc::Rc;

use serde_json::{Map, Value as J, json};

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::{DictKey, Value};

/// What the tape is doing on this thread.
enum Mode {
    Off,
    /// Capturing the impure results of the current step.
    Record(Vec<J>),
    /// Handing back a recorded step's results, in order.
    Replay(VecDeque<J>),
}

thread_local! {
    static MODE: RefCell<Mode> = const { RefCell::new(Mode::Off) };
    static FILE: RefCell<Option<std::fs::File>> = const { RefCell::new(None) };
    static ENTRIES: RefCell<Option<Vec<J>>> = const { RefCell::new(None) };
}

/// Builtins that are not inputs: they run on a replay as they did when recording.
fn passes_through(name: &str) -> bool {
    matches!(
        name,
        "print" | "emit" | "write" | "elog" | "raise" | "assert" | "assert_eq" | "assert_close" | "assert_error"
            // The tape's own controls.
            | "session_tape"
    )
}

/// Whether a builtin call must go through the tape — cheap when no tape is running.
pub fn wants(name: &str) -> bool {
    MODE.with(|m| !matches!(*m.borrow(), Mode::Off)) && crate::registry::is_impure_builtin(name) && !passes_through(name)
}

/// On a replay, the recorded result of this call — the builtin is not run.
pub fn replayed(name: &str, line: usize, col: usize) -> Option<Result<Value, HelixError>> {
    MODE.with(|m| match &mut *m.borrow_mut() {
        Mode::Replay(q) => Some(match q.pop_front() {
            Some(entry) => {
                let recorded = entry.get("fn").and_then(J::as_str).unwrap_or("");
                if recorded != name {
                    Err(HelixError::new(
                        format!("the replay went another way: this step called `{name}` where the recording called `{recorded}`"),
                        line,
                        col,
                    )
                    .hint("the app's code changed since the session was recorded, or it depends on something the tape does not hold (a method on a handle, a value read at the top level)."))
                } else if let Some(msg) = entry.get("error").and_then(J::as_str) {
                    // It failed when recorded, so it fails now, the same way.
                    let e = HelixError::new(msg, line, col);
                    Err(match entry.get("hint").and_then(J::as_str) {
                        Some(h) => e.hint(h),
                        None => e,
                    })
                } else {
                    match entry.get("value") {
                        Some(v) => decode(v).map_err(|why| HelixError::new(format!("`{name}` returned {why} when this session was recorded, so this step cannot be replayed"), line, col)),
                        None => Err(HelixError::new(format!("the recording of `{name}` has no value"), line, col)),
                    }
                }
            }
            None => Err(HelixError::new(
                format!("the replay went another way: this step called `{name}`, and the recording has no more calls for it"),
                line,
                col,
            )
            .hint("the app's code changed since the session was recorded.")),
        }),
        _ => None,
    })
}

/// While recording, note what a builtin returned — or the error it raised, which a replay
/// raises again.
pub fn record(name: &str, out: &Result<Value, HelixError>) {
    MODE.with(|m| {
        if let Mode::Record(t) = &mut *m.borrow_mut() {
            t.push(match out {
                Ok(v) => json!({"fn": name, "value": encode(v)}),
                Err(e) => json!({"fn": name, "error": e.message, "hint": e.hint}),
            });
        }
    });
}

// --- The program's side: `session_tape(op, arg?)`, used only by `std/ui.helix`. -------------

/// `session_tape("mode")` — `"record"`, `"replay"`, or `missing`, from `HELIX_RECORD` /
/// `HELIX_REPLAY` (`helix replay` sets the second).
/// `session_tape("start", info)` — open the recording and write its first entry.
/// `session_tape("begin")` — start capturing one step.
/// `session_tape("event", ev)` / `session_tape("view", screen)` — write the step.
/// `session_tape("entries")` — on a replay, every entry, as values (effects left out).
/// `session_tape("play", i)` — load entry `i`'s effects for the step about to run again.
/// `session_tape("done")` — end a replayed step; `false` if it left recorded calls unused.
pub fn session_tape(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let op = match args.first() {
        Some(Value::Str(s)) => s.as_str().to_string(),
        _ => return Err(HelixError::new("`session_tape` takes an operation name", line, col)),
    };
    let arg = args.get(1).cloned().unwrap_or(Value::Missing);
    let io = |e: std::io::Error| HelixError::new(format!("the session recording failed: {e}"), line, col);
    match op.as_str() {
        "mode" => Ok(match (std::env::var_os("HELIX_REPLAY"), std::env::var_os("HELIX_RECORD")) {
            (Some(_), _) => Value::Str(Rc::new("replay".into())),
            (None, Some(_)) => Value::Str(Rc::new("record".into())),
            _ => Value::Missing,
        }),
        "start" => {
            let Some(path) = std::env::var_os("HELIX_RECORD") else { return Ok(Value::Bool(false)) };
            let f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&path).map_err(io)?;
            FILE.with(|c| *c.borrow_mut() = Some(f));
            let mut entry = Map::new();
            entry.insert("kind".into(), json!("start"));
            entry.insert("helix".into(), json!(env!("CARGO_PKG_VERSION")));
            if let Value::Record(r) = &arg {
                for (k, v) in r.iter() {
                    entry.insert(k.as_str().to_string(), encode(v));
                }
            }
            write_line(&J::Object(entry)).map_err(io)?;
            Ok(Value::Bool(true))
        }
        "begin" => {
            MODE.with(|m| {
                let mut m = m.borrow_mut();
                if !matches!(*m, Mode::Replay(_)) {
                    *m = Mode::Record(Vec::new());
                }
            });
            Ok(Value::Unit)
        }
        "event" | "view" => {
            let effects = MODE.with(|m| match std::mem::replace(&mut *m.borrow_mut(), Mode::Off) {
                Mode::Record(t) => t,
                _ => Vec::new(),
            });
            let entry = if op == "event" {
                // `{event, error}`: the event, and what `update` raised on it, if anything.
                let field = |name: &str| match &arg {
                    Value::Record(r) => r.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| v.clone()).unwrap_or(Value::Missing),
                    _ => Value::Missing,
                };
                json!({"kind": "event", "event": encode(&field("event")), "error": encode(&field("error")), "effects": effects})
            } else {
                json!({"kind": "view", "screen": encode(&arg), "effects": effects})
            };
            write_line(&entry).map_err(io)?;
            Ok(Value::Unit)
        }
        "entries" => {
            let path = std::env::var_os("HELIX_REPLAY").ok_or_else(|| HelixError::new("no session to replay (HELIX_REPLAY is not set)", line, col))?;
            let text = std::fs::read_to_string(&path).map_err(|e| HelixError::new(format!("cannot read the session `{}`: {e}", std::path::Path::new(&path).display()), line, col))?;
            let mut parsed = Vec::new();
            for (i, l) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
                let j: J = serde_json::from_str(l).map_err(|e| HelixError::new(format!("line {} of the session is not JSON: {e}", i + 1), line, col))?;
                parsed.push(j);
            }
            let values = parsed
                .iter()
                .map(|j| {
                    let mut out: Vec<(Symbol, Value)> = Vec::new();
                    if let J::Object(o) = j {
                        for (k, v) in o {
                            if k == "effects" {
                                out.push((Symbol::intern("calls"), Value::Int(v.as_array().map_or(0, |a| a.len()) as i64)));
                            } else {
                                out.push((Symbol::intern(k), decode(v).unwrap_or(Value::Missing)));
                            }
                        }
                    }
                    Value::Record(Rc::new(out))
                })
                .collect();
            ENTRIES.with(|e| *e.borrow_mut() = Some(parsed));
            Ok(Value::array(values))
        }
        "play" => {
            let i = match arg {
                Value::Int(i) if i >= 0 => i as usize,
                _ => return Err(HelixError::new("`session_tape(\"play\", i)` needs an entry number", line, col)),
            };
            let effects = ENTRIES.with(|e| {
                e.borrow().as_ref().and_then(|v| v.get(i)).and_then(|j| j.get("effects")).and_then(J::as_array).cloned().unwrap_or_default()
            });
            MODE.with(|m| *m.borrow_mut() = Mode::Replay(effects.into()));
            Ok(Value::Unit)
        }
        "done" => Ok(Value::Bool(MODE.with(|m| match std::mem::replace(&mut *m.borrow_mut(), Mode::Off) {
            Mode::Replay(q) => q.is_empty(),
            _ => true,
        }))),
        other => Err(HelixError::new(format!("`session_tape` has no operation `{other}`"), line, col)),
    }
}

fn write_line(j: &J) -> std::io::Result<()> {
    FILE.with(|c| match c.borrow_mut().as_mut() {
        Some(f) => {
            let mut line = serde_json::to_string(j).map_err(std::io::Error::other)?;
            line.push('\n');
            f.write_all(line.as_bytes())?;
            f.flush()
        }
        None => Ok(()),
    })
}

// --- Values, written down exactly. -------------------------------------------------------

/// A Helix value as tagged JSON. Plain JSON where it is exact — strings, booleans, integers,
/// arrays, `missing` as `null` — and a one-key object where it is not.
pub fn encode(v: &Value) -> J {
    match v {
        Value::Missing => J::Null,
        Value::Unit => json!({"unit": true}),
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => json!(i),
        // A float is tagged, so `1.0` comes back a Float and NaN and the infinities survive.
        Value::Float(f) => json!({"float": f.to_string()}),
        Value::Str(s) => J::String(s.to_string()),
        Value::Dna(s) => json!({"dna": s.as_str()}),
        Value::Bytes(b) => json!({"bytes": b.iter().map(|x| format!("{x:02x}")).collect::<String>()}),
        Value::Array(a) => J::Array(a.to_values().iter().map(encode).collect()),
        Value::Tuple(t) => json!({"tuple": t.iter().map(encode).collect::<Vec<_>>()}),
        Value::Record(r) => json!({"record": r.iter().map(|(k, v)| json!([k.as_str(), encode(v)])).collect::<Vec<_>>()}),
        Value::Dict(d) => json!({"dict": d.map().iter().map(|(k, v)| json!([encode(&k.to_value()), encode(v)])).collect::<Vec<_>>()}),
        Value::Headers(h) => json!({"headers": h.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()}),
        other => json!({"opaque": other.type_name()}),
    }
}

/// The value `encode` wrote; `Err` names what could not be written down.
pub fn decode(j: &J) -> Result<Value, String> {
    Ok(match j {
        J::Null => Value::Missing,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        J::String(s) => Value::Str(Rc::new(s.clone())),
        J::Array(a) => Value::array(a.iter().map(decode).collect::<Result<_, _>>()?),
        J::Object(o) => {
            let (tag, body) = o.iter().next().ok_or("an empty object")?;
            match tag.as_str() {
                "unit" => Value::Unit,
                // Rust's shortest round-trip form, which also spells NaN and the infinities.
                "float" => {
                    let s = body.as_str().unwrap_or("");
                    Value::Float(s.parse().map_err(|_| format!("a float `{s}` that does not parse"))?)
                }
                "dna" => Value::Dna(Rc::new(body.as_str().unwrap_or("").to_string())),
                "bytes" => {
                    let h = body.as_str().unwrap_or("");
                    let bytes = (0..h.len() / 2).map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).map_err(|_| "bytes that are not hex".to_string())).collect::<Result<Vec<_>, _>>()?;
                    Value::Bytes(Rc::new(bytes))
                }
                "tuple" => Value::Tuple(Rc::new(body.as_array().ok_or("a malformed tuple")?.iter().map(decode).collect::<Result<_, _>>()?)),
                "record" => {
                    let mut out = Vec::new();
                    for pair in body.as_array().ok_or("a malformed record")? {
                        let k = pair.get(0).and_then(J::as_str).ok_or("a malformed record field")?;
                        out.push((Symbol::intern(k), decode(pair.get(1).unwrap_or(&J::Null))?));
                    }
                    Value::Record(Rc::new(out))
                }
                "dict" => {
                    let mut m = std::collections::BTreeMap::new();
                    for pair in body.as_array().ok_or("a malformed dict")? {
                        let k = decode(pair.get(0).unwrap_or(&J::Null))?;
                        let k = DictKey::from_value(&k)?;
                        m.insert(k, decode(pair.get(1).unwrap_or(&J::Null))?);
                    }
                    Value::Dict(Rc::new(crate::value::Dict::from_map(m)))
                }
                "headers" => {
                    let mut out = Vec::new();
                    for pair in body.as_array().ok_or("malformed headers")? {
                        let k = pair.get(0).and_then(J::as_str).unwrap_or("").to_string();
                        let v = pair.get(1).and_then(J::as_str).unwrap_or("").to_string();
                        out.push((k, v));
                    }
                    Value::Headers(Rc::new(out))
                }
                "opaque" => return Err(format!("{} (a value that cannot be written down)", crate::value::with_article(body.as_str().unwrap_or("value")))),
                other => return Err(format!("an unknown kind of value `{other}`")),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_exactly() {
        let s = |x: &str| Value::Str(Rc::new(x.into()));
        let v = Value::Record(Rc::new(vec![
            (Symbol::intern("n"), Value::Int(3)),
            (Symbol::intern("f"), Value::Float(1.0)),
            (Symbol::intern("nan"), Value::Float(f64::NAN)),
            (Symbol::intern("t"), Value::Tuple(Rc::new(vec![s("a"), Value::Missing]))),
            (Symbol::intern("xs"), Value::array(vec![Value::Bool(true), Value::Int(-7)])),
            (Symbol::intern("u"), Value::Unit),
        ]));
        let back = decode(&encode(&v)).unwrap();
        assert_eq!(format!("{back:?}"), format!("{v:?}"));
        // An Int and a Float stay apart; a record keeps its field order.
        assert!(matches!(decode(&encode(&Value::Float(2.0))).unwrap(), Value::Float(_)));
        assert!(matches!(decode(&encode(&Value::Int(2))).unwrap(), Value::Int(2)));
        let opaque = json!({"opaque": "DataFrame"});
        assert!(decode(&opaque).unwrap_err().contains("cannot be written down"));
    }
}

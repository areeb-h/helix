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
    /// Capturing the impure results of the current step (or of the program's startup).
    Record(Vec<J>),
    /// Handing back a recorded step's results, in order.
    Replay(VecDeque<J>),
}

thread_local! {
    static MODE: RefCell<Mode> = const { RefCell::new(Mode::Off) };
    static FILE: RefCell<Option<std::fs::File>> = const { RefCell::new(None) };
    static ENTRIES: RefCell<Option<Vec<J>>> = const { RefCell::new(None) };
    /// The first call a replay could not answer, and why. Kept once set: a diverged call
    /// fails, and the app may catch that and raise its own error, which would hide the cause.
    static DIVERGED: RefCell<Option<String>> = const { RefCell::new(None) };
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

fn set_mode(mode: Mode) {
    SEGMENT_BYTES.set(0);
    SEGMENT_FULL.set(false);
    MODE.with(|m| *m.borrow_mut() = mode);
}

thread_local! {
    /// What the calls taped since the mode was last set (the startup, or one step) take written
    /// down. A step's calls are held until the step is written, and the startup's until
    /// `ui.app` writes the session's first line — which a program that never starts an app
    /// never does — so without a bound a recorded program held every file it read.
    static SEGMENT_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The segment passed [`TAPE_BYTES`]: its overflow has been noted, and nothing more is held.
    static SEGMENT_FULL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn running() -> bool {
    // Constant folding (ADR 0050) tries calls while the program loads and drops any that turn
    // out impure. Those attempts are the compiler's, not the program's: they never reach the
    // world, and taping them would make a recording depend on what the folder tried.
    !crate::fold::sandbox_active() && MODE.with(|m| !matches!(*m.borrow(), Mode::Off))
}

/// Whether a builtin call must go through the tape — cheap when no tape is running.
pub fn wants(name: &str) -> bool {
    running() && crate::registry::is_impure_builtin(name) && !passes_through(name)
}

/// Whether a method call must go through the tape: one that writes (a file, a database). A
/// replay must not write, and returns what the write returned when the session was recorded.
/// The runtime's own socket methods (`accept`, `respond`, …) are outside the tape: they are
/// how the app is served, not what it reads.
pub fn wants_method(name: &str) -> bool {
    use crate::capability::Effect;
    running() && matches!(crate::capability::method_effect_of(name), Effect::FsWrite | Effect::DbWrite)
}

/// One call, as the tape identifies it: its name, and everything it was handed (the receiver
/// too, for a method).
pub struct Call<'a> {
    pub label: &'a str,
    pub recv: Option<&'a Value>,
    pub args: &'a [Value],
}

impl Call<'_> {
    /// SHA-256 (128 bits, hex) of the arguments, by their structure. A replay that makes the
    /// same call with other arguments (another file, another URL) has gone another way AT that
    /// call, and is stopped there rather than handed the answer to a different question.
    ///
    /// It is a hash of the value, not of its text: written down, a value that holds one part
    /// many times is as large as the tree it stands for (sixty doublings are 2^60 leaves), and
    /// writing it down to hash it ran the recording out of memory. Here each container's digest
    /// is made from its children's and remembered by address for this one fingerprint, so a
    /// shared part is hashed once and the cost is what the value takes in memory. Equal values
    /// have equal digests however they are shared, so a replay that rebuilt the same arguments
    /// another way still matches.
    fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut memo = rustc_hash::FxHashMap::default();
        let mut h = Sha256::new();
        h.update(b"helix tape arguments 2\0");
        match self.recv {
            Some(r) => {
                h.update([1]);
                h.update(digest(r, &mut memo));
            }
            None => h.update([0]),
        }
        h.update((self.args.len() as u64).to_le_bytes());
        for a in self.args {
            h.update(digest(a, &mut memo));
        }
        h.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// How the call reads in a message: `name(arg, …)`, each argument shortened at its middle
    /// (a path differs in its last part, a URL in its tail).
    fn shown(&self) -> String {
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

/// A value's SHA-256 by its structure (see [`Call::fingerprint`]). `memo` maps a container's
/// address to its digest for the length of one fingerprint; the value is borrowed throughout,
/// so no address in it can be freed and reused meanwhile.
fn digest(v: &Value, memo: &mut rustc_hash::FxHashMap<usize, [u8; 32]>) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let at = crate::deep::node(v).map(|n| n.key);
    if let Some(d) = at.and_then(|k| memo.get(&k)) {
        return *d;
    }
    fn bytes(h: &mut Sha256, tag: u8, b: &[u8]) {
        h.update([tag]);
        h.update((b.len() as u64).to_le_bytes());
        h.update(b);
    }
    // A child that is a container is a step deeper: it gets its own stack segment if it needs
    // one, so a value nested a million deep is hashed as surely as it was built.
    fn child(h: &mut Sha256, x: &Value, memo: &mut rustc_hash::FxHashMap<usize, [u8; 32]>) {
        let d = if crate::deep::is_container(x) { crate::deep::nested(|| digest(x, memo)) } else { digest(x, memo) };
        h.update(d);
    }
    let mut h = Sha256::new();
    match v {
        Value::Missing => h.update([0]),
        Value::Unit => h.update([1]),
        Value::Bool(b) => h.update([2, u8::from(*b)]),
        Value::Int(i) => {
            h.update([3]);
            h.update(i.to_le_bytes());
        }
        Value::Float(f) => {
            h.update([4]);
            h.update(f.to_bits().to_le_bytes());
        }
        Value::Str(s) => bytes(&mut h, 5, s.as_bytes()),
        Value::Dna(s) => bytes(&mut h, 6, s.as_bytes()),
        Value::Bytes(b) => bytes(&mut h, 7, b),
        Value::Array(a) => {
            let items = a.to_values();
            h.update([8]);
            h.update((items.len() as u64).to_le_bytes());
            for x in items.iter() {
                child(&mut h, x, memo);
            }
        }
        Value::Tuple(t) => {
            h.update([9]);
            h.update((t.len() as u64).to_le_bytes());
            for x in t.iter() {
                child(&mut h, x, memo);
            }
        }
        Value::Record(r) => {
            h.update([10]);
            h.update((r.len() as u64).to_le_bytes());
            for (k, x) in r.iter() {
                bytes(&mut h, 5, k.as_str().as_bytes());
                child(&mut h, x, memo);
            }
        }
        Value::Dict(d) => {
            let m = d.map();
            h.update([11]);
            h.update((m.len() as u64).to_le_bytes());
            for (k, x) in m.iter() {
                child(&mut h, &k.to_value(), memo);
                child(&mut h, x, memo);
            }
        }
        Value::Headers(hs) => {
            h.update([12]);
            h.update((hs.len() as u64).to_le_bytes());
            for (k, x) in hs.iter() {
                bytes(&mut h, 5, k.as_bytes());
                bytes(&mut h, 5, x.as_bytes());
            }
        }
        // What the tape writes for it, too: a value that cannot be written down is its kind.
        other => bytes(&mut h, 13, other.type_name().as_bytes()),
    }
    let d: [u8; 32] = h.finalize().into();
    if let Some(k) = at {
        memo.insert(k, d);
    }
    d
}

/// Run `call` through the tape: while recording, run it and note what it returned (or the
/// error it raised, which a replay raises again); while replaying, do NOT run it and return
/// what was recorded.
pub fn through(c: Call<'_>, line: usize, col: usize, call: impl FnOnce() -> Result<Value, HelixError>) -> Result<Value, HelixError> {
    let replaying = MODE.with(|m| matches!(*m.borrow(), Mode::Replay(_)));
    if !replaying {
        let out = call();
        if SEGMENT_FULL.get() {
            return out;
        }
        let fp = c.fingerprint();
        let (entry, size) = match &out {
            Ok(v) => {
                let (j, size) = encode_sized(v);
                (json!({"fn": c.label, "args": fp, "value": j}), size)
            }
            Err(e) => (json!({"fn": c.label, "args": fp, "error": e.message, "hint": e.hint}), (e.message.len() + 256) as u64),
        };
        let used = SEGMENT_BYTES.get().saturating_add(size);
        MODE.with(|m| {
            if let Mode::Record(t) = &mut *m.borrow_mut() {
                if used > TAPE_BYTES {
                    // Noted once, at the call that passed the bound; a replay stops there.
                    SEGMENT_FULL.set(true);
                    let why = format!("the recording stopped taping at `{}`: what this part of the run read passed {} MiB, more than a session holds", c.label, TAPE_BYTES >> 20);
                    t.push(json!({"fn": c.label, "args": fp, "overflow": why}));
                } else {
                    SEGMENT_BYTES.set(used);
                    t.push(entry);
                }
            }
        });
        return out;
    }
    let went_another_way = |why: String| {
        DIVERGED.with(|d| {
            let mut d = d.borrow_mut();
            if d.is_none() {
                *d = Some(why.clone());
            }
        });
        HelixError::new(format!("the replay went another way: {why}"), line, col)
            .hint("the app's code or its arguments changed since the session was recorded, or it depends on something the tape does not hold.")
    };
    // After a divergence nothing more is answered: every later call says why.
    if let Some(why) = DIVERGED.with(|d| d.borrow().clone()) {
        return Err(went_another_way(why));
    }
    let entry = MODE.with(|m| match &mut *m.borrow_mut() {
        Mode::Replay(q) => q.pop_front(),
        _ => None,
    });
    let Some(entry) = entry else {
        return Err(went_another_way(format!("the app called `{}`, and the recording has no more calls here", c.shown())));
    };
    let recorded = entry.get("fn").and_then(J::as_str).unwrap_or("");
    if recorded != c.label {
        return Err(went_another_way(format!("the app called `{}` where the recording called `{recorded}`", c.shown())));
    }
    // Recordings made before calls were fingerprinted have no `args`; their names still match.
    if let Some(fp) = entry.get("args").and_then(J::as_str)
        && fp != c.fingerprint()
    {
        return Err(went_another_way(format!("the app called `{}`, but when recorded `{}` was called with other arguments", c.shown(), c.label)));
    }
    if let Some(why) = entry.get("overflow").and_then(J::as_str) {
        // The recording held no more of this part of the run, so nothing after here can be
        // answered: every later call says the same.
        return Err(went_another_way(why.to_string()));
    }
    if let Some(msg) = entry.get("error").and_then(J::as_str) {
        // It failed when recorded, so it fails now, the same way.
        let e = HelixError::new(msg, line, col);
        return Err(match entry.get("hint").and_then(J::as_str) {
            Some(h) => e.hint(h),
            None => e,
        });
    }
    match entry.get("value") {
        Some(v) => decode(v).map_err(|why| HelixError::new(format!("`{}` returned {why} when this session was recorded, so this step cannot be replayed", c.label), line, col)),
        None => Err(HelixError::new(format!("the recording of `{}` has no value", c.label), line, col)),
    }
}

/// Whether this process read its session (`session_tape("entries")`), which is what replaying
/// one is. A program run by `helix replay` that never started an app read none, and must not
/// be reported as a session replayed with every screen identical.
static SESSION_READ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `helix replay` ran a program that finished without replaying the session: it never
/// started an app (`ui.app`).
pub fn replay_never_started() -> bool {
    std::env::var_os("HELIX_REPLAY").is_some() && !SESSION_READ.load(std::sync::atomic::Ordering::Relaxed)
}

/// Why the replay went another way, if it did.
pub fn divergence() -> Option<String> {
    DIVERGED.with(|d| d.borrow().clone())
}

// --- The whole program: startup and identity. ---------------------------------------------

/// The running program's identity in a recorded or replayed run: a fingerprint of every
/// module it loaded, its command line, and this Helix. A `RwLock`, not a thread-local: it
/// describes the process.
static PROGRAM: std::sync::RwLock<Option<J>> = std::sync::RwLock::new(None);

/// Start the tape with the program, before any of it runs, when a session is recorded or
/// replayed. Without this, what an app does before `ui.app` (`main` reading a folder, a
/// first-run migration) reads and writes the replaying machine's world. Recording, those calls
/// go on the session's `start` entry as its `effects`; replaying, they are answered from it.
///
/// Called by `helix run` on the evaluating thread (the tape is thread-local).
pub fn begin_program(spans: &[crate::module::Span], argv: &[String]) {
    let replay = std::env::var_os("HELIX_REPLAY");
    if replay.is_none() && std::env::var_os("HELIX_RECORD").is_none() {
        return;
    }
    // Which program this is, so a replay can say whether it reproduces the recorded program
    // or tests a changed one. The sources in load order, std.ui included (it is part of what
    // ran), but not their file names: a session sent as `stopwatch-0611.helix` that ran as
    // `app.helix` is the same program.
    let program = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for s in spans {
            h.update((s.source.len() as u64).to_le_bytes());
            h.update(s.source.as_bytes());
        }
        let sha: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        json!({"sha256": sha, "modules": spans.len(), "args": argv, "helix": env!("CARGO_PKG_VERSION")})
    };
    if let Ok(mut p) = PROGRAM.write() {
        *p = Some(program);
    }
    let mode = match replay {
        // A session that cannot be read here replays an empty startup; `session_tape("entries")`
        // then refuses the file with the reason.
        Some(path) => Mode::Replay(
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| t.lines().find(|l| !l.trim().is_empty()).map(str::to_string))
                .and_then(|l| serde_json::from_str::<J>(&l).ok())
                .filter(|j| j.get("kind").and_then(J::as_str) == Some("start"))
                .and_then(|j| j.get("effects").and_then(J::as_array).cloned())
                .unwrap_or_default()
                .into(),
        ),
        None => Mode::Record(Vec::new()),
    };
    set_mode(mode);
}

fn program() -> Option<J> {
    PROGRAM.read().ok().and_then(|p| p.clone())
}

// --- The program's side: `session_tape(op, arg?)`, used only by `std/ui.helix`. -------------

/// `session_tape("mode")` — `"record"`, `"replay"`, or `missing`, from `HELIX_RECORD` /
/// `HELIX_REPLAY` (`helix replay` sets the second).
/// `session_tape("start", info)` — open the recording and write its first entry, with what
/// the program's startup read (`effects`) and which program it is (`program`).
/// `session_tape("started")` — on a replay, end the startup: `true` if it made exactly the
/// recorded calls.
/// `session_tape("program")` — this run's `{sha256, modules, args, helix}`, or `missing`.
/// `session_tape("options")` — `{view, accept}`: whether `helix replay --view/--accept` asked.
/// `session_tape("viewer", page)` — write the viewer page to the `--view` path; its path.
/// `session_tape("accept", screens)` — rewrite the session with these screens and this program.
/// `session_tape("diverged")` — why the replay went another way, or `missing`.
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
            // The startup is over: what it read goes on this entry.
            let startup = MODE.with(|m| match std::mem::replace(&mut *m.borrow_mut(), Mode::Off) {
                Mode::Record(t) => t,
                _ => Vec::new(),
            });
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
            if let Some(p) = program() {
                entry.insert("program".into(), p);
            }
            entry.insert("effects".into(), J::Array(startup));
            write_line(&J::Object(entry)).map_err(io)?;
            Ok(Value::Bool(true))
        }
        "started" => {
            let left = MODE.with(|m| match std::mem::replace(&mut *m.borrow_mut(), Mode::Off) {
                Mode::Replay(q) => q.len(),
                _ => 0,
            });
            if left > 0 && divergence().is_none() {
                DIVERGED.with(|d| *d.borrow_mut() = Some(format!("the app made fewer calls before it started than when recorded ({left} left unanswered)")));
            }
            Ok(Value::Bool(divergence().is_none()))
        }
        // `--view` and `--accept`, which `helix replay` passes to the replaying run.
        "options" => {
            let view = std::env::var_os("HELIX_REPLAY_VIEW").is_some_and(|v| !v.is_empty());
            Ok(Value::record(vec![
                (Symbol::intern("view"), Value::Bool(view)),
                (Symbol::intern("accept"), Value::Bool(std::env::var_os("HELIX_REPLAY_ACCEPT").is_some())),
            ]))
        }
        // `--view`: write the viewer page where the person running `helix replay` asked. The
        // program hands over the page and never names the file: an app that is replayed must
        // not need, or be inferred to need, permission to write files.
        "viewer" => {
            let Value::Str(page) = &arg else {
                return Err(HelixError::new("`session_tape(\"viewer\", page)` takes the page as a string", line, col));
            };
            let path = std::env::var_os("HELIX_REPLAY_VIEW").filter(|v| !v.is_empty()).ok_or_else(|| HelixError::new("no viewer was asked for (`helix replay --view`)", line, col))?;
            std::fs::write(&path, page.as_bytes()).map_err(io)?;
            Ok(Value::Str(Rc::new(std::path::Path::new(&path).display().to_string())))
        }
        // `--accept`: this program's screens become the recorded ones. Only the screens and
        // the program: events and effects are what happened, and are kept byte for byte.
        "accept" => {
            let screens: Vec<String> = match &arg {
                Value::Array(a) => a.to_values().iter().map(|v| match v {
                    Value::Str(s) => Ok(s.to_string()),
                    _ => Err(()),
                }).collect::<Result<_, _>>().map_err(|()| HelixError::new("`session_tape(\"accept\", screens)` takes the screens as strings", line, col))?,
                _ => return Err(HelixError::new("`session_tape(\"accept\", screens)` takes a list of screens", line, col)),
            };
            let path = std::env::var_os("HELIX_REPLAY").ok_or_else(|| HelixError::new("no session to accept (HELIX_REPLAY is not set)", line, col))?;
            let mut entries = ENTRIES.with(|e| e.borrow().clone()).ok_or_else(|| HelixError::new("the session has not been read", line, col))?;
            let views = entries.iter().filter(|j| j.get("kind").and_then(J::as_str) == Some("view")).count();
            if views != screens.len() {
                return Err(HelixError::new(format!("the session has {views} screens and the replay drew {}", screens.len()), line, col));
            }
            let mut next = screens.into_iter();
            for j in &mut entries {
                let Some(o) = j.as_object_mut() else { continue };
                match o.get("kind").and_then(J::as_str) {
                    Some("view") => {
                        if let Some(h) = next.next() {
                            o.insert("screen".into(), J::String(h));
                        }
                    }
                    Some("start") => {
                        if let Some(p) = program() {
                            o.insert("program".into(), p);
                        }
                    }
                    _ => {}
                }
            }
            let mut text = String::new();
            for j in &entries {
                text.push_str(&serde_json::to_string(j).map_err(|e| HelixError::new(format!("{e}"), line, col))?);
                text.push('\n');
            }
            // Written beside it, then renamed over it: a crash leaves the old session whole.
            let path = std::path::PathBuf::from(path);
            let tmp = path.with_extension("jsonl.accepting");
            std::fs::write(&tmp, text).map_err(io)?;
            std::fs::rename(&tmp, &path).map_err(io)?;
            Ok(Value::Unit)
        }
        "program" => Ok(match program() {
            Some(p) => decode_plain(&p),
            None => Value::Missing,
        }),
        "diverged" => Ok(match divergence() {
            Some(w) => Value::Str(Rc::new(w)),
            None => Value::Missing,
        }),
        "begin" => {
            if !MODE.with(|m| matches!(*m.borrow(), Mode::Replay(_))) {
                set_mode(Mode::Record(Vec::new()));
            }
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
                parsed.push((i + 1, j));
            }
            // A replay that compares nothing must not report that everything matched: an empty
            // or truncated file in a corpus would pass the gate it is in.
            let name = std::path::Path::new(&path).display().to_string();
            let not_a_session = |why: String| {
                HelixError::new(format!("`{name}` is not a recorded session: {why}"), line, col).hint("record one with `HELIX_RECORD=session.jsonl helix run app.helix`")
            };
            let kind = |j: &J| j.get("kind").and_then(J::as_str).map(str::to_string);
            match parsed.first() {
                None => return Err(not_a_session("it is empty".into())),
                Some((n, j)) if kind(j).as_deref() != Some("start") => {
                    return Err(not_a_session(format!("its first entry (line {n}) is not the session's start, `{{\"kind\": \"start\", …}}`")));
                }
                _ => {}
            }
            if let Some((n, _)) = parsed.iter().find(|(_, j)| !matches!(kind(j).as_deref(), Some("start" | "event" | "view"))) {
                return Err(not_a_session(format!("line {n} is not an entry the black box writes (it has no `kind` of start, event or view)")));
            }
            if !parsed.iter().any(|(_, j)| kind(j).as_deref() == Some("view")) {
                return Err(not_a_session("it holds no screen to compare: no event reached the app while it was recorded".into()));
            }
            let parsed: Vec<J> = parsed.into_iter().map(|(_, j)| j).collect();
            SESSION_READ.store(true, std::sync::atomic::Ordering::Relaxed);
            let values = parsed
                .iter()
                .map(|j| {
                    let mut out: Vec<(Symbol, Value)> = Vec::new();
                    if let J::Object(o) = j {
                        for (k, v) in o {
                            if k == "effects" {
                                let calls = v.as_array().map(Vec::as_slice).unwrap_or(&[]);
                                out.push((Symbol::intern("calls"), Value::Int(calls.len() as i64)));
                                // What each call was and whether it answered, for the viewer:
                                // not the values, which may be large or private.
                                let reads = calls
                                    .iter()
                                    .map(|c| {
                                        let name = c.get("fn").and_then(J::as_str).unwrap_or("?");
                                        Value::record(vec![
                                            (Symbol::intern("fn"), Value::Str(Rc::new(name.into()))),
                                            (Symbol::intern("ok"), Value::Bool(c.get("error").is_none())),
                                        ])
                                    })
                                    .collect();
                                out.push((Symbol::intern("reads"), Value::array(reads)));
                            } else if k == "program" {
                                out.push((Symbol::intern(k), decode_plain(v)));
                            } else {
                                out.push((Symbol::intern(k), decode(v).unwrap_or(Value::Missing)));
                            }
                        }
                    }
                    Value::record(out)
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
            set_mode(Mode::Replay(effects.into()));
            Ok(Value::Unit)
        }
        "done" => Ok(Value::Bool(MODE.with(|m| match std::mem::replace(&mut *m.borrow_mut(), Mode::Off) {
            Mode::Replay(q) => q.is_empty(),
            _ => true,
        }))),
        other => Err(HelixError::new(format!("`session_tape` has no operation `{other}`"), line, col)),
    }
}

/// Plain JSON (not the tape's tagged form) as a Helix value: the program fingerprint is
/// written as ordinary JSON so a person reading the session file can read it.
fn decode_plain(j: &J) -> Value {
    crate::json::parse(&j.to_string()).unwrap_or(Value::Missing)
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

/// How deep, in JSON levels, a value may be written on the tape. A session is read back a line
/// at a time with serde_json's limit of 128 levels, and a value sits a few levels inside its
/// line (`{"effects": [{"value": …}]}`): a deeper value made the WHOLE session unreadable, not
/// only its step. A record level is three JSON levels (`{"record": [[k, v]]}`), so this is 33
/// records deep, or 100 arrays.
const TAPE_DEPTH: usize = 100;

/// How much one value may take written down, counting each JSON node at what it costs in
/// memory while its line is built. Past this a value is noted, not written: a session is a bug
/// report, and a step that read a quarter of a gigabyte is reproduced by the file, not the tape.
const TAPE_BYTES: u64 = 256 << 20;

/// A Helix value as tagged JSON. Plain JSON where it is exact — strings, booleans, integers,
/// arrays, `missing` as `null` — and a one-key object where it is not.
///
/// A value too deep or too large to write down is written as `{"unrecorded": why}`, and is
/// measured before anything is built: a value that holds one part many times is small in
/// memory and astronomically large written down, and building it first is what ran the
/// recording out of memory. The live run is unaffected; a replay that needs the value refuses
/// at that step and says why.
pub fn encode(v: &Value) -> J {
    encode_sized(v).0
}

/// [`encode`], and what the result takes written down.
fn encode_sized(v: &Value) -> (J, u64) {
    let mut memo = rustc_hash::FxHashMap::default();
    match written_size(v, 0, &mut memo) {
        Ok((size, _)) => (encode_unchecked(v), size),
        Err(why) => {
            let n = why.len() as u64 + 64;
            (json!({"unrecorded": why}), n)
        }
    }
}

/// What a value takes written down — bytes, roughly, counting each JSON node as a small fixed
/// cost — or why it cannot be. Remembered per container by address, with the levels it spans,
/// so a shared part is measured once and the walk is as large as the value is in memory. The
/// walk stops at [`TAPE_DEPTH`], so its own recursion is bounded too.
fn written_size(v: &Value, depth: usize, memo: &mut rustc_hash::FxHashMap<usize, (u64, usize)>) -> Result<(u64, usize), String> {
    const NODE: u64 = 64;
    let too_deep = || format!("a value nested deeper than a session holds ({TAPE_DEPTH} levels written down)");
    let at = crate::deep::node(v).map(|n| n.key);
    if let Some(&(size, levels)) = at.and_then(|k| memo.get(&k)) {
        return if depth + levels > TAPE_DEPTH { Err(too_deep()) } else { Ok((size, levels)) };
    }
    // The JSON levels a container opens before its elements: `[`; `{"tuple": [`; `{"record": [[`.
    let open = match v {
        Value::Array(_) => 1,
        Value::Tuple(_) => 2,
        Value::Record(_) | Value::Dict(_) | Value::Headers(_) => 3,
        _ => 0,
    };
    if depth + open > TAPE_DEPTH {
        return Err(too_deep());
    }
    let mut size = NODE;
    let mut levels = open.max(1);
    let add = |x: &Value, size: &mut u64, levels: &mut usize, memo: &mut rustc_hash::FxHashMap<usize, (u64, usize)>| -> Result<(), String> {
        let (s, l) = written_size(x, depth + open, memo)?;
        *size = size.saturating_add(s);
        *levels = (*levels).max(open + l);
        Ok(())
    };
    match v {
        Value::Str(s) | Value::Dna(s) => size += s.len() as u64 + 2,
        Value::Bytes(b) => size += 2 * b.len() as u64,
        Value::Float(_) => size += 32,
        Value::Array(a) => {
            for x in a.to_values().iter() {
                add(x, &mut size, &mut levels, memo)?;
                if size > TAPE_BYTES {
                    break;
                }
            }
        }
        Value::Tuple(t) => {
            for x in t.iter() {
                add(x, &mut size, &mut levels, memo)?;
                if size > TAPE_BYTES {
                    break;
                }
            }
        }
        Value::Record(r) => {
            for (k, x) in r.iter() {
                size += k.as_str().len() as u64 + NODE;
                add(x, &mut size, &mut levels, memo)?;
                if size > TAPE_BYTES {
                    break;
                }
            }
        }
        Value::Dict(d) => {
            for (k, x) in d.map().iter() {
                add(&k.to_value(), &mut size, &mut levels, memo)?;
                add(x, &mut size, &mut levels, memo)?;
                if size > TAPE_BYTES {
                    break;
                }
            }
        }
        Value::Headers(hs) => {
            for (k, x) in hs.iter() {
                size += (k.len() + x.len()) as u64 + 2 * NODE;
            }
        }
        _ => {}
    }
    if size > TAPE_BYTES {
        return Err(format!("a value too large for a session (over {} MiB written down)", TAPE_BYTES >> 20));
    }
    if let Some(k) = at {
        memo.insert(k, (size, levels));
    }
    Ok((size, levels))
}

/// [`encode`] without the measuring: only for a value [`written_size`] accepted.
fn encode_unchecked(v: &Value) -> J {
    let encode = encode_unchecked;
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
                "tuple" => Value::tuple(body.as_array().ok_or("a malformed tuple")?.iter().map(decode).collect::<Result<_, _>>()?),
                "record" => {
                    let mut out = Vec::new();
                    for pair in body.as_array().ok_or("a malformed record")? {
                        let k = pair.get(0).and_then(J::as_str).ok_or("a malformed record field")?;
                        out.push((Symbol::intern(k), decode(pair.get(1).unwrap_or(&J::Null))?));
                    }
                    Value::record(out)
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
                // `encode` refused it while recording, and said why.
                "unrecorded" => return Err(body.as_str().unwrap_or("a value that could not be recorded").to_string()),
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
        let v = Value::record(vec![
            (Symbol::intern("n"), Value::Int(3)),
            (Symbol::intern("f"), Value::Float(1.0)),
            (Symbol::intern("nan"), Value::Float(f64::NAN)),
            (Symbol::intern("t"), Value::tuple(vec![s("a"), Value::Missing])),
            (Symbol::intern("xs"), Value::array(vec![Value::Bool(true), Value::Int(-7)])),
            (Symbol::intern("u"), Value::Unit),
        ]);
        let back = decode(&encode(&v)).unwrap();
        assert_eq!(format!("{back:?}"), format!("{v:?}"));
        // An Int and a Float stay apart; a record keeps its field order.
        assert!(matches!(decode(&encode(&Value::Float(2.0))).unwrap(), Value::Float(_)));
        assert!(matches!(decode(&encode(&Value::Int(2))).unwrap(), Value::Int(2)));
        let opaque = json!({"opaque": "DataFrame"});
        assert!(decode(&opaque).unwrap_err().contains("cannot be written down"));
    }

    /// `[acc, acc]` sixty times over: sixty-one arrays in memory, 2^60 leaves as a tree.
    fn doubled(n: usize) -> Value {
        (0..n).fold(Value::Int(1), |acc, _| Value::array(vec![acc.clone(), acc]))
    }

    /// A value that holds one part many times is fingerprinted by what it is in memory, not by
    /// the tree it stands for — and the same however it is shared, so a replay that rebuilt
    /// the same arguments another way still matches the recording.
    #[test]
    fn a_fingerprint_hashes_each_shared_part_once_and_ignores_how_it_is_shared() {
        let fp = |args: &[Value]| Call { label: "f", recv: None, args }.fingerprint();
        let start = std::time::Instant::now();
        let huge = fp(&[doubled(60)]);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "took {:?}", start.elapsed());
        assert_eq!(huge.len(), 32);
        // Shared, and written out in full: the same value, the same fingerprint.
        let pair = |a: Value, b: Value| Value::array(vec![a, b]);
        let one = || Value::Int(1);
        let spelled_out = pair(pair(pair(one(), one()), pair(one(), one())), pair(pair(one(), one()), pair(one(), one())));
        assert_eq!(fp(&[doubled(3)]), fp(&[spelled_out]));
        // And different values, different fingerprints — down to the kind of a number.
        assert_ne!(fp(&[doubled(3)]), fp(&[doubled(4)]));
        assert_ne!(fp(&[Value::Int(1)]), fp(&[Value::Float(1.0)]));
        assert_ne!(fp(&[Value::Str(Rc::new("ab".into())), Value::Str(Rc::new("c".into()))]), fp(&[Value::Str(Rc::new("a".into())), Value::Str(Rc::new("bc".into()))]));
        // A value nested far past any stack is hashed too.
        let deep = (0..1_000_000).fold(Value::Int(1), |acc, _| Value::array(vec![acc]));
        assert_eq!(fp(&[deep]).len(), 32);
    }

    /// A part of the run that reads more than a session holds is held only up to that bound:
    /// the call that passes it is noted, later ones are not kept, and a replay stops at the
    /// note and says why. Before, a recorded program that never started an app held every
    /// result it read, without limit, because nothing ever wrote its startup down.
    #[test]
    fn a_recording_holds_a_bounded_amount_and_a_replay_stops_where_it_stopped() {
        let big = Value::Str(Rc::new("x".repeat(100 << 20)));
        let read = |v: &Value| {
            let args = [Value::Str(Rc::new("f.txt".into()))];
            let c = Call { label: "read_text", recv: None, args: &args };
            through(c, 1, 1, || Ok(v.clone()))
        };
        set_mode(Mode::Record(Vec::new()));
        for _ in 0..5 {
            assert!(read(&big).is_ok(), "the live run is never affected");
        }
        let taped = match MODE.with(|m| std::mem::replace(&mut *m.borrow_mut(), Mode::Off)) {
            Mode::Record(t) => t,
            _ => unreachable!(),
        };
        // Two reads fit (200 MiB); the third passed 256 MiB and is the note; the rest are not kept.
        assert_eq!(taped.len(), 3);
        assert!(taped[..2].iter().all(|e| e.get("value").is_some()));
        let why = taped[2]["overflow"].as_str().unwrap();
        assert!(why.contains("`read_text`") && why.contains("256 MiB"), "{why}");

        set_mode(Mode::Replay(taped.into()));
        assert!(read(&Value::Missing).is_ok());
        assert!(read(&Value::Missing).is_ok());
        for _ in 0..2 {
            let e = read(&Value::Missing).unwrap_err();
            assert!(e.message.contains("stopped taping at `read_text`"), "{}", e.message);
        }
        set_mode(Mode::Off);
        DIVERGED.with(|d| *d.borrow_mut() = None);
    }

    /// What the tape cannot hold it notes instead of building: a shared value is refused in
    /// time and memory proportional to what it is, a deep one before it reaches the depth a
    /// session line can be read back at. Every line the tape writes can be read back.
    #[test]
    fn the_tape_writes_down_only_what_a_session_can_hold() {
        let start = std::time::Instant::now();
        let j = encode(&doubled(60));
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "took {:?}", start.elapsed());
        let why = decode(&j).unwrap_err();
        assert!(why.contains("too large for a session"), "{why}");

        let nest = |n: usize| (0..n).fold(Value::Int(1), |acc, _| Value::array(vec![acc]));
        let rec = |n: usize| (0..n).fold(Value::Int(1), |acc, _| Value::record(vec![(Symbol::intern("a"), acc)]));
        for v in [nest(TAPE_DEPTH), nest(TAPE_DEPTH + 1), nest(1_000_000), rec(32), rec(34)] {
            let line = json!({"kind": "event", "effects": [{"fn": "read_json", "args": "0", "value": encode(&v)}]}).to_string();
            let back: J = serde_json::from_str(&line).expect("every line the tape writes reads back");
            let value = &back["effects"][0]["value"];
            match decode(value) {
                Ok(got) => assert_eq!(format!("{got:?}"), format!("{v:?}")),
                Err(why) => assert!(why.contains("nested deeper than a session holds"), "{why}"),
            }
        }
        assert!(decode(&encode(&nest(TAPE_DEPTH))).is_ok());
        assert!(decode(&encode(&nest(TAPE_DEPTH + 1))).is_err());
        assert!(decode(&encode(&rec(33))).is_ok());
        assert!(decode(&encode(&rec(34))).is_err());
        // Ordinary data is written exactly as before.
        let row = Value::record(vec![(Symbol::intern("n"), Value::Int(3)), (Symbol::intern("s"), Value::Str(Rc::new("x".into())))]);
        assert_eq!(encode(&row), json!({"record": [["n", 3], ["s", "x"]]}));
    }
}

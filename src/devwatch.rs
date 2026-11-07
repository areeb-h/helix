//! Hot reload for apps: `helix run --watch app.helix` (ADR 0055, Phase 1).
//!
//! Save a file and the open app shows the new code, with its state kept. The design is a
//! SUPERVISOR, not code swapping. Swapping a function inside a running interpreter would
//! mean mixing two compiled programs' closures in one process, on three engines with three
//! closure representations. Instead:
//!
//! 1. The parent watches every file the program loads (the module graph, re-read after each
//!    reload, so a new import is watched too) by polling size and mtime.
//! 2. On a change it CHECKS the program first, in-process, exactly as `helix check` does. A
//!    program that does not check is reported and NOT reloaded: the running app keeps going,
//!    so a typo costs nothing but the message.
//! 3. A program that checks replaces the child: the old one is killed, a new one started on
//!    the same command line. The model crosses over in a file the parent owns, written by the
//!    child after every change (`dev_keep`) and read by the next one (`dev_session`), and so
//!    does the session token, so the browser's cookie stays valid. The browser's event
//!    stream drops, reconnects within a fraction of a second, and receives the new view
//!    (and the new stylesheet) as a patch.
//!
//! Every engine reloads the same way, because nothing here is engine-specific: the child is
//! an ordinary `helix run`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::Value;

/// The session token, shared by every generation of one `--watch` run.
const ENV_TOKEN: &str = "HELIX_DEV_TOKEN";
/// The file the model crosses generations in. The supervisor creates and removes it.
const ENV_STATE: &str = "HELIX_DEV_STATE";
/// Which generation this child is: 1 for the first start, then 2, 3, … per reload.
const ENV_GENERATION: &str = "HELIX_DEV_GENERATION";

// --- The builtins. ---------------------------------------------------------------------

/// `dev_session()` — `missing` in an ordinary run. Under `helix run --watch`, the record
/// `{token, generation, model}`: the session token every generation shares, which start
/// this is (1 = the first), and the model the previous generation kept (`missing` when
/// there is none, or when what it kept is not JSON this run can read).
pub fn dev_session(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if !args.is_empty() {
        return Err(HelixError::new(format!("`dev_session` takes no arguments, got {}", args.len()), line, col));
    }
    let Ok(token) = std::env::var(ENV_TOKEN) else { return Ok(Value::Missing) };
    let generation = std::env::var(ENV_GENERATION).ok().and_then(|g| g.parse::<i64>().ok()).unwrap_or(1);
    let model = std::env::var(ENV_STATE)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| crate::json::parse(&s).ok())
        .unwrap_or(Value::Missing);
    Ok(Value::Record(Rc::new(vec![
        (Symbol::intern("token"), Value::Str(Rc::new(token))),
        (Symbol::intern("generation"), Value::Int(generation)),
        (Symbol::intern("model"), model),
    ])))
}

/// `dev_keep(model)` — under `helix run --watch`, hand the model to the next generation and
/// answer `true`; in an ordinary run do nothing and answer `false`. A model that is not JSON
/// (a function in it, say) is not kept, and answers `false`: the next generation starts
/// from `init`, which is what it would do without hot reload.
pub fn dev_keep(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let [model] = args else {
        return Err(HelixError::new(format!("`dev_keep` takes 1 argument, got {}", args.len()), line, col));
    };
    let Ok(path) = std::env::var(ENV_STATE) else { return Ok(Value::Bool(false)) };
    let Ok(json) = crate::json::stringify(model) else { return Ok(Value::Bool(false)) };
    // Written beside and renamed over, so the next generation never reads half a model.
    let tmp = format!("{path}.tmp");
    let ok = std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_ok();
    Ok(Value::Bool(ok))
}

/// Under `helix run --watch`, exit when the supervisor does. The supervisor holds the write
/// end of this process's stdin and never writes to it, so end-of-file on stdin means it is
/// gone — killed outright, not only by Ctrl-C (which reaches both). Without this an app
/// outlives a supervisor stopped by an editor's stop button and keeps the port, and the next
/// `--watch` cannot listen. An ordinary run is untouched: no thread, stdin unread.
pub fn exit_with_supervisor() {
    if std::env::var_os(ENV_TOKEN).is_none() {
        return;
    }
    std::thread::spawn(|| {
        use std::io::Read;
        let mut buf = [0u8; 256];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(_) => {}
            }
        }
    });
}

// --- The supervisor. -------------------------------------------------------------------

/// What one check of the program answers: the files it loads (to watch), or the rendered
/// diagnostic that kept it from loading.
pub type Check = Result<Vec<PathBuf>, String>;

/// Each watched file's size and modification time.
fn stamps(files: &[PathBuf]) -> BTreeMap<PathBuf, Option<(u64, SystemTime)>> {
    files
        .iter()
        .map(|f| (f.clone(), std::fs::metadata(f).ok().and_then(|m| Some((m.len(), m.modified().ok()?)))))
        .collect()
}

fn token() -> String {
    use aes_gcm::{Aes256Gcm, KeyInit, aead::OsRng};
    Aes256Gcm::generate_key(&mut OsRng).iter().map(|b| format!("{b:02x}")).collect()
}

/// `helix run --watch <script> [args…]`. `check` is `helix check` for one path; it is passed
/// in so this module does not reach into the command-line front end.
pub fn run_watch(script: &Path, argv: &[String], check: &dyn Fn(&Path) -> Check) -> ExitCode {
    let token = token();
    // ONE state file per script, cleared as a session starts. A supervisor stopped with
    // Ctrl-C cannot clean up after itself (that would take a signal handler, and `unsafe`),
    // so a name per process would leave one file behind per session; a name per script
    // leaves at most one per app, and a new session never inherits an old one's model.
    let state = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        script.canonicalize().unwrap_or_else(|_| script.to_path_buf()).hash(&mut h);
        std::env::temp_dir().join(format!("helix-dev-{:016x}.json", h.finish()))
    };
    let _ = std::fs::remove_file(&state);
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: cannot find this helix executable to restart the app: {e}");
            return ExitCode::FAILURE;
        }
    };
    let spawn = |generation: u32| -> Option<Child> {
        let started = Command::new(&exe)
            .arg("run")
            .arg(script)
            .args(argv)
            .stdin(std::process::Stdio::piped())
            .env(ENV_TOKEN, &token)
            .env(ENV_STATE, &state)
            .env(ENV_GENERATION, generation.to_string())
            .spawn();
        match started {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("hot reload: could not start the app: {e}");
                None
            }
        }
    };
    let shown = script.display();

    // The first start runs whatever the check says: a program that does not check fails
    // in the child with the same message, and the supervisor waits for a fix.
    let mut files = match check(script) {
        Ok(f) => f,
        Err(_) => vec![script.to_path_buf()],
    };
    let mut seen = stamps(&files);
    let mut generation = 1u32;
    let mut child = spawn(generation);
    eprintln!("hot reload: watching {} file(s) of {shown}; save to reload, Ctrl-C to stop", files.len());

    loop {
        std::thread::sleep(Duration::from_millis(120));
        if let Some(c) = child.as_mut()
            && let Ok(Some(status)) = c.try_wait()
        {
            eprintln!("hot reload: the app stopped ({status}); save a file to start it again");
            child = None;
        }
        let now = stamps(&files);
        if now == seen {
            continue;
        }
        // An editor's save can be several writes; wait until the files hold still.
        let mut settled = now;
        let deadline = Instant::now() + Duration::from_millis(600);
        loop {
            std::thread::sleep(Duration::from_millis(60));
            let again = stamps(&files);
            if again == settled || Instant::now() > deadline {
                break;
            }
            settled = again;
        }
        seen = settled;
        match check(script) {
            Err(rendered) => {
                eprint!("{rendered}");
                eprintln!(
                    "hot reload: {shown} does not check, so the running app was kept — fix it and save again"
                );
            }
            Ok(loaded) => {
                if let Some(mut c) = child.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                generation += 1;
                files = loaded;
                seen = stamps(&files);
                child = spawn(generation);
                eprintln!("hot reload: reloaded {shown} (#{generation}), state kept");
            }
        }
    }
}

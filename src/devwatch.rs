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
//! 3. A program that checks replaces the child, and the model and session cross over.
//!
//! THE HANDOVER GIVES THE APP NO AUTHORITY IT DID NOT ALREADY HAVE. It reads and writes no
//! file and puts no secret in the environment (another process of the same user can read a
//! process's environment, which is exactly who the session token keeps out). It is the app's
//! own authenticated HTTP:
//!
//! - Each start generates its own session token, as every app does, and prints its link on
//!   stderr. The supervisor holds that pipe — shared with no one else — and reads the link
//!   from it, showing the user the first one only.
//! - Before a restart it asks the running start for its model: `GET /__dev/model`, with that
//!   start's token.
//! - It tells the new start, with ITS token, `POST /__dev/adopt {token, model}`: also accept
//!   the token the open browser holds, and take this model. A caller can only hand over a
//!   session it could already use.
//! - The `/__dev/` routes exist only in a `--watch` run (`dev_generation()`); in any other run
//!   they are a 404 like any unknown path.
//!
//! Every engine reloads the same way, because nothing here is engine-specific: the child is an
//! ordinary `helix run`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::error::HelixError;
use crate::value::Value;

/// Which start of the app this is under `helix run --watch`: 1 until a link has been shown,
/// then one more per reload. Not a secret, and no authority: a number.
const ENV_GENERATION: &str = "HELIX_DEV_GENERATION";

// --- The builtin. ----------------------------------------------------------------------

/// `dev_generation()` — `missing` in an ordinary run; under `helix run --watch`, which start
/// of the app this is (1 for the first). It reads ONE fixed, non-secret variable that the
/// supervisor sets, and reaches no file, socket or secret.
pub fn dev_generation(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if !args.is_empty() {
        return Err(HelixError::new(format!("`dev_generation` takes no arguments, got {}", args.len()), line, col));
    }
    Ok(match std::env::var(ENV_GENERATION).ok().and_then(|g| g.parse::<i64>().ok()) {
        Some(g) if g >= 1 => Value::Int(g),
        _ => Value::Missing,
    })
}

/// Under `helix run --watch`, exit when the supervisor does. The supervisor holds the write
/// end of this process's stdin and never writes to it, so end-of-file on stdin means it is
/// gone — killed outright, not only by Ctrl-C (which reaches both). Without this an app
/// outlives a supervisor stopped by an editor's stop button and keeps the port, and the next
/// `--watch` cannot listen. An ordinary run is untouched: no thread, stdin unread.
pub fn exit_with_supervisor() {
    if std::env::var_os(ENV_GENERATION).is_none() {
        return;
    }
    std::thread::spawn(|| {
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

/// A running start of the app, and the link it printed once it was listening.
struct Running {
    child: Child,
    link: Arc<Mutex<Option<Link>>>,
}

/// Where a start listens and the token it generated, read from the line it printed.
#[derive(Clone)]
struct Link {
    port: u16,
    token: String,
}

/// `… on http://127.0.0.1:<port>/?token=<64 hex>` — the line `std.ui` prints.
fn parse_link(line: &str) -> Option<Link> {
    let rest = line.split("http://127.0.0.1:").nth(1)?;
    let (port, rest) = rest.split_once("/?token=")?;
    let token: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    let port = port.parse().ok()?;
    (token.len() == 64).then_some(Link { port, token })
}

/// One request to a start on loopback, with its session cookie. The answer's status and body.
fn ask(link: &Link, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", link.port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nCookie: helix_app_{port}={token}\r\nContent-Length: {len}\r\n\r\n{body}",
        port = link.port,
        token = link.token,
        len = body.len(),
    );
    s.write_all(req.as_bytes()).ok()?;
    // The app keeps a connection open after answering (it is a keep-alive server), so the
    // reply ends where its Content-Length says, not at end-of-stream.
    let mut r = BufReader::new(s);
    let mut status_line = String::new();
    r.read_line(&mut status_line).ok()?;
    let status = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some((status, String::from_utf8(body).ok()?))
}

/// `helix run --watch <script> [args…]`. `check` is `helix check` for one path; it is passed
/// in so this module does not reach into the command-line front end.
pub fn run_watch(script: &Path, argv: &[String], check: &dyn Fn(&Path) -> Check) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: cannot find this helix executable to restart the app: {e}");
            return ExitCode::FAILURE;
        }
    };
    let shown = script.display().to_string();
    // The token the open browser holds: the one in the link the user was shown.
    let browser: Arc<Mutex<Option<Link>>> = Arc::new(Mutex::new(None));

    let spawn = |generation: u32| -> Option<Running> {
        let started = Command::new(&exe)
            .arg("run")
            .arg(script)
            .args(argv)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .env(ENV_GENERATION, generation.to_string())
            .spawn();
        let mut child = match started {
            Ok(c) => c,
            Err(e) => {
                eprintln!("hot reload: could not start the app: {e}");
                return None;
            }
        };
        // Relay the app's stderr. Its link is captured, and shown only while the user has not
        // been shown one: after that the open page reconnects by itself.
        let link: Arc<Mutex<Option<Link>>> = Arc::new(Mutex::new(None));
        let (found, shown_link) = (link.clone(), browser.clone());
        if let Some(err) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if found.lock().map(|l| l.is_none()).unwrap_or(false)
                        && let Some(l) = parse_link(&line)
                    {
                        *found.lock().unwrap() = Some(l.clone());
                        let mut b = shown_link.lock().unwrap();
                        match b.as_ref() {
                            None => {
                                *b = Some(l);
                                eprintln!("{line}");
                            }
                            // The port the page uses was taken while the app restarted, so this
                            // start is on another one: the open page cannot follow, so say so.
                            Some(open) if open.port != l.port => {
                                eprintln!("hot reload: port {} was taken during the reload; reopen the app at this link:", open.port);
                                eprintln!("{line}");
                            }
                            Some(_) => {}
                        }
                        continue;
                    }
                    eprintln!("{line}");
                }
            });
        }
        Some(Running { child, link })
    };
    let wait_link = |r: &Running| -> Option<Link> {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if let Some(l) = r.link.lock().ok()?.clone() {
                return Some(l);
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        None
    };

    // The first start runs whatever the check says: a program that does not check fails in
    // the child with the same message, and the supervisor waits for a fix.
    let mut files = check(script).unwrap_or_else(|_| vec![script.to_path_buf()]);
    let mut seen = stamps(&files);
    let mut generation = 1u32;
    let mut running = spawn(generation);
    eprintln!("hot reload: watching {} file(s) of {shown}; save to reload, Ctrl-C to stop", files.len());

    loop {
        std::thread::sleep(Duration::from_millis(120));
        if let Some(r) = running.as_mut()
            && let Ok(Some(status)) = r.child.try_wait()
        {
            eprintln!("hot reload: the app stopped ({status}); save a file to start it again");
            running = None;
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
        let loaded = match check(script) {
            Ok(l) => l,
            Err(rendered) => {
                eprint!("{rendered}");
                eprintln!("hot reload: {shown} does not check, so the running app was kept — fix it and save again");
                continue;
            }
        };
        // Take the model from the running start, then replace it.
        let model = running
            .as_ref()
            .and_then(|r| r.link.lock().ok()?.clone())
            .and_then(|l| ask(&l, "GET", "/__dev/model", ""))
            .and_then(|(status, body)| (status == 200).then_some(body));
        if let Some(mut r) = running.take() {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
        // Until a link has been shown, a start is still "the first" (it opens the browser).
        generation = if browser.lock().map(|b| b.is_none()).unwrap_or(true) { 1 } else { generation + 1 };
        files = loaded;
        seen = stamps(&files);
        running = spawn(generation);
        let handed = match (running.as_ref().and_then(wait_link), browser.lock().ok().and_then(|b| b.clone())) {
            (Some(new), Some(open)) if new.token != open.token => {
                let body = format!(
                    "{{\"token\":\"{}\",\"model\":{}}}",
                    open.token,
                    model.as_deref().unwrap_or("null")
                );
                matches!(ask(&new, "POST", "/__dev/adopt", &body), Some((204, _)))
            }
            _ => false,
        };
        let kept = if handed && model.is_some() { "state kept" } else { "started fresh" };
        eprintln!("hot reload: reloaded {shown} (#{generation}), {kept}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_is_read_from_the_line_std_ui_prints() {
        let t = "ab".repeat(32);
        let l = parse_link(&format!("Counter on http://127.0.0.1:8080/?token={t}")).expect("a link");
        assert_eq!((l.port, l.token.as_str()), (8080, t.as_str()));
        assert!(parse_link("Counter on http://127.0.0.1:8080/?token=abc").is_none(), "a short token is not one");
        assert!(parse_link("update failed on {}: http://example.com").is_none());
    }
}

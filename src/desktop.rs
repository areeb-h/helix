//! Native app windows (ADR 0055, Phase 4): an app in its own window, on the platform's web view
//! — WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux — instead of a browser tab.
//!
//! THE MAIN THREAD. macOS will only run a window's event loop on the process's main thread,
//! and GTK wants the thread that initialised it. Helix already evaluates every program on a
//! spawned big-stack thread (`run_on_big_stack`), so the main thread is free: [`run_main`]
//! parks it on a channel, and only if the program asks for a window does it create the event
//! loop there and hand it everything after. A program that never asks — every script, every
//! test, every headless server — never touches the windowing system, so a `desktop` build runs
//! on a machine with no display exactly as any other build does.
//!
//! NO NEW AUTHORITY. A window shows THIS app: [`open_window`] accepts only a URL on the app's
//! own loopback origin (`http://127.0.0.1:<port>/…`), navigation away from that origin is
//! refused inside the window, a page cannot open further windows, and the inspector exists
//! only in debug builds. It cannot be pointed at a website, so it is not a browser the program
//! controls — it is a frame around the page the program already serves.
//!
//! Built only with `--features desktop` (wry + tao). Without it, `open_window` answers `false`
//! and `std.ui` falls back to opening the system browser, so an app runs the same everywhere.

use crate::error::HelixError;
use crate::value::Value;

/// The origin a window may show — `http://127.0.0.1:<port>` — when `url` is on it.
pub fn local_origin(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let after = &rest[digits.len()..];
    let port: u16 = digits.parse().ok().filter(|p| *p > 0)?;
    (after.is_empty() || after.starts_with('/') || after.starts_with('?')).then(|| format!("http://127.0.0.1:{port}"))
}

/// Whether a window showing `origin` may navigate to `url`: only within that origin.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub fn stays_home(origin: &str, url: &str) -> bool {
    local_origin(url).as_deref() == Some(origin)
}

/// `open_window(url, title?, width?, height?)` — show this app in a native window; `true` if
/// one opened, `false` where none can (a build without `desktop`, a machine with no display),
/// so the caller can open the browser instead.
pub fn open_window(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.is_empty() || args.len() > 4 {
        return Err(HelixError::new(format!("`open_window` takes 1 to 4 arguments, got {}", args.len()), line, col));
    }
    let url = match &args[0] {
        Value::Str(s) => s.to_string(),
        other => return Err(HelixError::new(format!("`open_window` needs the app's URL, got {}", crate::value::with_article(other.type_name())), line, col)),
    };
    if local_origin(&url).is_none() {
        return Err(HelixError::new(format!("a window shows only this app's own page on 127.0.0.1, not `{url}`"), line, col)
            .hint("a window is a frame around the page the app serves; `open_url` opens anything else in the system browser."));
    }
    let title = match args.get(1) {
        Some(Value::Str(s)) => s.to_string(),
        Some(Value::Missing) | None => "Helix".to_string(),
        Some(other) => other.to_string(),
    };
    let size = |i: usize, dflt: f64| match args.get(i) {
        Some(Value::Int(n)) => *n as f64,
        Some(Value::Float(f)) => *f,
        _ => dflt,
    };
    let (w, h) = (size(2, 1100.0).clamp(200.0, 8000.0), size(3, 760.0).clamp(150.0, 8000.0));
    Ok(Value::Bool(imp::request(url, title, w, h)))
}

/// Run `f` — a whole program — on a big-stack thread, keeping the main thread for a window if
/// the program asks for one. Without the `desktop` feature this is exactly `run_on_big_stack`.
pub fn run_main<F: FnOnce() -> std::process::ExitCode + Send>(stack: usize, f: F) -> std::process::ExitCode {
    imp::run_main(stack, f)
}

#[cfg(not(feature = "desktop"))]
mod imp {
    pub fn request(_url: String, _title: String, _w: f64, _h: f64) -> bool {
        false
    }
    pub fn run_main<F: FnOnce() -> std::process::ExitCode + Send>(stack: usize, f: F) -> std::process::ExitCode {
        std::thread::scope(|scope| match std::thread::Builder::new().stack_size(stack).spawn_scoped(scope, f) {
            Ok(h) => h.join().unwrap_or(std::process::ExitCode::FAILURE),
            Err(e) => {
                eprintln!("error: could not allocate the interpreter stack: {e}");
                std::process::ExitCode::FAILURE
            }
        })
    }
}

#[cfg(feature = "desktop")]
mod imp {
    use std::collections::HashMap;
    use std::process::ExitCode;
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Mutex, OnceLock};

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;
    use wry::{NewWindowResponse, WebViewBuilder};

    pub enum Msg {
        Open { url: String, title: String, w: f64, h: f64 },
        Done(bool),
    }

    /// The program's line to the main thread; set by `run_main` before the program starts.
    static LINE: OnceLock<Mutex<Sender<Msg>>> = OnceLock::new();

    /// Whether the program has asked for a window, so the main thread is (or is about to be)
    /// inside the window's event loop rather than waiting to join the program.
    static WINDOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Whether this machine has a display a window could appear on. Asked BEFORE anything is
    /// created, because creating an event loop with no display aborts the process.
    fn has_display() -> bool {
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            return true;
        }
        std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
            || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
    }

    pub fn request(url: String, title: String, w: f64, h: f64) -> bool {
        if !has_display() {
            return false;
        }
        match LINE.get().and_then(|l| l.lock().ok()) {
            Some(tx) => {
                WINDOWED.store(true, std::sync::atomic::Ordering::SeqCst);
                tx.send(Msg::Open { url, title, w, h }).is_ok()
            }
            None => false,
        }
    }

    pub fn run_main<F: FnOnce() -> ExitCode + Send>(stack: usize, f: F) -> ExitCode {
        let (tx, rx) = channel::<Msg>();
        let _ = LINE.set(Mutex::new(tx.clone()));
        std::thread::scope(|scope| {
            let done = tx.clone();
            let worker = std::thread::Builder::new().stack_size(stack).spawn_scoped(scope, move || {
                let code = f();
                // Once a window was asked for, the main thread belongs to the window toolkit,
                // which may be busy — a first start can spend many seconds building the font
                // cache before the window even appears. A finished program must end the process
                // regardless, with its own code, so it does not wait for the toolkit to listen.
                if WINDOWED.load(std::sync::atomic::Ordering::SeqCst) {
                    std::process::exit(if code == ExitCode::SUCCESS { 0 } else { 1 });
                }
                let _ = done.send(Msg::Done(code == ExitCode::SUCCESS));
                code
            });
            let worker = match worker {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("error: could not allocate the interpreter stack: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Until the program asks for a window, the main thread only waits.
            let first = match rx.recv() {
                Ok(Msg::Open { url, title, w, h }) => (url, title, w, h),
                Ok(Msg::Done(_)) | Err(_) => return worker.join().unwrap_or(ExitCode::FAILURE),
            };
            // It asked: the event loop lives here from now on, and everything else the program
            // says arrives through it.
            let event_loop = EventLoopBuilder::<Msg>::with_user_event().build();
            let proxy = event_loop.create_proxy();
            std::thread::spawn(move || {
                for m in rx {
                    if proxy.send_event(m).is_err() {
                        break;
                    }
                }
            });
            let mut windows: HashMap<tao::window::WindowId, (tao::window::Window, wry::WebView)> = HashMap::new();
            let open = |target: &tao::event_loop::EventLoopWindowTarget<Msg>,
                        windows: &mut HashMap<tao::window::WindowId, (tao::window::Window, wry::WebView)>,
                        (url, title, w, h): (String, String, f64, f64)| {
                let Some(origin) = super::local_origin(&url) else { return };
                let window = match WindowBuilder::new()
                    .with_title(title)
                    .with_inner_size(tao::dpi::LogicalSize::new(w, h))
                    .build(target)
                {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("error: could not open a window: {e}");
                        return;
                    }
                };
                let home = origin.clone();
                let builder = WebViewBuilder::new()
                    .with_url(&url)
                    .with_devtools(cfg!(debug_assertions))
                    .with_navigation_handler(move |to| super::stays_home(&home, &to))
                    .with_new_window_req_handler(|_, _| NewWindowResponse::Deny);
                #[cfg(target_os = "linux")]
                let view = {
                    use tao::platform::unix::WindowExtUnix;
                    use wry::WebViewBuilderExtUnix;
                    match window.default_vbox() {
                        Some(vbox) => builder.build_gtk(vbox),
                        None => {
                            eprintln!("error: this window has no container for a web view");
                            return;
                        }
                    }
                };
                #[cfg(not(target_os = "linux"))]
                let view = builder.build(&window);
                match view {
                    Ok(v) => {
                        windows.insert(window.id(), (window, v));
                    }
                    Err(e) => eprintln!("error: could not create the window's web view: {e}"),
                }
            };
            let mut pending = Some(first);
            event_loop.run(move |event, target, control_flow| {
                *control_flow = ControlFlow::Wait;
                if let Some(first) = pending.take() {
                    open(target, &mut windows, first);
                }
                match event {
                    Event::UserEvent(Msg::Open { url, title, w, h }) => open(target, &mut windows, (url, title, w, h)),
                    // The program ended: so does the app, window and all.
                    Event::UserEvent(Msg::Done(ok)) => std::process::exit(if ok { 0 } else { 1 }),
                    // Closing an app's window ends the app, as it does everywhere else.
                    Event::WindowEvent { event: WindowEvent::CloseRequested, window_id, .. } => {
                        windows.remove(&window_id);
                        if windows.is_empty() {
                            std::process::exit(0);
                        }
                    }
                    _ => {}
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_shows_only_the_apps_own_loopback_origin() {
        assert_eq!(local_origin("http://127.0.0.1:8080/?token=ab").as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(local_origin("http://127.0.0.1:8080").as_deref(), Some("http://127.0.0.1:8080"));
        for bad in [
            "https://127.0.0.1:8080/",
            "http://localhost:8080/",
            "http://127.0.0.1.evil.com:8080/",
            "http://127.0.0.1:8080@evil.com/",
            "http://127.0.0.1:0/",
            "http://127.0.0.1:99999/",
            "http://127.0.0.1/",
            "javascript:alert(1)",
            "file:///etc/passwd",
        ] {
            assert!(local_origin(bad).is_none(), "{bad}");
        }
        assert!(stays_home("http://127.0.0.1:8080", "http://127.0.0.1:8080/events"));
        assert!(!stays_home("http://127.0.0.1:8080", "http://127.0.0.1:9090/"));
        assert!(!stays_home("http://127.0.0.1:8080", "https://example.com/"));
    }
}

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

/// `app_window(title, width?, height?)` — open a native window for this app and return a
/// listener fed by it (ADR 0063, `src/localnet.rs`): the page loads through the private
/// `helix:` scheme and its events and frames travel through the web view's own message
/// channel, so NO PORT is opened. `missing` where no window can open, so the caller serves
/// over TCP instead.
pub fn app_window(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.is_empty() || args.len() > 3 {
        return Err(HelixError::new(format!("`app_window` takes 1 to 3 arguments, got {}", args.len()), line, col));
    }
    let title = match &args[0] {
        Value::Str(s) => s.to_string(),
        other => other.to_string(),
    };
    let size = |i: usize, dflt: f64| match args.get(i) {
        Some(Value::Int(n)) => *n as f64,
        Some(Value::Float(f)) => *f,
        _ => dflt,
    };
    let (w, h) = (size(1, 1100.0).clamp(200.0, 8000.0), size(2, 760.0).clamp(150.0, 8000.0));
    Ok(match imp::request_app(title, w, h) {
        Some(link) => Value::Net(std::rc::Rc::new(crate::serve::NetHandle::Local(std::rc::Rc::new(link)))),
        None => Value::Missing,
    })
}

/// The address a window loads its app from. WebKitGTK and WKWebView show a custom scheme as
/// itself; WebView2 (and Android) map it onto `http://<scheme>.<host>`.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub const APP_URL: &str = if cfg!(any(target_os = "windows", target_os = "android")) { "http://helix.app/" } else { "helix://app/" };

/// Whether `url` is on the app's own private origin — the only place its window may go, and
/// the only page whose messages are read.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub fn on_app_origin(url: &str) -> bool {
    url.strip_prefix(APP_URL).is_some_and(|rest| !rest.starts_with('/') && !rest.contains('@'))
        || url == APP_URL.trim_end_matches('/')
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
    pub fn request_app(_title: String, _w: f64, _h: f64) -> Option<crate::localnet::Link> {
        None
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
    use std::time::Duration;
    use std::sync::{Mutex, OnceLock};

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    use tao::window::{WindowBuilder, WindowId};
    use wry::{NewWindowResponse, WebViewBuilder};

    use crate::localnet::{Incoming, Link, Reply};

    /// `reply` is answered `true` once the window is on screen, `false` if none could open —
    /// so `open_window` reports what happened, and the app falls back to the browser.
    pub enum Msg {
        Open { url: String, title: String, w: f64, h: f64, reply: Sender<bool> },
        /// A window for the app's own loop (ADR 0063): its requests go to `to_app`, and the
        /// reply says where to send frames — or `None` if no window opened.
        OpenApp { title: String, w: f64, h: f64, to_app: Sender<Incoming>, reply: Sender<Option<(EventLoopProxy<Msg>, WindowId)>> },
        /// One frame for a window's page.
        Frame(WindowId, String),
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
        let Some(tx) = LINE.get().and_then(|l| l.lock().ok()).map(|l| l.clone()) else { return false };
        let (reply, answer) = channel();
        if tx.send(Msg::Open { url, title, w, h, reply }).is_err() {
            return false;
        }
        // The main thread answers once the window exists or could not be made. A first start can
        // spend seconds building caches, so this waits; if it waits very long, the window is
        // still coming — a browser tab as well would be the wrong answer.
        let opened = answer.recv_timeout(Duration::from_secs(20)).unwrap_or(true);
        if opened {
            WINDOWED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        opened
    }

    /// Ask the main thread for an app window; the link its requests arrive on, or `None`.
    pub fn request_app(title: String, w: f64, h: f64) -> Option<Link> {
        if !has_display() {
            return None;
        }
        let tx = LINE.get().and_then(|l| l.lock().ok()).map(|l| l.clone())?;
        let (to_app, from_window) = channel();
        let (reply, answer) = channel();
        tx.send(Msg::OpenApp { title, w, h, to_app, reply }).ok()?;
        // As for `request`: a first start can be slow, but a window that never answers is none.
        let (proxy, id) = answer.recv_timeout(Duration::from_secs(30)).ok()??;
        WINDOWED.store(true, std::sync::atomic::Ordering::SeqCst);
        let push = move |frame: &str| proxy.send_event(Msg::Frame(id, frame.to_string())).is_ok();
        Some(Link::new(from_window, Box::new(push)))
    }

    /// Whether the windowing system can actually be reached — a `DISPLAY` that names no
    /// running server, or a Wayland socket nobody listens on, is caught here, where it is an
    /// answer, instead of inside tao, where it is a panic.
    fn display_opens() -> bool {
        #[cfg(target_os = "linux")]
        {
            gtk::init().is_ok()
        }
        #[cfg(not(target_os = "linux"))]
        {
            true
        }
    }

    /// A window whose web view talks only to the app's loop (ADR 0063): the page comes from
    /// the private `helix:` scheme, the page's messages are read only from that origin and
    /// only in the runtime's own shapes, and navigation anywhere else is refused.
    fn open_app(
        target: &tao::event_loop::EventLoopWindowTarget<Msg>,
        windows: &mut HashMap<WindowId, (tao::window::Window, wry::WebView)>,
        title: String,
        w: f64,
        h: f64,
        to_app: Sender<Incoming>,
    ) -> Option<WindowId> {
        let window = match WindowBuilder::new().with_title(title).with_inner_size(tao::dpi::LogicalSize::new(w, h)).build(target) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("error: could not open a window: {e}");
                return None;
            }
        };
        let pages = to_app.clone();
        let builder = WebViewBuilder::new()
            .with_asynchronous_custom_protocol("helix".into(), move |_id, req, responder| {
                let answer = move |status: u16, ctype: &str, body: Vec<u8>| {
                    let resp = wry::http::Response::builder()
                        .status(status)
                        .header("Content-Type", ctype)
                        .header("Cache-Control", "no-store")
                        .body(body)
                        .unwrap_or_else(|_| wry::http::Response::new(Vec::new()));
                    responder.respond(resp);
                };
                // The page and nothing else comes through here: a GET on the app's own host.
                let uri = req.uri();
                let ours = uri.host().is_none_or(|h| h == "app") && req.method() == wry::http::Method::GET;
                if !ours {
                    return answer(405, "text/plain", b"not here".to_vec());
                }
                let path = uri.path().to_string();
                let query = uri.query().unwrap_or("").to_string();
                let (ret, back) = channel::<Reply>();
                let asked = pages.send(Incoming::Request {
                    method: "GET".into(),
                    path,
                    query,
                    body: String::new(),
                    reply: Some(Box::new(move |r| {
                        let _ = ret.send(r);
                    })),
                });
                if asked.is_err() {
                    return answer(503, "text/plain", b"the app has stopped".to_vec());
                }
                // Waited for OFF the main thread, so the window stays live while the app renders.
                std::thread::spawn(move || match back.recv_timeout(Duration::from_secs(60)) {
                    Ok(r) => answer(r.status, &r.content_type, r.body),
                    Err(_) => answer(504, "text/plain", b"the app did not answer".to_vec()),
                });
            })
            .with_ipc_handler(move |req| {
                if !super::on_app_origin(&req.uri().to_string()) {
                    return;
                }
                if let Some(m) = crate::localnet::from_page(req.body()) {
                    let _ = to_app.send(m);
                }
            })
            .with_url(super::APP_URL)
            .with_devtools(cfg!(debug_assertions))
            .with_navigation_handler(|to| super::on_app_origin(&to))
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny);
        #[cfg(target_os = "linux")]
        let view = {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            match window.default_vbox() {
                Some(vbox) => builder.build_gtk(vbox),
                None => {
                    eprintln!("error: this window has no container for a web view");
                    return None;
                }
            }
        };
        #[cfg(not(target_os = "linux"))]
        let view = builder.build(&window);
        match view {
            Ok(v) => {
                let id = window.id();
                windows.insert(id, (window, v));
                Some(id)
            }
            Err(e) => {
                eprintln!("error: could not create the window's web view: {e}");
                None
            }
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
            // Until the program asks for a window, the main thread only waits. A request this
            // machine cannot serve — no display after all — is answered `false`, and the thread
            // goes back to waiting: the program carries on in the browser.
            let first = loop {
                match rx.recv() {
                    Ok(m @ (Msg::Open { .. } | Msg::OpenApp { .. })) => {
                        if display_opens() {
                            break m;
                        }
                        match m {
                            Msg::Open { reply, .. } => {
                                let _ = reply.send(false);
                            }
                            Msg::OpenApp { reply, .. } => {
                                let _ = reply.send(None);
                            }
                            _ => {}
                        }
                    }
                    Ok(Msg::Frame(..)) => {}
                    Ok(Msg::Done(_)) | Err(_) => return worker.join().unwrap_or(ExitCode::FAILURE),
                }
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
            let mut windows: HashMap<WindowId, (tao::window::Window, wry::WebView)> = HashMap::new();
            let open = |target: &tao::event_loop::EventLoopWindowTarget<Msg>,
                        windows: &mut HashMap<WindowId, (tao::window::Window, wry::WebView)>,
                        (url, title, w, h): (String, String, f64, f64)|
             -> bool {
                let Some(origin) = super::local_origin(&url) else { return false };
                let window = match WindowBuilder::new()
                    .with_title(title)
                    .with_inner_size(tao::dpi::LogicalSize::new(w, h))
                    .build(target)
                {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("error: could not open a window: {e}");
                        return false;
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
                            return false;
                        }
                    }
                };
                #[cfg(not(target_os = "linux"))]
                let view = builder.build(&window);
                match view {
                    Ok(v) => {
                        windows.insert(window.id(), (window, v));
                        true
                    }
                    Err(e) => {
                        eprintln!("error: could not create the window's web view: {e}");
                        false
                    }
                }
            };
            let frames = event_loop.create_proxy();
            let mut pending = Some(first);
            event_loop.run(move |event, target, control_flow| {
                *control_flow = ControlFlow::Wait;
                let handle = |m: Msg, windows: &mut HashMap<WindowId, (tao::window::Window, wry::WebView)>| match m {
                    Msg::Open { url, title, w, h, reply } => {
                        let _ = reply.send(open(target, windows, (url, title, w, h)));
                    }
                    Msg::OpenApp { title, w, h, to_app, reply } => {
                        let id = open_app(target, windows, title, w, h, to_app);
                        let _ = reply.send(id.map(|id| (frames.clone(), id)));
                    }
                    Msg::Frame(id, text) => {
                        if let Some((_, view)) = windows.get(&id) {
                            // The frame as a JS string literal: JSON's string syntax is JS's.
                            let lit = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".into());
                            let _ = view.evaluate_script(&format!("window.__hx_frame && window.__hx_frame({lit})"));
                        }
                    }
                    Msg::Done(ok) => std::process::exit(if ok { 0 } else { 1 }),
                };
                if let Some(m) = pending.take() {
                    handle(m, &mut windows);
                }
                match event {
                    Event::UserEvent(m @ (Msg::Open { .. } | Msg::OpenApp { .. } | Msg::Frame(..))) => handle(m, &mut windows),
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

    #[test]
    fn an_app_window_stays_on_its_private_origin() {
        let base = APP_URL;
        assert!(on_app_origin(base));
        assert!(on_app_origin(&format!("{base}?x=1")));
        assert!(on_app_origin(&format!("{base}page")));
        for bad in [
            "https://example.com/".to_string(),
            "http://127.0.0.1:8080/".to_string(),
            base.replace("app/", "app.evil.com/"),
            base.replace("app/", "appx/"),
            format!("{base}/evil.com"),
            format!("{base}x@evil.com"),
            "file:///etc/passwd".to_string(),
            "javascript:alert(1)".to_string(),
        ] {
            assert!(!on_app_origin(&bad), "{bad}");
        }
    }
}

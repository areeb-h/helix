//! In-process app connections (ADR 0063): a listener fed by a channel instead of a socket.
//!
//! An app's runtime (`std/ui.helix`) is a loop over a LISTENER and the CONNECTIONS it yields:
//! a page load, a subscription to frames, an event. Over TCP those are HTTP requests. In a
//! native window they arrive from the web view itself — the page through a private URL scheme,
//! events and the subscription as `window.ipc.postMessage` — and this module turns each into
//! the same request record a socket would have produced, so the loop runs unchanged.
//!
//! WHAT THIS REMOVES. A windowed app opens no port. Nothing on the machine — another user's
//! process, a web page rebinding DNS, a port scanner — has anything to connect to, so the
//! session token, the cookie, the Host and Origin checks have nothing left to defend. And the
//! program needs no network authority to show its own window: these handles skip the `net`
//! gate, because they reach nothing but the app's own page.
//!
//! The window side (`src/desktop.rs`) is the only producer; this side is plain std, in every
//! build, so the loop and its tests do not depend on a window toolkit.

// Without `desktop` nothing produces a link; the loop's side still compiles and is tested.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use crate::symbol::Symbol;
use crate::value::Value;

/// What a page load is answered with.
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Answers one page load; called once, from whichever thread the loop runs on.
pub type ReplyFn = Box<dyn FnOnce(Reply) + Send>;

/// One thing the window sent.
pub enum Incoming {
    /// A request: `GET /` for the page (with a reply), or `GET /events` / `POST /event` from
    /// the page's script (no reply — the script does not wait for one).
    Request { method: String, path: String, query: String, body: String, reply: Option<ReplyFn> },
}

/// The listener's half: requests in, frames out.
pub struct Link {
    rx: Receiver<Incoming>,
    held: RefCell<VecDeque<Incoming>>,
    /// Deliver one frame to the page; `false` once the window is gone.
    push: Box<dyn Fn(&str) -> bool>,
    open: Cell<bool>,
}

impl Link {
    pub fn new(rx: Receiver<Incoming>, push: Box<dyn Fn(&str) -> bool>) -> Link {
        Link { rx, held: RefCell::new(VecDeque::new()), push, open: Cell::new(true) }
    }

    /// The next request, without waiting.
    pub fn next(&self) -> Option<Incoming> {
        if let Some(i) = self.held.borrow_mut().pop_front() {
            return Some(i);
        }
        match self.rx.try_recv() {
            Ok(i) => Some(i),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.open.set(false);
                None
            }
        }
    }

    /// Block until a request is waiting or `ms` pass — the loop's `wait`, so an idle window
    /// app sleeps instead of spinning.
    pub fn wait(&self, ms: i64) {
        if !self.held.borrow().is_empty() || !self.open.get() {
            return;
        }
        match self.rx.recv_timeout(Duration::from_millis(ms.clamp(0, 60_000) as u64)) {
            Ok(i) => self.held.borrow_mut().push_back(i),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => self.open.set(false),
        }
    }

    pub fn push(&self, frame: &str) -> bool {
        self.open.get() && (self.push)(frame)
    }
}

/// One connection from a [`Link`]: a request, and — for the subscription — the stream of
/// frames after it.
pub struct Conn {
    pub link: Rc<Link>,
    pub request: RefCell<Value>,
    pub reply: RefCell<Option<ReplyFn>>,
    pub streaming: Cell<bool>,
    pub open: Cell<bool>,
}

/// The request record a socket would have produced, plus `transport: "window"`. That field is
/// set ONLY here — a request parsed from a socket has exactly the seven HTTP fields — so the
/// runtime can tell its own window from the network without trusting anything a peer sent.
pub fn request_record(method: &str, path: &str, query: &str, body: &str) -> Value {
    let s = |x: &str| Value::Str(Rc::new(x.to_string()));
    Value::Record(Rc::new(vec![
        (Symbol::intern("method"), s(method)),
        (Symbol::intern("path"), s(path)),
        (Symbol::intern("query"), s(query)),
        (Symbol::intern("headers"), Value::Headers(Rc::new(Vec::new()))),
        (Symbol::intern("body"), s(body)),
        (Symbol::intern("peer"), Value::Missing),
        (Symbol::intern("version"), s("1.1")),
        (Symbol::intern("transport"), s("window")),
    ]))
}

/// The paths a window's script may send, and how. Anything else from the page is dropped —
/// the script is the runtime's own, so another message is a bug or an injection, and the
/// loop never sees it.
pub fn from_page(message: &str) -> Option<Incoming> {
    let v: serde_json::Value = serde_json::from_str(message).ok()?;
    let path = v.get("path")?.as_str()?;
    let (method, body) = match path {
        "/events" => ("GET", String::new()),
        "/event" => ("POST", v.get("body")?.as_str()?.to_string()),
        _ => return None,
    };
    Some(Incoming::Request { method: method.into(), path: path.into(), query: String::new(), body, reply: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_may_send_only_events_and_its_subscription() {
        let ok = |m: &str| matches!(from_page(m), Some(Incoming::Request { .. }));
        assert!(ok(r#"{"path":"/events"}"#));
        assert!(ok(r#"{"path":"/event","body":"{\"msg\":\"go\"}"}"#));
        for bad in [
            r#"{"path":"/"}"#,
            r#"{"path":"/__dev/model"}"#,
            r#"{"path":"/event"}"#,
            r#"{"path":"/event","body":7}"#,
            "not json",
            r#"["/events"]"#,
        ] {
            assert!(from_page(bad).is_none(), "{bad}");
        }
        let Some(Incoming::Request { method, body, .. }) = from_page(r#"{"path":"/event","body":"x"}"#) else { panic!() };
        assert_eq!((method.as_str(), body.as_str()), ("POST", "x"));
    }

    #[test]
    fn a_link_hands_over_requests_in_order_and_reports_frames() {
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = Rc::new(RefCell::new(Vec::<String>::new()));
        let s2 = sent.clone();
        // `push` is `Fn`, not `Send`: the link lives on the loop's thread.
        let link = Link::new(rx, Box::new(move |f| {
            s2.borrow_mut().push(f.to_string());
            true
        }));
        for p in ["/events", "/event"] {
            tx.send(from_page(&format!(r#"{{"path":"{p}","body":"b"}}"#)).unwrap()).unwrap();
        }
        link.wait(10);
        let paths: Vec<String> = std::iter::from_fn(|| link.next())
            .map(|Incoming::Request { path, .. }| path)
            .collect();
        assert_eq!(paths, ["/events", "/event"]);
        assert!(link.push("frame"));
        assert_eq!(sent.borrow().as_slice(), ["frame"]);
        drop(tx);
        link.wait(10);
        assert!(!link.push("after"), "a closed window takes no frames");
    }
}

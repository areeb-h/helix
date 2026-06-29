//! A minimal blocking HTTP/1.1 server — the "serve an API straight from Helix" need.
//!
//! The model is the same recursive *fold over events* that drives the real-time
//! `sleep`+`emit` loop, with HTTP requests as the events: `listen(port)` returns a
//! listener, `listener.accept()` blocks for one request and hands back a connection
//! carrying it, `conn.request()` is the parsed request record, and
//! `conn.respond(value)` writes the reply. The handler is an ordinary Helix function
//! the *program* calls in its own loop — so there is no engine re-entrancy and no
//! first-class dispatch from Rust; routing is a plain `match conn.request().path`.
//! Like `print`/`emit`/`http_get`, these are **effects**: impure builtins that never
//! enter the differential oracle or vmparity.
//!
//! v1 is single-threaded and blocking (one request at a time — fine for dev servers,
//! internal tools, and data/bio dashboards), HTTP/1.1 with a `Content-Length` body,
//! `Connection: close` per response. No TLS (sit behind a reverse proxy), no
//! keep-alive, no chunked transfer. Pure `std::net` — no new dependency, so a built
//! binary stays self-contained.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::rc::Rc;
use std::time::Duration;

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::{DictKey, Value};

/// The opaque payload of [`Value::Net`]: either a bound listener or a single accepted
/// connection. Held behind an `Rc` so the `Value` variant stays one word. A connection
/// carries the already-parsed `request` record plus its writable `stream`; the stream
/// is taken out of the `Option` when responded to, so a second `respond` on the same
/// connection is a clean error rather than a double write.
pub enum NetHandle {
    Listener(TcpListener),
    Conn { request: Value, stream: RefCell<Option<TcpStream>> },
}

/// Cap on a request body we will buffer (a larger `Content-Length` is truncated to
/// this) — a single malicious/oversized request must not OOM the process.
const MAX_BODY: usize = 64 << 20; // 64 MiB

/// How long to wait on a slow client mid-request before giving up — a blocking,
/// single-threaded server would otherwise hang forever on one stalled connection.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// `listen(port)` — bind a TCP listener on `127.0.0.1:port` (loopback by default, like
/// Flask's dev server; front it with a proxy to expose it). Returns a listener value
/// to drive with `.accept()`.
pub fn listen(port: i64, line: usize, col: usize) -> Result<Value, HelixError> {
    if !(1..=65535).contains(&port) {
        return Err(HelixError::new(
            format!("`listen` needs a port in 1..=65535, got {port}"),
            line,
            col,
        ));
    }
    let listener = TcpListener::bind(("127.0.0.1", port as u16)).map_err(|e| {
        HelixError::new(format!("could not bind 127.0.0.1:{port}: {e}"), line, col)
            .hint("the port may already be in use — try another, or stop the process holding it.")
    })?;
    Ok(Value::Net(Rc::new(NetHandle::Listener(listener))))
}

/// `listener.accept()` — block until a client connects, read one HTTP request, and
/// return a **connection** carrying it. Drive it with `conn.request()` (the record
/// `{method, path, query, headers, body}`) and `conn.respond(value)`.
pub fn accept(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    let listener = match &**handle {
        NetHandle::Listener(l) => l,
        NetHandle::Conn { .. } => {
            return Err(HelixError::new(
                "`accept` works on a listener from `listen(port)`, not a connection",
                line,
                col,
            ));
        }
    };
    let (stream, _peer) = listener
        .accept()
        .map_err(|e| HelixError::new(format!("accept failed: {e}"), line, col))?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
    // Read the request from a clone so the original stream stays free for the reply
    // (both refer to the same socket; the read clone is dropped when this returns).
    let read_side = stream
        .try_clone()
        .map_err(|e| HelixError::new(format!("could not read the connection: {e}"), line, col))?;
    let request = parse_request(read_side, line, col)?;
    Ok(Value::Net(Rc::new(NetHandle::Conn {
        request,
        stream: RefCell::new(Some(stream)),
    })))
}

/// `conn.request()` — the parsed request record for this connection.
pub fn request(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    match &**handle {
        NetHandle::Conn { request, .. } => Ok(request.clone()),
        NetHandle::Listener(_) => Err(HelixError::new(
            "`request` works on a connection from `accept()`, not a listener",
            line,
            col,
        )),
    }
}

/// Parse one HTTP/1.1 request into a record `{method, path, query, headers, body}`.
/// Headers are a `Dict` keyed by lowercased name (HTTP names are case-insensitive).
fn parse_request(stream: TcpStream, line: usize, col: usize) -> Result<Value, HelixError> {
    let err = |e: std::io::Error| HelixError::new(format!("reading the request failed: {e}"), line, col);
    let mut reader = BufReader::new(stream);

    // Request line: METHOD TARGET HTTP/1.1
    let mut request_line = String::new();
    reader.read_line(&mut request_line).map_err(err)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    // Headers until the blank line; capture Content-Length for the body.
    let mut headers = std::collections::BTreeMap::new();
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h).map_err(err)?;
        let t = h.trim_end_matches(['\r', '\n']);
        if n == 0 || t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.insert(DictKey::Str(Rc::new(key)), Value::Str(Rc::new(val)));
        }
    }

    // Body (clamped to MAX_BODY so an oversized Content-Length can't exhaust memory).
    let to_read = content_length.min(MAX_BODY);
    let mut body = vec![0u8; to_read];
    if to_read > 0 {
        reader.read_exact(&mut body).map_err(err)?;
    }
    let body = String::from_utf8_lossy(&body).into_owned();

    let record = vec![
        (Symbol::intern("method"), Value::Str(Rc::new(method))),
        (Symbol::intern("path"), Value::Str(Rc::new(path))),
        (Symbol::intern("query"), Value::Str(Rc::new(query))),
        (Symbol::intern("headers"), Value::Dict(Rc::new(headers))),
        (Symbol::intern("body"), Value::Str(Rc::new(body))),
    ];
    Ok(Value::Record(Rc::new(record)))
}

/// `connection.respond(value)` — write the HTTP reply and close the connection.
///
/// The reply is derived from `value`:
/// - a **record** may carry `{status, json, html, text}` (any subset): `status`
///   defaults to 200; `json` serializes the value (`application/json`), `html` and
///   `text` send a string as `text/html` / `text/plain`;
/// - a **string** is sent as `text/plain`;
/// - any other value is serialized as JSON.
pub fn respond(handle: &Rc<NetHandle>, value: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let cell = match &**handle {
        NetHandle::Conn { stream, .. } => stream,
        NetHandle::Listener(_) => {
            return Err(HelixError::new(
                "`respond` works on a connection from `accept()`, not a listener",
                line,
                col,
            ));
        }
    };
    let mut stream = match cell.borrow_mut().take() {
        Some(s) => s,
        None => {
            return Err(HelixError::new("this connection has already been responded to", line, col));
        }
    };

    let (status, content_type, body) = build_response(value, line, col)?;
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        reason = reason_phrase(status),
        len = body.len(),
    );
    let write = |s: &mut TcpStream| -> std::io::Result<()> {
        s.write_all(head.as_bytes())?;
        s.write_all(body.as_bytes())?;
        s.flush()
    };
    // A write failure means the client went away (broken pipe / connection reset) — a
    // routine event for a server, never the program's fault. Best-effort: drop the
    // undeliverable response and keep the program's accept loop alive, the same
    // philosophy as the SIGPIPE-for-stdout handling. A server that died because a
    // browser tab closed would be unusable (and SSE clients disconnect constantly).
    let _ = write(&mut stream);
    let _ = stream.shutdown(Shutdown::Both);
    Ok(Value::Unit)
}

/// Derive `(status, content_type, body)` from a response value (see [`respond`]).
fn build_response(value: &Value, line: usize, col: usize) -> Result<(i64, &'static str, String), HelixError> {
    let json_of = |v: &Value| -> Result<String, HelixError> {
        match crate::writers::to_json(std::slice::from_ref(v), line, col)? {
            Value::Str(s) => Ok((*s).clone()),
            other => Ok(other.to_string()),
        }
    };
    match value {
        Value::Record(fields) => {
            let get = |name: &str| fields.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| v);
            let status = match get("status") {
                Some(Value::Int(n)) => *n,
                _ => 200,
            };
            // `text`/`html` stringify any value via Display (a `Dna`, number, etc. —
            // not only a `String`), so `{ text: seq.reverse_complement() }` sends the
            // sequence text, not a JSON dump of the record.
            let as_text = |v: &Value| match v {
                Value::Str(s) => (**s).clone(),
                other => other.to_string(),
            };
            if let Some(v) = get("json") {
                Ok((status, "application/json", json_of(v)?))
            } else if let Some(v) = get("html") {
                Ok((status, "text/html; charset=utf-8", as_text(v)))
            } else if let Some(v) = get("text").or_else(|| get("body")) {
                Ok((status, "text/plain; charset=utf-8", as_text(v)))
            } else {
                // A record with neither a body field nor a recognized one → JSON of it.
                Ok((status, "application/json", json_of(value)?))
            }
        }
        Value::Str(s) => Ok((200, "text/plain; charset=utf-8", (**s).clone())),
        other => Ok((200, "application/json", json_of(other)?)),
    }
}

/// The reason phrase for the common status codes; unknown codes get a generic phrase
/// by class (HTTP allows any phrase — clients key off the numeric code).
fn reason_phrase(status: i64) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        s if (200..300).contains(&s) => "OK",
        s if (300..400).contains(&s) => "Redirect",
        s if (400..500).contains(&s) => "Client Error",
        _ => "Server Error",
    }
}

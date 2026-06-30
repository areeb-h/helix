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

use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
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
    Conn {
        request: Value,
        stream: RefCell<Option<TcpStream>>,
        /// Outbound bytes not yet accepted by the kernel send buffer. For a streaming
        /// (`sse`) connection the socket is non-blocking, so a slow client backs bytes
        /// up here instead of freezing the event loop; they drain on later `send`s.
        pending: RefCell<Vec<u8>>,
    },
}

/// Cap on a request body we will buffer (a larger `Content-Length` is truncated to
/// this) — a single malicious/oversized request must not OOM the process.
const MAX_BODY: usize = 64 << 20; // 64 MiB

/// How long to wait on a slow client mid-request before giving up — a blocking,
/// single-threaded server would otherwise hang forever on one stalled connection.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on a streaming connection's un-drained backlog. A client more than this far
/// behind on an SSE stream is treated as gone (and `send` returns false), so one slow
/// reader can never grow memory without bound or wedge the shard's event loop.
const MAX_PENDING: usize = 4 << 20; // 4 MiB

/// Bound on how long a one-shot `respond` write may block on a slow reader — without it,
/// a client that stops reading mid-body would hang the (single-threaded) shard.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How to re-run this program from the top — stashed by the CLI before execution so a
/// sharded `listen` can launch identical worker interpreters. `File` re-loads the entry
/// (and its imports); `Source` re-runs an inline program (`helix eval` / a bundled exe).
#[derive(Clone)]
pub enum Rerun {
    File(PathBuf),
    Source(String, String),
}

static RERUN: OnceLock<Rerun> = OnceLock::new();

/// Record how to re-run this program (called once by the CLI at startup). Idempotent.
pub fn set_rerun(spec: Rerun) {
    let _ = RERUN.set(spec);
}

thread_local! {
    /// True on a spawned shard worker, so its own `listen(port, shards)` call binds its
    /// socket but does NOT recursively spawn more shards.
    static IS_SHARD: Cell<bool> = const { Cell::new(false) };
}

/// Stack size for a shard worker — matches the main interpreter thread (the front-end
/// recurses over the AST); reserved address space, not resident memory.
const SHARD_STACK: usize = 1024 * 1024 * 1024;

/// `listen(port)` / `listen(port, shards)` — bind a listener on `127.0.0.1:port`.
///
/// With `shards > 1`, Helix serves on N **share-nothing** worker interpreters: this
/// spawns `shards - 1` threads, each of which re-runs the whole program independently
/// (its own parse → compile → VM → globals — nothing is shared, so `Rc` values never
/// cross a thread and no `Arc`/locks are needed) and binds the *same* port via
/// `SO_REUSEPORT`. The Linux kernel then hashes each incoming connection to one worker
/// — true multi-core serving with no lock contention. This is the across-core half of
/// a thread-per-core design (ScyllaDB/Redpanda/Seastar); `poll()` is the within-core
/// half. Top-level code runs once per shard (each worker re-initializes its own state).
pub fn listen(port: i64, shards: i64, line: usize, col: usize) -> Result<Value, HelixError> {
    if !(1..=65535).contains(&port) {
        return Err(HelixError::new(format!("`listen` needs a port in 1..=65535, got {port}"), line, col));
    }
    if shards < 1 {
        return Err(HelixError::new(format!("`listen` needs at least 1 shard, got {shards}"), line, col));
    }
    let listener = bind_listener(port as u16, line, col)?;
    // The primary worker (not itself a shard) spawns the rest.
    if shards > 1 && !IS_SHARD.with(Cell::get) {
        spawn_shards(shards as usize, line, col)?;
    }
    Ok(Value::Net(Rc::new(NetHandle::Listener(listener))))
}

/// Bind a loopback listener. On Linux it sets `SO_REUSEPORT`/`SO_REUSEADDR` so multiple
/// shard workers can bind the same port and the kernel load-balances; elsewhere it falls
/// back to a plain bind (so `shards > 1` would fail on the second bind — sharding is
/// Linux-only for now).
fn bind_listener(port: u16, line: usize, col: usize) -> Result<TcpListener, HelixError> {
    let bind_err = |e: std::io::Error| {
        HelixError::new(format!("could not bind 127.0.0.1:{port}: {e}"), line, col)
            .hint("the port may already be in use — try another, or stop the process holding it.")
    };
    #[cfg(target_os = "linux")]
    {
        bind_reuseport(port).map_err(bind_err)
    }
    #[cfg(not(target_os = "linux"))]
    {
        TcpListener::bind(("127.0.0.1", port)).map_err(bind_err)
    }
}

/// Create a `127.0.0.1:port` listener with `SO_REUSEPORT` + `SO_REUSEADDR` set before
/// bind, so N worker sockets can share the port and the kernel distributes connections.
#[cfg(target_os = "linux")]
fn bind_reuseport(port: u16) -> std::io::Result<TcpListener> {
    use std::os::unix::io::FromRawFd;
    // SAFETY: a straight-line socket/setsockopt/bind/listen sequence on a fresh fd. The
    // fd is wrapped in a `TcpListener` immediately, so every early return (a failed
    // option/bind/listen) drops it and closes the fd — no leak.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let listener = TcpListener::from_raw_fd(fd);
        let one: libc::c_int = 1;
        let set_opt = |opt| {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &one as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if set_opt(libc::SO_REUSEADDR) < 0 || set_opt(libc::SO_REUSEPORT) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: libc::INADDR_LOOPBACK.to_be() },
            sin_zero: [0; 8],
        };
        let bind = libc::bind(
            fd,
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        if bind < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::listen(fd, 128) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(listener)
    }
}

/// Spawn `total - 1` shard workers, each re-running the program on its own thread with
/// `IS_SHARD` set (so it binds but doesn't recurse into spawning). Workers are detached:
/// they run their own accept loops for the life of the process.
fn spawn_shards(total: usize, line: usize, col: usize) -> Result<(), HelixError> {
    let rerun = RERUN.get().cloned().ok_or_else(|| {
        HelixError::new("internal: cannot shard — the program's source was not recorded", line, col)
    })?;
    for k in 1..total {
        let spec = rerun.clone();
        std::thread::Builder::new()
            .name(format!("shard-{k}"))
            .stack_size(SHARD_STACK)
            .spawn(move || {
                IS_SHARD.with(|s| s.set(true));
                match spec {
                    Rerun::File(p) => {
                        if let Err(e) = crate::run_file_capture(&p) {
                            eprint!("shard {k}: {e}");
                        }
                    }
                    Rerun::Source(code, name) => {
                        crate::run_source(&code, &name);
                    }
                }
            })
            .map_err(|e| HelixError::new(format!("could not spawn shard {k}: {e}"), line, col))?;
    }
    eprintln!("serving on {total} shards (SO_REUSEPORT, share-nothing)");
    Ok(())
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
    finish_connection(stream, line, col)
}

/// `listener.poll()` — a **non-blocking** accept: return a connection if a client is
/// already waiting, else `missing`. This is the one primitive the language is missing
/// to express a cooperative event loop in pure Helix — keep a list of live SSE
/// connections, `poll()` for new ones each tick, and `send` a frame to each — so a
/// single thread serves many slow streams at once (the within-core half of a
/// thread-per-core design; no shared state, no `Arc`). Mixing `poll()` and the blocking
/// `accept()` is fine: the listener is returned to blocking mode after each poll.
pub fn poll(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    let listener = match &**handle {
        NetHandle::Listener(l) => l,
        NetHandle::Conn { .. } => {
            return Err(HelixError::new(
                "`poll` works on a listener from `listen(port)`, not a connection",
                line,
                col,
            ));
        }
    };
    listener.set_nonblocking(true).ok();
    let accepted = listener.accept();
    listener.set_nonblocking(false).ok(); // restore so `accept()` still blocks if used
    match accepted {
        Ok((stream, _peer)) => finish_connection(stream, line, col),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Value::Missing),
        Err(e) => Err(HelixError::new(format!("poll failed: {e}"), line, col)),
    }
}

/// Turn a freshly accepted stream into a connection value: apply the read timeout, read
/// and parse the request, and wrap the stream for the reply. Shared by `accept`/`poll`.
fn finish_connection(stream: TcpStream, line: usize, col: usize) -> Result<Value, HelixError> {
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
        pending: RefCell::new(Vec::new()),
    })))
}

/// Append `bytes` to a streaming connection's backlog and drain as much as the kernel
/// will take **without blocking**. Returns whether the connection is still usable:
/// `false` if the peer is gone (broken pipe / reset) or the backlog passed
/// [`MAX_PENDING`] (a client too slow to keep up — dropped); `true` otherwise (fully
/// sent, or partially sent with the remainder buffered for a later `send`). Never blocks
/// and never splits a frame mid-write, so one slow client can't stall the shard.
fn push_and_flush(stream: &mut TcpStream, pending: &mut Vec<u8>, bytes: &[u8]) -> bool {
    pending.extend_from_slice(bytes);
    let mut written = 0;
    while written < pending.len() {
        match stream.write(&pending[written..]) {
            Ok(0) => break, // the send buffer is full right now
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break, // full — keep remainder
            Err(_) => return false, // broken pipe / reset → the client is gone
        }
    }
    if written > 0 {
        pending.drain(..written);
    }
    pending.len() <= MAX_PENDING
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
    // A slow reader must not hang the (single-threaded) shard mid-response: bound the
    // write. On timeout the write errors and is swallowed below, like a disconnect.
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();

    let (status, headers, body) = build_response(value, line, col)?;
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n", reason = reason_phrase(status));
    for (k, v) in &headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    // Content-Length and Connection are computed here (a custom `headers` value can't
    // override them — see `merge_headers`), so they are appended after the user's.
    head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
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

/// A built HTTP response: status code, header `(name, value)` pairs (always carrying a
/// `Content-Type`), and the body. `Content-Length`/`Connection` are added by `respond`.
type Response = (i64, Vec<(String, String)>, String);

/// Derive `(status, headers, body)` from a response value (see [`respond`]). `headers`
/// always carries a `Content-Type` (overridable) plus any the program supplied; the
/// caller adds `Content-Length`/`Connection`.
fn build_response(value: &Value, line: usize, col: usize) -> Result<Response, HelixError> {
    let json_of = |v: &Value| -> Result<String, HelixError> {
        match crate::writers::to_json(std::slice::from_ref(v), line, col)? {
            Value::Str(s) => Ok((*s).clone()),
            other => Ok(other.to_string()),
        }
    };
    // `text`/`html` stringify any value via Display (a `Dna`, number, etc. — not only a
    // `String`), so `{ text: seq.reverse_complement() }` sends the sequence text.
    let as_text = |v: &Value| match v {
        Value::Str(s) => (**s).clone(),
        other => other.to_string(),
    };
    let one = |ct: &str, body: String| (vec![("Content-Type".to_string(), ct.to_string())], body);

    let (status, (headers, body)) = match value {
        Value::Record(fields) => {
            let get = |name: &str| fields.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| v);
            let status = match get("status") {
                Some(Value::Int(n)) => *n,
                _ => 200,
            };
            let payload = if let Some(v) = get("json") {
                one("application/json", json_of(v)?)
            } else if let Some(v) = get("html") {
                one("text/html; charset=utf-8", as_text(v))
            } else if let Some(v) = get("text").or_else(|| get("body")) {
                one("text/plain; charset=utf-8", as_text(v))
            } else if get("status").is_some() || get("headers").is_some() {
                // An explicit response envelope with no body (e.g. a redirect:
                // `{ status: 302, headers: { Location: "/" } }`) → empty body.
                one("text/plain; charset=utf-8", String::new())
            } else {
                // A plain data record (no envelope fields) → JSON of the whole record.
                one("application/json", json_of(value)?)
            };
            // Merge any program-supplied response headers (record or dict of name→value).
            let mut payload = payload;
            if let Some(h) = get("headers") {
                merge_headers(&mut payload.0, h);
            }
            (status, payload)
        }
        Value::Str(s) => (200, one("text/plain; charset=utf-8", (**s).clone())),
        other => (200, one("application/json", json_of(other)?)),
    };
    Ok((status, headers, body))
}

/// Merge a program-supplied `headers` value (a record `{ Location: "/" }`, or a dict
/// `{ "Set-Cookie" => "…" }` for names that aren't identifiers) into the response
/// header list. A custom `Content-Type` replaces the auto one; `Content-Length` and
/// `Connection` are reserved (the server computes them) and silently ignored.
fn merge_headers(out: &mut Vec<(String, String)>, headers: &Value) {
    let text = |v: &Value| match v {
        Value::Str(s) => (**s).clone(),
        other => other.to_string(),
    };
    let mut add = |name: String, val: String| {
        let lname = name.to_ascii_lowercase();
        if lname == "content-length" || lname == "connection" {
            return; // server-controlled — never overridden
        }
        if lname == "content-type"
            && let Some(ct) = out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            ct.1 = val;
            return;
        }
        out.push((name, val));
    };
    match headers {
        Value::Record(fields) => {
            for (k, v) in fields.iter() {
                add(k.as_str().to_string(), text(v));
            }
        }
        Value::Dict(map) => {
            for (k, v) in map.iter() {
                if let DictKey::Str(s) = k {
                    add((**s).clone(), text(v));
                }
            }
        }
        _ => {} // a non-record/dict `headers` field is ignored
    }
}

/// `conn.sse()` — begin a Server-Sent-Events response: status `200`, `text/event-stream`,
/// no `Content-Length`, the socket kept open. Drive it with `conn.send(value)` per event.
pub fn sse(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    let (cell, pending) = match &**handle {
        NetHandle::Conn { stream, pending, .. } => (stream, pending),
        NetHandle::Listener(_) => {
            return Err(HelixError::new("`sse` works on a connection from `accept()`", line, col));
        }
    };
    let mut guard = cell.borrow_mut();
    let stream = match guard.as_mut() {
        Some(s) => s,
        None => return Err(HelixError::new("this connection is already closed", line, col)),
    };
    // Streaming mode: the socket is non-blocking so a slow client backs bytes up in
    // `pending` instead of freezing the loop. Best-effort header write; if the client is
    // already gone, the next `send` reports it.
    stream.set_nonblocking(true).ok();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    if !push_and_flush(stream, &mut pending.borrow_mut(), head.as_bytes()) {
        *guard = None; // client already gone
    }
    Ok(Value::Unit)
}

/// `conn.send(value)` — write one SSE event (`data: …\n\n`) on a streaming connection,
/// **without blocking**. Returns a **Bool**: `true` the connection is alive (the event
/// was sent or buffered), `false` the client is gone or too far behind to keep up (so the
/// producer loop drops it). A string/`Dna` is sent verbatim; any other value as JSON.
pub fn send(handle: &Rc<NetHandle>, value: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let (cell, pending) = match &**handle {
        NetHandle::Conn { stream, pending, .. } => (stream, pending),
        NetHandle::Listener(_) => {
            return Err(HelixError::new("`send` works on a connection from `accept()`", line, col));
        }
    };
    let payload = match value {
        Value::Str(s) => (**s).clone(),
        Value::Dna(s) => (**s).clone(),
        other => match crate::writers::to_json(std::slice::from_ref(other), line, col)? {
            Value::Str(s) => (*s).clone(),
            v => v.to_string(),
        },
    };
    // SSE framing: each line of the payload is its own `data:` field; a blank line ends
    // the event (so a multi-line body is delivered as one event, per the spec).
    let mut frame = String::new();
    for l in payload.split('\n') {
        frame.push_str("data: ");
        frame.push_str(l);
        frame.push('\n');
    }
    frame.push('\n');

    let mut guard = cell.borrow_mut();
    let alive = match guard.as_mut() {
        Some(s) => push_and_flush(s, &mut pending.borrow_mut(), frame.as_bytes()),
        None => false, // already closed / responded
    };
    if !alive {
        *guard = None; // the client is gone (or too slow) — drop the stream
    }
    Ok(Value::Bool(alive))
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

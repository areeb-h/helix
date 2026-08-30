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

use std::borrow::Cow;
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
/// How an open streaming response is framed.
///
/// `send` writes "the next piece of whatever you started", and what a piece looks like on
/// the wire is exactly this. Keeping it on the connection is what lets ONE verb serve both
/// an SSE event stream and a chunked document, instead of a second write-verb whose only
/// difference is framing — and `write` was not available for that anyway: it is already a
/// builtin (`print`/`emit`/`write`), so a `conn.write` would have been a second meaning
/// for a word the language already spends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Streaming {
    /// No streaming response open; `send` frames as SSE, which is what it always did.
    No,
    /// `sse()` — `data: …\n\n` events.
    Sse,
    /// `stream()` on an HTTP/1.1 client — `<hex len>\r\n<bytes>\r\n`, ended by a
    /// zero-length chunk that `close` writes.
    Chunked,
    /// `stream()` on an HTTP/1.0 client — raw bytes, framed by the close itself, because
    /// 1.0 has no chunked encoding. Decidable only because the request carries `version`.
    Raw,
}

pub enum NetHandle {
    Listener(TcpListener),
    Conn {
        request: Value,
        stream: RefCell<Option<TcpStream>>,
        /// Outbound bytes not yet accepted by the kernel send buffer. For a streaming
        /// (`sse`) connection the socket is non-blocking, so a slow client backs bytes
        /// up here instead of freezing the event loop; they drain on later `send`s.
        pending: RefCell<Vec<u8>>,
        /// Accumulated inbound request bytes for the cooperative event loop (used by
        /// `accept_poll`/`poll_request`): a non-blocking read appends here, and a request is
        /// parsed once the buffer holds a complete one. Empty/unused on the blocking `accept`
        /// path.
        inbuf: RefCell<Vec<u8>>,
        /// Event-loop connection state: `open` is false once the peer closed (EOF); `event`
        /// marks a keep-alive event-loop connection, so `respond` keeps the socket open and
        /// sends `Connection: keep-alive` instead of closing.
        open: Cell<bool>,
        event: Cell<bool>,
        /// WHICH KIND of streaming response is open, which decides how `send` frames the
        /// next piece and whether `close` owes a terminator. A bool could not express it:
        /// an SSE stream and an HTTP/1.0 chunkless stream are both "not chunked" and are
        /// framed completely differently.
        stream_mode: Cell<Streaming>,
        /// The other end of the socket, as the `{address, port}` record the request
        /// carries — BUILT ONCE, here, because it cannot change while the socket is open
        /// and a keep-alive connection serves many requests through it. Also the only
        /// place it could live for the event-loop path, which parses its request an
        /// arbitrary number of polls after the `SocketAddr` was available. See
        /// `peer_value` for why it is not called "client".
        peer: Value,
    },
    /// An open HTTP response body being read incrementally (`http_stream`) — the pull-based
    /// streaming *client*. Holds the response `status` and a buffered reader consumed
    /// line-by-line by `.next()`, so a model's token stream (Ollama NDJSON, OpenAI SSE) is
    /// forwarded chunk-by-chunk by the program's own loop — the client mirror of the
    /// `accept`→`send` server loop. `reader` is `None` once EOF is reached.
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    HttpStream {
        status: i64,
        reader: RefCell<Option<std::io::BufReader<Box<dyn std::io::Read>>>>,
    },
    /// A cookie jar (ADR 0031 §4): explicit, program-held state that a request threads
    /// through to store what a response sets and send what a later request should carry.
    /// Behind the `Net` value so it reuses that dispatch; interior-mutable because a
    /// request mutates it while the program holds it by shared reference.
    CookieJar(crate::cookiejar::CookieJar),
}

impl Drop for NetHandle {
    /// When a connection closes, release its still-buffered SSE backlog from the shard-wide
    /// total ([`SSE_PENDING`]) so the accounting a dropped slow client leaves behind can't
    /// permanently shrink the budget for the survivors.
    fn drop(&mut self) {
        if let NetHandle::Conn { pending, inbuf, .. } = self {
            let n = pending.borrow().len();
            if n > 0 {
                SSE_PENDING.with(|c| c.set(c.get().saturating_sub(n)));
            }
            let m = inbuf.borrow().len();
            if m > 0 {
                INBUF_PENDING.with(|c| c.set(c.get().saturating_sub(m)));
            }
        }
    }
}

/// Cap on a request body we will buffer (a larger `Content-Length` is truncated to
/// this) — a single malicious/oversized request must not OOM the process.
const MAX_BODY: usize = 64 << 20; // 64 MiB

/// Cap on a single request-head line (the request line or one header). Without it,
/// `read_line` would grow a `String` until a newline arrives — a client sending one
/// endless header line (no `\n`) could OOM the process before the read timeout fires.
const MAX_HEADER_LINE: usize = 16 << 10; // 16 KiB (matches nginx's default)

/// Cap on the number of request headers. Without it, a client sending millions of tiny
/// headers would grow the header map unbounded (header-bombing) within the read timeout.
const MAX_HEADER_COUNT: usize = 1000;

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
    /// A program built into this executable: `(modules, entry index)`. A shard cannot
    /// re-read a path, because a bundled program has no path -- its source lives in the
    /// overlay appended to this binary.
    Archive(Vec<(String, String)>, usize),
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
    /// Sum of un-drained SSE backlog across *all* this shard's connections. The per-connection
    /// [`MAX_PENDING`] bounds one slow client; this bounds a *fleet* of them, so N slow SSE
    /// clients can't each hold [`MAX_PENDING`] and add up to N × 4 MiB. A connection whose
    /// `send` would push the shard total over [`SSE_GLOBAL_PENDING`] is dropped.
    static SSE_PENDING: Cell<usize> = const { Cell::new(0) };
    /// Sum of buffered *inbound* request bytes across all this shard's connections —
    /// the read-side mirror of [`SSE_PENDING`]. The per-connection cap bounds one
    /// oversized request; this bounds a fleet of them (eight concurrent 64 MiB
    /// uploads must not OOM a 512 MB box). Released on drain and in `Drop`.
    static INBUF_PENDING: Cell<usize> = const { Cell::new(0) };
    /// Scratch for `poll_request`'s non-blocking reads. On the stack it was zeroed on
    /// every call — and the call runs per connection per tick, so at 50 connections
    /// that was 400 KiB of pure zeroing per event-loop tick.
    static READ_SCRATCH: RefCell<[u8; 8192]> = const { RefCell::new([0u8; 8192]) };
}

/// Per-shard cap on the *total* SSE backlog across all connections (see [`SSE_PENDING`]).
const SSE_GLOBAL_PENDING: usize = 64 << 20; // 64 MiB per shard

/// Per-shard cap on total buffered inbound bytes (see [`INBUF_PENDING`]). Must be
/// >= [`MAX_BODY`] or a single legal max-size request could never be received.
const INBUF_GLOBAL: usize = 64 << 20; // 64 MiB per shard

/// Stack size for every eval thread — the main interpreter thread and each shard
/// worker share this, so primary and shards can never diverge on recursion depth.
/// Measured in the gate build: a Helix call frame costs ~1 KiB of native stack, so
/// the full `MAX_CALL_DEPTH` (20k) touches ~19 MiB — 128 MiB is ~6x headroom even
/// for fat frames. The size matters on small machines because a thread-stack
/// reservation is *committed* memory under strict overcommit
/// (`vm.overcommit_memory=2`, common on small VPSes): the old 1 GiB per shard meant
/// 4 shards could not even spawn on a 2 GB box. Debug frames are ~25x larger, so
/// debug builds keep 1 GiB. `HELIX_STACK_MB` overrides for the rare program that
/// recurses deep with huge frames.
pub(crate) fn eval_stack_size() -> usize {
    if let Ok(v) = std::env::var("HELIX_STACK_MB")
        && let Ok(mb) = v.trim().parse::<usize>()
        && mb > 0
    {
        return mb << 20;
    }
    if cfg!(debug_assertions) { 1 << 30 } else { 128 << 20 }
}

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
            .stack_size(eval_stack_size())
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
                    Rerun::Archive(modules, entry) => {
                        if let Err(e) = crate::run_archive_capture(modules, entry) {
                            eprint!("shard {k}: {e}");
                        }
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
        _ => {
            return Err(HelixError::new(
                "`accept` works on a listener from `listen(port)`, not a connection",
                line,
                col,
            ));
        }
    };
    // Loop so a malformed / oversized request — already answered with a best-effort 400 and
    // closed inside `finish_connection` — is skipped rather than propagated. One bad or
    // hostile client (a header bomb, a mid-request disconnect) must never take down the
    // accept loop; only a genuine *listener* failure is returned to the program.
    loop {
        let (stream, peer) = listener
            .accept()
            .map_err(|e| HelixError::new(format!("accept failed: {e}"), line, col))?;
        match finish_connection(stream, peer, line, col) {
            Ok(conn) => return Ok(conn),
            Err(_) => continue,
        }
    }
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
        _ => {
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
        // A malformed request was answered with a 400 and dropped inside `finish_connection`;
        // there's no valid connection to hand back this tick, so report `missing` (as if none
        // had arrived) — the cooperative loop just continues.
        Ok((stream, peer)) => match finish_connection(stream, peer, line, col) {
            Ok(conn) => Ok(conn),
            Err(_) => Ok(Value::Missing),
        },
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Value::Missing),
        Err(e) => Err(HelixError::new(format!("poll failed: {e}"), line, col)),
    }
}

/// `stream.next()` — read the next line/chunk from an `http_stream` response body, returning
/// it as a `String` (trailing newline stripped) or `missing` at end of stream. The program
/// drives this in its own loop (the client mirror of `accept`), so a model's token stream is
/// forwarded chunk-by-chunk. A read error, or a non-stream handle, ends the stream.
pub fn stream_next(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    use std::io::BufRead;
    let reader_cell = match &**handle {
        NetHandle::HttpStream { reader, .. } => reader,
        _ => return Err(HelixError::new("`next` works on an `http_stream` handle", line, col)),
    };
    let mut guard = reader_cell.borrow_mut();
    let Some(rdr) = guard.as_mut() else {
        return Ok(Value::Missing); // already at EOF
    };
    let mut buf = String::new();
    match rdr.read_line(&mut buf) {
        Ok(0) => {
            *guard = None; // EOF — drop the reader (closes the connection)
            Ok(Value::Missing)
        }
        Ok(_) => {
            let keep = buf.trim_end_matches(['\n', '\r']).len();
            buf.truncate(keep);
            Ok(Value::Str(Rc::new(buf)))
        }
        // A per-chunk timeout (`timeout_ms`) means the server is idle, not gone: the socket
        // read hit its deadline (`TimedOut`/`WouldBlock`) with no data. Keep the stream OPEN
        // — the caller can retry `.next()` or `.close()` — and raise a catchable error, so a
        // hung server is distinguishable from the `missing` that signals clean end-of-stream.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {
            Err(HelixError::new("`next` timed out waiting for the next chunk", line, col)
                .hint("the server sent nothing within `timeout_ms`; retry `.next()`, or `.close()` to give up."))
        }
        // Any other read error ends the stream (a reset/broken connection is, for the
        // program's purposes, the end) — drop the reader and report EOF.
        Err(_) => {
            *guard = None;
            Ok(Value::Missing)
        }
    }
}

/// `stream.status()` — the HTTP status of an `http_stream` response (e.g. `200`).
pub fn stream_status(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    match &**handle {
        NetHandle::HttpStream { status, .. } => Ok(Value::Int(*status)),
        _ => Err(HelixError::new("`status` works on an `http_stream` handle", line, col)),
    }
}

/// `stream.close()` — abandon the stream early (a stop-word, a token budget, a user hitting
/// stop) without draining to EOF: drop the reader, which closes the underlying socket and frees
/// it now. Idempotent — closing an already-closed or exhausted stream is a no-op — and a
/// subsequent `.next()` returns `missing`, exactly as at EOF. Returns `missing`.
pub fn stream_close(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    match &**handle {
        NetHandle::HttpStream { reader, .. } => {
            *reader.borrow_mut() = None; // drop the reader → close the socket
            Ok(Value::Missing)
        }
        // A SERVER CONNECTION CLOSES THE SAME WAY, and until now it could not close at
        // all. `Net` had fifteen methods and the only `close` was the outbound client's,
        // so the sole way to hang up on a peer was to release the last reference to the
        // handle and hope. That is not a close: a server could not honour
        // `Connection: close` promptly, shed a slow client, or drop an abusive one.
        //
        // THE COST WAS A REMOTE DENIAL OF SERVICE, found in a Helix web app. Its accept
        // loop called `close()` on an accepted connection — the obvious spelling — this
        // arm refused, the raise unwound the accept loop, and the process died. Three
        // `curl --http1.0 -H 'Connection: close'` requests took down a six-shard server:
        // no auth, no body, no volume, using a header any HTTP/1.0 client sends by
        // default. On the sharded build it was worse than a crash — shards died one at a
        // time while the server kept answering, so it read as healthy until it was not.
        //
        // The plumbing was already here: `Conn` owns its `TcpStream` behind an `Option`
        // for exactly this, and `open` is the flag `is_open` reports. Dropping the stream
        // closes the socket; setting `open` false makes the connection agree with what
        // the program can already observe.
        NetHandle::Conn { stream, open, pending, stream_mode, .. } => {
            // A CHUNKED RESPONSE ENDS WITH A ZERO-LENGTH CHUNK. Dropping the socket
            // without it leaves the client waiting for an end that never arrives, which
            // presents as a hang rather than as the truncation it is.
            // ONE GUARD for both the terminator and the drop. Taking `borrow_mut`
            // twice in sequence works, but every extra borrow is a place a later edit
            // can nest one inside another, and a nested borrow is a host abort
            // (ADR 0024) reachable from a client's traffic.
            let mut g = stream.borrow_mut();
            if stream_mode.replace(Streaming::No) == Streaming::Chunked
                && let Some(st) = g.as_mut()
            {
                // `pending` is a DIFFERENT cell, so this cannot conflict with `g`.
                    let _ = push_and_flush(st, &mut pending.borrow_mut(), b"0\r\n\r\n");
            }
            *g = None;
            drop(g);
            open.set(false);
            // Buffered SSE bytes can never be sent now, so release them from the
            // shard-wide budget here rather than leaving it shrunk until the handle
            // drops — the same accounting `Drop` does, and clearing keeps it from
            // running twice.
            let mut p = pending.borrow_mut();
            let n = p.len();
            if n > 0 {
                p.clear();
                SSE_PENDING.with(|c| c.set(c.get().saturating_sub(n)));
            }
            // IDEMPOTENT, like the `http_stream` arm above: closing twice is what a
            // handler does when it also closes on the way out of an error path.
            Ok(Value::Missing)
        }
        NetHandle::Listener(_) => Err(HelixError::new(
            "`close` works on a connection or an `http_stream`, not on a listener",
            line,
            col,
        )
        .hint("a listener stops accepting when the program stops holding it.")),
        NetHandle::CookieJar(_) => Err(HelixError::new(
            "`close` works on a connection or an `http_stream`, not on a cookie jar",
            line,
            col,
        )
        .hint("use `clear()` to empty a jar.")),
    }
}

/// Turn a freshly accepted stream into a connection value: apply the read timeout, read
/// and parse the request, and wrap the stream for the reply. Shared by `accept`/`poll`.
fn finish_connection(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let peer = peer_value(&peer);
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
    // Read the request from a clone so the original stream stays free for the reply
    // (both refer to the same socket; the read clone is dropped when this returns).
    let read_side = stream
        .try_clone()
        .map_err(|e| HelixError::new(format!("could not read the connection: {e}"), line, col))?;
    match parse_request(read_side, &peer, line, col) {
        Ok(request) => Ok(Value::Net(Rc::new(NetHandle::Conn {
            request,
            stream: RefCell::new(Some(stream)),
            pending: RefCell::new(Vec::new()),
            // Blocking one-shot connection: the event-loop fields are inert.
            inbuf: RefCell::new(Vec::new()),
            open: Cell::new(true),
            event: Cell::new(false),
            stream_mode: Cell::new(Streaming::No),
            peer,
        }))),
        Err(e) => {
            // A malformed / oversized request (a header bomb, a bad request line, a client
            // that vanished mid-request). Answer with a best-effort `400` and drop the
            // connection; `accept`/`poll` skip this Err so the server keeps serving.
            let mut s = stream;
            s.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
            let _ = s.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            Err(e)
        }
    }
}

/// Append `bytes` to a streaming connection's backlog and drain as much as the kernel
/// will take **without blocking**. Returns whether the connection is still usable:
/// `false` if the peer is gone (broken pipe / reset) or the backlog passed
/// [`MAX_PENDING`] (a client too slow to keep up — dropped); `true` otherwise (fully
/// sent, or partially sent with the remainder buffered for a later `send`). Never blocks
/// and never splits a frame mid-write, so one slow client can't stall the shard.
fn push_and_flush(stream: &mut TcpStream, pending: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let before = pending.len();
    pending.extend_from_slice(bytes);
    let mut written = 0;
    while written < pending.len() {
        match stream.write(&pending[written..]) {
            Ok(0) => break, // the send buffer is full right now
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break, // full — keep remainder
            Err(_) => {
                // Peer gone: release this connection's whole backlog from the shard total,
                // since the connection is about to be dropped.
                SSE_PENDING.with(|c| c.set(c.get().saturating_sub(before)));
                return false;
            }
        }
    }
    if written > 0 {
        pending.drain(..written);
    }
    // Keep the shard-wide total in step: replace this connection's old contribution with its
    // new one (`total - before + now`). A drop later releases whatever remains (see NetHandle's
    // Drop impl).
    let now = pending.len();
    let total = SSE_PENDING.with(|c| {
        let t = c.get().saturating_sub(before) + now;
        c.set(t);
        t
    });
    // Usable unless this client alone is too far behind, or the whole shard's backlog is.
    now <= MAX_PENDING && total <= SSE_GLOBAL_PENDING
}

/// `conn.request()` — the parsed request record for this connection.
pub fn request(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    match &**handle {
        NetHandle::Conn { request, .. } => Ok(request.clone()),
        _ => Err(HelixError::new(
            "`request` works on a connection from `accept()`, not a listener",
            line,
            col,
        )),
    }
}

/// The five request-record keys, interned ONCE. A `Symbol` is a `u32`, so this is
/// `Copy`; interning per request was a locked hashmap lookup times five times every
/// request (25 at 55k req/s). Both engines already read these keys by their `Symbol`.
#[allow(clippy::type_complexity)]
fn request_keys() -> (Symbol, Symbol, Symbol, Symbol, Symbol, Symbol, Symbol) {
    use std::sync::OnceLock;
    static KEYS: OnceLock<(Symbol, Symbol, Symbol, Symbol, Symbol, Symbol, Symbol)> =
        OnceLock::new();
    *KEYS.get_or_init(|| {
        (
            Symbol::intern("method"),
            Symbol::intern("path"),
            Symbol::intern("query"),
            Symbol::intern("headers"),
            Symbol::intern("body"),
            Symbol::intern("peer"),
            Symbol::intern("version"),
        )
    })
}

/// The TCP peer as `{address, port}`.
///
/// **`peer`, not `client` or `ip`, and the name IS the documentation.** This is the other
/// end of the socket. Behind a reverse proxy it is the proxy, and `X-Forwarded-For` is
/// what carries the original client — a header the peer controls, so it means something
/// only when the proxy is one you run and it OVERWRITES rather than appends. Naming this
/// field `client_ip` would have made a lie convenient.
///
/// A RECORD, not `"127.0.0.1:54321"`. Rate limiting groups by address, and splitting the
/// string to get one is where the naive version meets IPv6 (`[::1]:8080`). Go hands over
/// the string and every user re-parses it; this hands over the parts already separated.
///
/// Until now the address was accepted and dropped on the floor — literally `_peer` — so
/// telling two clients apart was not awkward, it was INEXPRESSIBLE.
fn peer_value(addr: &std::net::SocketAddr) -> Value {
    use std::sync::OnceLock;
    static KEYS: OnceLock<(Symbol, Symbol)> = OnceLock::new();
    let k = *KEYS.get_or_init(|| (Symbol::intern("address"), Symbol::intern("port")));
    Value::Record(Rc::new(vec![
        (k.0, Value::Str(Rc::new(addr.ip().to_string()))),
        (k.1, Value::Int(addr.port() as i64)),
    ]))
}

/// The protocol version from a request line's third token: `HTTP/1.1` -> `"1.1"`.
///
/// **An absent or unrecognisable token answers `"1.0"`, deliberately.** A server branches
/// on this to decide whether to keep a connection alive, and 1.0's rule is *close unless
/// asked otherwise* where 1.1's is *keep alive unless asked otherwise*. Guessing the
/// version that closes is the guess that cannot leak a connection.
fn request_version(tok: Option<&str>) -> &'static str {
    match tok.and_then(|t| t.strip_prefix("HTTP/")) {
        Some("1.1") => "1.1",
        Some("2.0") | Some("2") => "2.0",
        Some("0.9") => "0.9",
        _ => "1.0",
    }
}

/// The six response-envelope keys, interned once — `build_response` probes them per
/// response, and `Symbol::as_str` takes the global interner's read lock every call.
fn respond_keys() -> (Symbol, Symbol, Symbol, Symbol, Symbol, Symbol) {
    use std::sync::OnceLock;
    static KEYS: OnceLock<(Symbol, Symbol, Symbol, Symbol, Symbol, Symbol)> = OnceLock::new();
    *KEYS.get_or_init(|| {
        (
            Symbol::intern("status"),
            Symbol::intern("json"),
            Symbol::intern("html"),
            Symbol::intern("text"),
            Symbol::intern("body"),
            Symbol::intern("headers"),
        )
    })
}

/// Pre-built `Rc`s for the strings every request repeats: the common verbs, and the
/// empty string a typical GET's `query`/`body` share. One allocation per shard
/// instead of three per request. The COW paths that `Rc::get_mut` a pooled string
/// see `strong_count >= 2` and take their clone arm — byte-identical behavior, and
/// cloning a verb or an empty string is trivial.
struct CommonStrs {
    verbs: [(&'static str, Rc<String>); 7],
    empty: Rc<String>,
    /// The protocol version is one of four fixed strings, so it belongs here for the
    /// same reason the verbs do: `Rc::new(version.to_string())` per request is an
    /// allocation for a value that is drawn from a set of four.
    versions: [(&'static str, Rc<String>); 4],
}

impl CommonStrs {
    fn method(&self, m: &str) -> Rc<String> {
        match self.verbs.iter().find(|(k, _)| *k == m) {
            Some((_, v)) => Rc::clone(v),
            None => Rc::new(m.to_string()),
        }
    }

    fn nonempty(&self, s: String) -> Rc<String> {
        if s.is_empty() { Rc::clone(&self.empty) } else { Rc::new(s) }
    }

    fn version(&self, v: &str) -> Rc<String> {
        match self.versions.iter().find(|(k, _)| *k == v) {
            Some((_, r)) => Rc::clone(r),
            // Unreachable: `request_version` answers from a closed set of four.
            None => Rc::new(v.to_string()),
        }
    }
}

thread_local! {
    static COMMON_STRS: CommonStrs = CommonStrs {
        verbs: ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH", "OPTIONS"]
            .map(|v| (v, Rc::new(v.to_string()))),
        empty: Rc::new(String::new()),
        versions: ["1.0", "1.1", "2.0", "0.9"].map(|v| (v, Rc::new(v.to_string()))),
    };
}

/// Parse one HTTP/1.1 request into a record `{method, path, query, headers, body}`.
/// Headers are a `Dict` keyed by lowercased name (HTTP names are case-insensitive).
fn parse_request(
    stream: TcpStream,
    peer: &Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let err = |e: std::io::Error| HelixError::new(format!("reading the request failed: {e}"), line, col);
    let mut reader = BufReader::new(stream);

    // Request line: METHOD TARGET HTTP/1.1. Bounded like a header line so a client can't
    // stream an endless first line to exhaust memory (`take` caps the bytes read).
    let mut request_line = String::new();
    (&mut reader)
        .take(MAX_HEADER_LINE as u64)
        .read_line(&mut request_line)
        .map_err(err)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/").to_string();
    // The third token was read and discarded until now, which is why a server could not
    // tell HTTP/1.0 from 1.1 and so could implement neither correctly.
    let version = request_version(parts.next());
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    // Headers until the blank line; capture Content-Length for the body. Each line is
    // read through `take(MAX_HEADER_LINE)` (bounds one giant header) and the count is
    // capped (bounds header-bombing) — the body already has MAX_BODY, so the head needs
    // its own limits to be DoS-safe.
    let mut headers: Vec<(String, String)> = Vec::with_capacity(8);
    let mut content_length = 0usize;
    let mut header_count = 0usize;
    loop {
        let mut h = String::new();
        let n = (&mut reader).take(MAX_HEADER_LINE as u64).read_line(&mut h).map_err(err)?;
        // A read that stopped at the cap without reaching a newline is an over-long line.
        if n >= MAX_HEADER_LINE && !h.ends_with('\n') {
            return Err(HelixError::new(
                format!("request header line exceeds {MAX_HEADER_LINE} bytes"),
                line,
                col,
            ));
        }
        let t = h.trim_end_matches(['\r', '\n']);
        if n == 0 || t.is_empty() {
            break;
        }
        header_count += 1;
        if header_count > MAX_HEADER_COUNT {
            return Err(HelixError::new(
                format!("request has more than {MAX_HEADER_COUNT} headers"),
                line,
                col,
            )
            .hint("this looks like a header-bombing request; a well-formed one has far fewer."));
        }
        if let Some((k, v)) = t.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.push((key, val));
        }
    }

    // Body (clamped to MAX_BODY so an oversized Content-Length can't exhaust memory).
    let to_read = content_length.min(MAX_BODY);
    let mut body = vec![0u8; to_read];
    if to_read > 0 {
        reader.read_exact(&mut body).map_err(err)?;
    }
    let body = String::from_utf8_lossy(&body).into_owned();

    let k = request_keys();
    let (method, query, body, version) = COMMON_STRS.with(|c| {
        (c.method(method), c.nonempty(query), c.nonempty(body), c.version(version))
    });
    let record = vec![
        (k.0, Value::Str(method)),
        (k.1, Value::Str(Rc::new(path))),
        (k.2, Value::Str(query)),
        (k.3, Value::Headers(Rc::new(headers))),
        (k.4, Value::Str(body)),
        (k.5, peer.clone()),
        (k.6, Value::Str(version)),
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
    let (cell, event, open) = match &**handle {
        NetHandle::Conn { stream, event, open, .. } => (stream, event, open),
        _ => {
            return Err(HelixError::new(
                "`respond` works on a connection from `accept()`, not a listener",
                line,
                col,
            ));
        }
    };

    let (status, headers, body) = build_response(value, line, col)?;
    // Build the whole message (head + body) in one buffer and send it with a single
    // write_all. Two writes meant two packets per response on the TCP_NODELAY keep-alive
    // path; and `write!` into a pre-sized String avoids the throwaway format!() the
    // Content-Length/Connection tail used to allocate. `fmt::Write` is imported
    // anonymously so it doesn't shadow the io::Write used for the socket below.
    use std::fmt::Write as _;
    // Sized exactly (status line + computed tail fit in 96): `64 + body` guaranteed
    // one realloc-and-copy of the whole message on every response.
    let head_len: usize = headers.iter().map(|(k, v)| k.len() + v.len() + 4).sum();
    let mut head = String::with_capacity(96 + head_len + body.len());
    let _ = write!(head, "HTTP/1.1 {status} {reason}\r\n", reason = reason_phrase(status));
    for (k, v) in &headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    // Content-Length and Connection are computed here (a custom `headers` value can't
    // override them — see `merge_headers`), so they are appended after the user's. An
    // event-loop connection (`accept_poll`) is kept alive for the next request; a plain
    // blocking connection is closed after the reply.
    let ka = event.get();
    let conn_hdr = if ka { "keep-alive" } else { "close" };
    let _ = write!(head, "Content-Length: {}\r\nConnection: {conn_hdr}\r\n\r\n", body.len());
    head.push_str(&body);
    let write = |s: &mut TcpStream| -> std::io::Result<()> {
        s.write_all(head.as_bytes())?;
        s.flush()
    };

    if ka {
        // Keep-alive event-loop connection: write via a borrow and leave the socket open for
        // `poll_request` to read the next request. A write failure means the client left —
        // close and mark it done so the event loop drops it.
        let mut guard = cell.borrow_mut();
        let Some(s) = guard.as_mut() else { return Ok(Value::Unit) };
        if write(s).is_err() {
            let _ = s.shutdown(Shutdown::Both);
            *guard = None;
            open.set(false);
        }
        return Ok(Value::Unit);
    }

    // Blocking one-shot connection: take the stream, write, and close.
    let mut stream = match cell.borrow_mut().take() {
        Some(s) => s,
        None => {
            return Err(HelixError::new("this connection has already been responded to", line, col));
        }
    };
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
    // A write failure means the client went away (broken pipe / connection reset) — a
    // routine event for a server, never the program's fault. Best-effort: drop the
    // undeliverable response and keep the program's accept loop alive.
    let _ = write(&mut stream);
    let _ = stream.shutdown(Shutdown::Both);
    Ok(Value::Unit)
}

/// The result of trying to parse one request out of a connection's accumulated bytes.
enum BufParse {
    /// Not enough bytes yet — keep the buffer and read more later.
    Incomplete,
    /// A full request plus the number of bytes it consumed (drained from the buffer).
    Complete(Box<Value>, usize),
    /// The head is malformed or over its size limit — the connection should be closed.
    Malformed,
}

/// Find the first occurrence of `needle` in `hay` (for locating the `\r\n\r\n` head end).
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Parse one HTTP request out of accumulated bytes (the cooperative event loop's non-blocking
/// path). Unlike [`parse_request`], it never reads/blocks — it works on what's buffered so far,
/// returning [`BufParse::Incomplete`] until a whole request (head + `Content-Length` body) is
/// present. The DoS caps ([`MAX_HEADER_LINE`]/[`MAX_HEADER_COUNT`]/[`MAX_BODY`]) apply here too.
fn parse_request_buf(buf: &[u8], peer: &Value) -> BufParse {
    let Some(head_end) = find_sub(buf, b"\r\n\r\n") else {
        // No end-of-head yet. Refuse an unbounded head (slow-loris) before it grows forever.
        return if buf.len() > MAX_HEADER_LINE * 8 {
            BufParse::Malformed
        } else {
            BufParse::Incomplete
        };
    };
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/").to_string();
    // The third token was read and discarded until now, which is why a server could not
    // tell HTTP/1.0 from 1.1 and so could implement neither correctly.
    let version = request_version(parts.next());
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    // Ordered pairs, the wire's own casing — a `Headers` value, matching the blocking
    // parser. `content-length` is matched case-insensitively (it can arrive any way) but
    // the stored name is left as sent, because that is what `Headers` promises.
    let mut headers: Vec<(String, String)> = Vec::with_capacity(8);
    let mut content_length = 0usize;
    let mut count = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        count += 1;
        if count > MAX_HEADER_COUNT {
            return BufParse::Malformed;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = val.parse().unwrap_or(0);
            }
            headers.push((key.to_string(), val.to_string()));
        }
    }
    let content_length = content_length.min(MAX_BODY);
    let total = head_end + 4 + content_length;
    if buf.len() < total {
        return BufParse::Incomplete; // waiting for the body
    }
    let body = String::from_utf8_lossy(&buf[head_end + 4..total]).into_owned();
    let k = request_keys();
    let (method, query, body, version) = COMMON_STRS.with(|c| {
        (c.method(method), c.nonempty(query), c.nonempty(body), c.version(version))
    });
    let record = vec![
        (k.0, Value::Str(method)),
        (k.1, Value::Str(Rc::new(path))),
        (k.2, Value::Str(query)),
        (k.3, Value::Headers(Rc::new(headers))),
        (k.4, Value::Str(body)),
        (k.5, peer.clone()),
        (k.6, Value::Str(version)),
    ];
    BufParse::Complete(Box::new(Value::Record(Rc::new(record))), total)
}

/// `listener.accept_poll()` — the cooperative event loop's non-blocking accept: return a
/// **persistent keep-alive connection** (its socket set non-blocking + `TCP_NODELAY`) if a
/// client is waiting, else `missing`. Unlike `accept`/`poll`, the request is NOT parsed here —
/// the program drives `poll_request`/`respond` across many connections in one loop, so a single
/// thread serves many keep-alive clients interleaved (no per-connection blocking).
pub fn accept_poll(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    let listener = match &**handle {
        NetHandle::Listener(l) => l,
        _ => {
            return Err(HelixError::new(
                "`accept_poll` works on a listener from `listen(port)`, not a connection",
                line,
                col,
            ))
        }
    };
    listener.set_nonblocking(true).ok();
    let accepted = listener.accept();
    listener.set_nonblocking(false).ok();
    match accepted {
        Ok((stream, peer)) => {
            stream.set_nonblocking(true).ok();
            stream.set_nodelay(true).ok();
            Ok(Value::Net(Rc::new(NetHandle::Conn {
                request: Value::Missing,
                stream: RefCell::new(Some(stream)),
                pending: RefCell::new(Vec::new()),
                inbuf: RefCell::new(Vec::new()),
                open: Cell::new(true),
                event: Cell::new(true),
                stream_mode: Cell::new(Streaming::No),
                peer: peer_value(&peer),
            })))
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Value::Missing),
        Err(e) => Err(HelixError::new(format!("accept_poll failed: {e}"), line, col)),
    }
}

/// `conn.poll_request()` — the cooperative event loop's non-blocking read: drain whatever bytes
/// are available (without blocking), and return the next request if a whole one has arrived,
/// else `missing`. `missing` means either "not ready yet" (still open — check `is_open`) or
/// "closed" (`is_open` is now false). A malformed/oversized request closes the connection.
pub fn poll_request(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    let (stream, inbuf, open, peer) = match &**handle {
        NetHandle::Conn { stream, inbuf, open, peer, .. } => (stream, inbuf, open, peer),
        _ => {
            return Err(HelixError::new(
                "`poll_request` works on a connection from `accept_poll()`",
                line,
                col,
            ))
        }
    };
    if !open.get() {
        return Ok(Value::Missing);
    }
    // Non-blocking drain of everything currently readable into the accumulation buffer.
    {
        let mut guard = stream.borrow_mut();
        let Some(s) = guard.as_mut() else {
            open.set(false);
            return Ok(Value::Missing);
        };
        let mut buf = inbuf.borrow_mut();
        READ_SCRATCH.with(|sc| {
            let mut tmp = sc.borrow_mut();
            loop {
                match s.read(&mut tmp[..]) {
                    Ok(0) => {
                        open.set(false); // EOF: peer closed
                        break;
                    }
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        let total = INBUF_PENDING.with(|c| {
                            let t = c.get() + n;
                            c.set(t);
                            t
                        });
                        // Per-connection AND per-shard bounds, the same two-level
                        // shape the SSE side uses — dropping the offender, exactly
                        // as `push_and_flush` does over there.
                        if buf.len() > MAX_HEADER_LINE * 8 + MAX_BODY || total > INBUF_GLOBAL {
                            open.set(false); // runaway: drop it
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break, // nothing more now
                    Err(_) => {
                        open.set(false);
                        break;
                    }
                }
            }
        });
    }
    // Try to carve a complete request out of the buffer.
    let mut buf = inbuf.borrow_mut();
    match parse_request_buf(&buf, peer) {
        BufParse::Complete(req, consumed) => {
            buf.drain(..consumed);
            INBUF_PENDING.with(|c| c.set(c.get().saturating_sub(consumed)));
            // `drain` keeps capacity: without this a connection that once carried a
            // large body holds its high-water buffer for its whole keep-alive life
            // (300 idle conns x one historical 1 MiB POST = 300 MiB pinned).
            if buf.capacity() > MAX_HEADER_LINE && buf.len() <= MAX_HEADER_LINE / 2 {
                buf.shrink_to(MAX_HEADER_LINE);
            }
            Ok(*req)
        }
        BufParse::Incomplete => Ok(Value::Missing),
        BufParse::Malformed => {
            open.set(false);
            Ok(Value::Missing)
        }
    }
}

/// `conn.is_open()` — whether a cooperative-event-loop connection is still open (the peer
/// hasn't closed and it hasn't been dropped). The loop keeps open connections and discards
/// closed ones.
pub fn is_open(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    match &**handle {
        NetHandle::Conn { open, .. } => Ok(Value::Bool(open.get())),
        _ => Err(HelixError::new("`is_open` works on a connection", line, col)),
    }
}

/// `listener.wait(conns, timeout_ms)` — block until the listener has a pending connection, OR
/// any connection in `conns` has readable data, OR `timeout_ms` elapses. This is the readiness
/// primitive (`poll(2)`) that turns the cooperative event loop from a busy-spin (100% CPU) into
/// a sleep-until-ready loop: at full load `poll` returns immediately (work is waiting) so
/// throughput is unaffected, but an idle server blocks here instead of spinning — ~0% CPU. The
/// small timeout also bounds the rare case of an already-buffered pipelined request.
pub fn wait(
    handle: &Rc<NetHandle>,
    conns: &Value,
    timeout_ms: i64,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    if !matches!(&**handle, NetHandle::Listener(_)) {
        return Err(HelixError::new("`wait` works on a listener from `listen(port)`", line, col));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // One slot for the listener + one per connection: pre-size so the per-tick fill
        // doesn't reallocate (this runs every event-loop tick).
        let cap = 1 + if let Value::Array(arr) = conns { arr.len() } else { 0 };
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(cap);
        let mut push_fd = |fd: std::os::fd::RawFd| {
            fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        };
        if let NetHandle::Listener(l) = &**handle {
            push_fd(l.as_raw_fd());
        }
        if let Value::Array(arr) = conns {
            for v in arr.to_values().iter() {
                if let Value::Net(h) = v
                    && let NetHandle::Conn { stream, .. } = &**h
                    && let Some(s) = stream.borrow().as_ref()
                {
                    push_fd(s.as_raw_fd());
                }
            }
        }
        let t = timeout_ms.clamp(0, i32::MAX as i64) as i32;
        // SAFETY: `fds` is a valid, correctly-sized slice of `pollfd` for the call's duration.
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, t);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = conns;
        std::thread::sleep(Duration::from_millis(timeout_ms.clamp(0, 1000) as u64));
    }
    Ok(Value::Unit)
}

/// A built HTTP response: status code, header `(name, value)` pairs (always carrying a
/// `Content-Type`), and the body. `Content-Length`/`Connection` are added by `respond`.
type Response<'a> = (i64, Vec<(Cow<'static, str>, Cow<'static, str>)>, Cow<'a, str>);

/// Derive `(status, headers, body)` from a response value (see [`respond`]). `headers`
/// always carries a `Content-Type` (overridable) plus any the program supplied; the
/// caller adds `Content-Length`/`Connection`.
fn build_response<'a>(value: &'a Value, line: usize, col: usize) -> Result<Response<'a>, HelixError> {
    let json_of = |v: &Value| -> Result<String, HelixError> {
        match crate::writers::to_json(std::slice::from_ref(v), line, col)? {
            Value::Str(s) => Ok((*s).clone()),
            other => Ok(other.to_string()),
        }
    };
    // `text`/`html` stringify any value via Display (a `Dna`, number, etc. — not only a
    // `String`), so `{ text: seq.reverse_complement() }` sends the sequence text.
    // A string body is BORROWED straight out of the value (zero-copy — `respond`
    // reads it through the `Cow`); only a non-string pays a Display allocation.
    let as_text = |v: &'a Value| -> Cow<'a, str> {
        match v {
            Value::Str(s) => Cow::Borrowed(s.as_str()),
            other => Cow::Owned(other.to_string()),
        }
    };
    let one = |ct: &'static str, body: Cow<'a, str>| {
        (vec![(Cow::Borrowed("Content-Type"), Cow::Borrowed(ct))], body)
    };

    let (status, (headers, body)) = match value {
        Value::Record(fields) => {
            // Probes compare interned `Symbol`s (a `u32` each): `as_str` per probe
            // took the global interner's read lock several times every response.
            let (k_status, k_json, k_html, k_text, k_body, k_headers) = respond_keys();
            let get = |sym: Symbol| fields.iter().find(|(k, _)| *k == sym).map(|(_, v)| v);
            let status = match get(k_status) {
                Some(Value::Int(n)) if (100..=599).contains(n) => *n,
                // A present-but-invalid status must never reach the wire:
                // `status: 9999` wrote a protocol-invalid line Helix's own
                // client could not parse, and `status: "active"` silently
                // became an EMPTY 200, discarding the payload (sweep finds).
                Some(other) => {
                    return Err(HelixError::new(
                        format!(
                            "`status` must be an integer between 100 and 599, got {}",
                            match other {
                                Value::Int(n) => n.to_string(),
                                v => crate::value::with_article(v.type_name()),
                            }
                        ),
                        line,
                        col,
                    )
                    .hint("RFC 9110: a status code is exactly three digits."));
                }
                None => 200,
            };
            let payload = if let Some(v) = get(k_json) {
                one("application/json", Cow::Owned(json_of(v)?))
            } else if let Some(v) = get(k_html) {
                one("text/html; charset=utf-8", as_text(v))
            } else if let Some(v) = get(k_text).or_else(|| get(k_body)) {
                one("text/plain; charset=utf-8", as_text(v))
            } else if get(k_status).is_some() || get(k_headers).is_some() {
                // An explicit response envelope with no body (e.g. a redirect:
                // `{ status: 302, headers: { Location: "/" } }`) → empty body.
                one("text/plain; charset=utf-8", Cow::Borrowed(""))
            } else {
                // A plain data record (no envelope fields) → JSON of the whole record.
                one("application/json", Cow::Owned(json_of(value)?))
            };
            // Merge any program-supplied response headers (record or dict of name→value).
            let mut payload = payload;
            if let Some(h) = get(k_headers) {
                merge_headers(&mut payload.0, h)
                    .map_err(|m| HelixError::new(m, line, col))?;
            }
            (status, payload)
        }
        Value::Str(s) => (200, one("text/plain; charset=utf-8", Cow::Borrowed(s.as_str()))),
        other => (200, one("application/json", Cow::Owned(json_of(other)?))),
    };
    Ok((status, headers, body))
}

/// Merge a program-supplied `headers` value (a record `{ Location: "/" }`, or a dict
/// `{ "Set-Cookie" => "…" }` for names that aren't identifiers) into the response
/// header list. A custom `Content-Type` replaces the auto one; `Content-Length` and
/// `Connection` are reserved (the server computes them) and silently ignored.
fn merge_headers(
    out: &mut Vec<(Cow<'static, str>, Cow<'static, str>)>,
    headers: &Value,
) -> Result<(), String> {
    let text = |v: &Value| match v {
        Value::Str(s) => (**s).clone(),
        other => other.to_string(),
    };
    let mut err: Option<String> = None;
    let mut add = |name: String, val: String| {
        // A response header carrying a newline injects into the message just as a
        // request header does — a server that echoes a query parameter is the classic
        // case. Recorded rather than returned because this closure is a `FnMut` used
        // by the walks below; the first failure is reported after them.
        if let Err(m) = crate::value::validate_header(&name, &val) {
            if err.is_none() {
                err = Some(m);
            }
            return;
        }
        let lname = name.to_ascii_lowercase();
        if lname == "content-length" || lname == "connection" {
            return; // server-controlled — never overridden
        }
        if lname == "content-type"
            && let Some(ct) = out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            ct.1 = Cow::Owned(val);
            return;
        }
        out.push((Cow::Owned(name), Cow::Owned(val)));
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
        // Round-tripping: a Headers value (a forwarded request's, or a previous
        // response's) is already pairs in order.
        Value::Headers(pairs) => {
            for (k, v) in pairs.iter() {
                add(k.clone(), v.clone());
            }
        }
        // An array of `(name, value)` pairs — the one shape that can say a REPEATED
        // header, which a record (one field per name) and a dict (one key per name)
        // cannot. Two `Set-Cookie`s in one response is the canonical need. Accepts a
        // tuple or a 2-element array per pair, the same spellings the client's
        // request `headers` field takes, so the two directions agree.
        Value::Array(items) => {
            for it in items.to_values().iter() {
                let two: Vec<Value> = match it {
                    Value::Array(a) => a.to_values().to_vec(),
                    Value::Tuple(t) => t.iter().cloned().collect(),
                    _ => continue,
                };
                if let [Value::Str(k), v] = two.as_slice() {
                    add((**k).clone(), text(v));
                }
            }
        }
        _ => {} // a non-record/dict `headers` field is ignored
    }
    // `add` borrows `err` mutably; its last use is the match above, so the borrow
    // ends there and the recorded failure can be read.
    match err {
        Some(m) => Err(m),
        None => Ok(()),
    }
}

/// `conn.sse()` — begin a Server-Sent-Events response: status `200`, `text/event-stream`,
/// no `Content-Length`, the socket kept open. Drive it with `conn.send(value)` per event.
pub fn sse(handle: &Rc<NetHandle>, line: usize, col: usize) -> Result<Value, HelixError> {
    let (cell, pending, mode) = match &**handle {
        NetHandle::Conn { stream, pending, stream_mode, .. } => (stream, pending, stream_mode),
        _ => {
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
    if push_and_flush(stream, &mut pending.borrow_mut(), head.as_bytes()) {
        mode.set(Streaming::Sse);
    } else {
        *guard = None; // client already gone
    }
    Ok(Value::Unit)
}

/// `conn.stream(response)` — begin a response whose body is written INCREMENTALLY.
///
/// **The gap this closes.** `Net` streamed exactly one way: `sse()` opens an
/// `text/event-stream`, and `respond` sends a complete reply. So a document that is slow
/// to produce could not be flushed as it was produced — the first paint had to wait for
/// the last byte. A field report building a server-rendered UI reached this trying to
/// match streaming SSR, and correctly identified that the socket already stays open and
/// accepts incremental writes: `sse` proves it. This is that mechanism with the framing
/// generalized and the content type left to the caller.
///
/// Takes the SAME response value `respond` does — `{status, html, text, json, headers}` —
/// so "what a response is" has one spelling. Any body in it is sent as the FIRST chunk,
/// which makes `stream({status: 200, html: shell})` the natural way to flush a page shell
/// before the slow part exists.
///
/// **The framing depends on the client's HTTP version, and that is now decidable.**
/// HTTP/1.1 gets `Transfer-Encoding: chunked`. HTTP/1.0 has no chunked encoding, so it
/// gets `Connection: close` and the raw bytes, framed by the close itself — the one
/// correct answer for 1.0, and unavailable until the request record started carrying
/// `version`.
///
/// `Content-Length` is deliberately absent: a length is what streaming does not know.
pub fn stream_begin(handle: &Rc<NetHandle>, value: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let (cell, pending, mode, request) = match &**handle {
        NetHandle::Conn { stream, pending, stream_mode, request, .. } => {
            (stream, pending, stream_mode, request)
        }
        _ => {
            return Err(HelixError::new(
                "`stream` works on a connection from `accept()`, not a listener",
                line,
                col,
            ))
        }
    };
    if mode.get() != Streaming::No {
        return Err(HelixError::new("this connection is already streaming", line, col)
            .hint("`stream` (or `sse`) begins a response once; add to it with `send`, finish with `close`."));
    }
    let (status, headers, body) = build_response(value, line, col)?;

    // HTTP/1.0 has no chunked transfer encoding. Reading the version rather than assuming
    // 1.1 is what keeps a 1.0 client from being sent hex chunk lengths as document text.
    let one_zero = matches!(request, Value::Record(f)
        if f.iter().any(|(k, v)| k.as_str() == "version"
            && matches!(v, Value::Str(s) if s.as_str() == "1.0")));

    let mut guard = cell.borrow_mut();
    let st = match guard.as_mut() {
        Some(s) => s,
        None => return Err(HelixError::new("this connection is already closed", line, col)),
    };
    st.set_nonblocking(true).ok();

    use std::fmt::Write as _;
    let mut head = String::with_capacity(128 + body.len());
    let _ = write!(head, "HTTP/1.1 {status} {reason}\r\n", reason = reason_phrase(status));
    for (k, v) in &headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str(if one_zero {
        "Connection: close\r\n\r\n"
    } else {
        "Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n"
    });

    let mut buf = pending.borrow_mut();
    let mut alive = push_and_flush(st, &mut buf, head.as_bytes());
    if alive && !body.is_empty() {
        alive = push_chunk(st, &mut buf, &body, !one_zero);
    }
    drop(buf);
    if alive {
        mode.set(if one_zero { Streaming::Raw } else { Streaming::Chunked });
    } else {
        *guard = None;
    }
    Ok(Value::Bool(alive))
}

/// One body chunk, framed for the transfer encoding in use.
fn push_chunk(stream: &mut TcpStream, pending: &mut Vec<u8>, body: &str, chunked: bool) -> bool {
    if !chunked {
        return push_and_flush(stream, pending, body.as_bytes());
    }
    // `<hex len>\r\n<bytes>\r\n`, built in one buffer so a chunk is never split across
    // two pushes — a half-written chunk header is a protocol error, and `push_and_flush`
    // may legitimately buffer a partial write for a slow client.
    let mut framed = Vec::with_capacity(body.len() + 16);
    framed.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
    framed.extend_from_slice(body.as_bytes());
    framed.extend_from_slice(b"\r\n");
    push_and_flush(stream, pending, &framed)
}

/// `conn.send(value)` — write one SSE event (`data: …\n\n`) on a streaming connection,
/// **without blocking**. Returns a **Bool**: `true` the connection is alive (the event
/// was sent or buffered), `false` the client is gone or too far behind to keep up (so the
/// producer loop drops it). A string/`Dna` is sent verbatim; any other value as JSON.
pub fn send(handle: &Rc<NetHandle>, value: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let (cell, pending, mode) = match &**handle {
        NetHandle::Conn { stream, pending, stream_mode, .. } => (stream, pending, stream_mode),
        _ => {
            return Err(HelixError::new("`send` works on a connection from `accept()`", line, col));
        }
    };
    // Borrow the string/Dna case; only the JSON case needs an owned buffer. This avoids
    // cloning the whole payload on every event — the framing loop below only reads it.
    let owned;
    let payload: &str = match value {
        Value::Str(s) => s.as_str(),
        Value::Dna(s) => s.as_str(),
        other => match crate::writers::to_json(std::slice::from_ref(other), line, col)? {
            Value::Str(s) => {
                owned = (*s).clone();
                owned.as_str()
            }
            v => {
                owned = v.to_string();
                owned.as_str()
            }
        },
    };
    // A CHUNKED OR RAW RESPONSE takes the payload as a body chunk, not an SSE event —
    // `send` means "the next piece of whatever you started", and what a piece looks like
    // on the wire is the mode's business rather than the caller's. Returning early keeps
    // the SSE framing below exactly as it was for every existing program.
    //
    // An empty piece is skipped: in chunked encoding a zero-length chunk is the
    // TERMINATOR, so writing one would end the document mid-stream, and `close` owns that.
    let m = mode.get();
    if matches!(m, Streaming::Chunked | Streaming::Raw) {
        if payload.is_empty() {
            return Ok(Value::Bool(true));
        }
        let mut guard = cell.borrow_mut();
        let alive = match guard.as_mut() {
            Some(st) => {
                push_chunk(st, &mut pending.borrow_mut(), payload, m == Streaming::Chunked)
            }
            None => false,
        };
        if !alive {
            *guard = None;
        }
        return Ok(Value::Bool(alive));
    }

    // SSE framing: each line of the payload is its own `data:` field; a blank line ends
    // the event (so a multi-line body is delivered as one event, per the spec). Pre-size
    // the frame: "data: " + line + '\n' per line (7 bytes of framing), plus a trailing '\n'.
    let mut frame = String::with_capacity(payload.len() + (payload.matches('\n').count() + 1) * 7 + 1);
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

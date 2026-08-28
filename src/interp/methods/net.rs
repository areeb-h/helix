//! Net-handle methods (servers, streams, jars) — moved verbatim from the one-file methods module (2026-08-24).

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use std::rc::Rc;

/// Methods on a [`Value::Net`] handle — the HTTP server surface (`src/serve.rs`).
/// `accept` (on a listener) blocks for one request and returns `(request, connection)`;
/// `respond` (on a connection) writes the reply. Both are effects, outside the oracle.
pub(crate) fn net_method(
    h: &Rc<crate::serve::NetHandle>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // The cookie-jar handle answers a small set of its own methods; everything else is
    // a server/stream method below.
    if let crate::serve::NetHandle::CookieJar(jar) = &**h {
        let now = crate::cookiejar::now_unix();
        return match name {
            "cookies" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`cookies` takes no arguments", line, col));
                }
                // Each cookie as a record `{name, value, domain, path}`, in jar order.
                let rows = jar.snapshot(now).into_iter().map(|(n, v, d, p)| {
                    Value::Record(Rc::new(vec![
                        (crate::symbol::Symbol::intern("name"), Value::Str(Rc::new(n))),
                        (crate::symbol::Symbol::intern("value"), Value::Str(Rc::new(v))),
                        (crate::symbol::Symbol::intern("domain"), Value::Str(Rc::new(d))),
                        (crate::symbol::Symbol::intern("path"), Value::Str(Rc::new(p))),
                    ]))
                });
                Ok(Value::array(rows.collect()))
            }
            "count" | "length" => {
                if !args.is_empty() {
                    return Err(HelixError::new(format!("`{name}` takes no arguments"), line, col));
                }
                Ok(Value::Int(jar.snapshot(now).len() as i64))
            }
            "clear" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`clear` takes no arguments", line, col));
                }
                jar.clear();
                Ok(Value::Unit)
            }
            other => Err(HelixError::new(
                format!("a cookie jar has no method `{other}`"),
                line,
                col,
            )
            .hint("jar methods: cookies, count, clear.")),
        };
    }

    // Capability gate (ADR 0021): the socket-touching verbs (accept/poll/respond/sse/send)
    // are `Net` authority — defense-in-depth behind `listen` (itself gated). `request` reads
    // the already-parsed request record (no socket I/O) and is ungated (`Pure`).
    crate::capability::gate_method(name, args, line, col)?;
    match name {
        "accept" => {
            if !args.is_empty() {
                return Err(HelixError::new("`accept` takes no arguments", line, col));
            }
            crate::serve::accept(h, line, col)
        }
        "poll" => {
            if !args.is_empty() {
                return Err(HelixError::new("`poll` takes no arguments", line, col));
            }
            crate::serve::poll(h, line, col)
        }
        // Cooperative event-loop server: non-blocking accept + per-connection non-blocking
        // read, so one thread serves many keep-alive connections interleaved.
        "accept_poll" => {
            if !args.is_empty() {
                return Err(HelixError::new("`accept_poll` takes no arguments", line, col));
            }
            crate::serve::accept_poll(h, line, col)
        }
        "poll_request" => {
            if !args.is_empty() {
                return Err(HelixError::new("`poll_request` takes no arguments", line, col));
            }
            crate::serve::poll_request(h, line, col)
        }
        "is_open" => {
            if !args.is_empty() {
                return Err(HelixError::new("`is_open` takes no arguments", line, col));
            }
            crate::serve::is_open(h, line, col)
        }
        "wait" => {
            if args.len() != 2 {
                return Err(HelixError::new(
                    "`wait` takes (conns, timeout_ms)",
                    line,
                    col,
                )
                .hint("e.g. `l.wait(conns, 50)` — block until a connection is ready."));
            }
            let timeout = match &args[1] {
                Value::Int(n) => *n,
                other => return Err(type_err("wait", "a timeout in ms (integer)", other, line, col)),
            };
            crate::serve::wait(h, &args[0], timeout, line, col)
        }
        "request" => {
            if !args.is_empty() {
                return Err(HelixError::new("`request` takes no arguments", line, col));
            }
            crate::serve::request(h, line, col)
        }
        "respond" => {
            if args.len() != 1 {
                return Err(HelixError::new("`respond` takes one response value", line, col)
                    .hint("e.g. `conn.respond({ status: 200, json: data })`."));
            }
            crate::serve::respond(h, &args[0], line, col)
        }
        "stream" => {
            if args.len() != 1 {
                return Err(HelixError::new(
                    format!("`stream` takes 1 argument (the response), got {}", args.len()),
                    line,
                    col,
                ));
            }
            crate::serve::stream_begin(h, &args[0], line, col)
        }
        "sse" => {
            if !args.is_empty() {
                return Err(HelixError::new("`sse` takes no arguments", line, col));
            }
            crate::serve::sse(h, line, col)
        }
        "send" => {
            if args.len() != 1 {
                return Err(HelixError::new("`send` takes one event value", line, col)
                    .hint("e.g. `conn.send(world.to_json())` — returns false when the client leaves."));
            }
            crate::serve::send(h, &args[0], line, col)
        }
        // Streaming client (`http_stream`): pull chunks and read the status.
        "next" => {
            if !args.is_empty() {
                return Err(HelixError::new("`next` takes no arguments", line, col));
            }
            crate::serve::stream_next(h, line, col)
        }
        "status" => {
            if !args.is_empty() {
                return Err(HelixError::new("`status` takes no arguments", line, col));
            }
            crate::serve::stream_status(h, line, col)
        }
        "close" => {
            if !args.is_empty() {
                return Err(HelixError::new("`close` takes no arguments", line, col));
            }
            crate::serve::stream_close(h, line, col)
        }
        other => Err(HelixError::new(format!("type Net has no method `{other}`"), line, col)
            .hint("a listener has `accept`/`poll`; a connection has `request`/`respond`, or `sse`/`send` to stream; an http_stream has `status`/`next`/`close`.")),
    }
}


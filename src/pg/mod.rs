//! PostgreSQL access — the ADR 0038 decisions, over a network connection.
//!
//! `postgres_query(url, sql, params)` returns a **DataFrame**, parameters are **values**,
//! the session is **read-only**, and the body is **feature-gated**. Those are D1–D4 of
//! ADR 0038, unchanged; what differs from SQLite is that the effect is `net` rather than
//! `fs-read`, and that read-only has to be enforced by the SERVER because there is no
//! connection flag to open a socket read-only with.
//!
//! WHY HAND-ROLLED. The protocol is v3, frozen since 2003 — PostgreSQL 18 added 3.2 and 19
//! carries it, but backward-compatibly, and `libpq` still requests 3.0 by default. Against
//! that, every alternative costs a dependency stack: `libpq` is a C library that would end
//! the binary's "no system dependency" property (the same property that made SQLite a
//! bundled build), and the pure-Rust drivers bring an async runtime for a synchronous
//! language. What this needs instead — SHA-256, HMAC, base64, `OsRng`, rustls — is already
//! in the tree for other reasons, so the client adds no crates at all.
//!
//! TOTALITY (ADR 0024). Every byte here comes off a socket. `proto` bounds-checks the
//! framing; this module bounds the WAIT, with connect and read timeouts, because a server
//! that accepts a connection and then says nothing would otherwise hang a Helix program
//! with no way to interrupt it.

/// The connection URL and its security policy — NOT gated, so the gate tests it. See the
/// module note; the `allow` is narrowed to exactly the build where "nothing calls this" is
/// the intended truth, rather than a blanket that could also hide a real dead branch.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
mod conninfo;
#[cfg(feature = "postgres")]
mod proto;
#[cfg(feature = "postgres")]
mod scram;
#[cfg(feature = "postgres")]
mod tls;
#[cfg(feature = "postgres")]
mod types;

#[cfg(feature = "postgres")]
use std::net::TcpStream;
#[cfg(feature = "postgres")]
use std::time::Duration;

use crate::backend::Df;
use crate::error::HelixError;
use crate::value::Value;

#[cfg(feature = "postgres")]
use proto::{error_text, put_cstr, read_msg, write_msg, Msg};
#[cfg(feature = "postgres")]
use conninfo::{parse_url, SslMode, Target};
#[cfg(feature = "postgres")]
use tls::Stream;
#[cfg(feature = "postgres")]
use types::ColBuf;

/// An open connection, alive for as long as a Helix value holds it.
///
/// Opaque and effect-only, like `Net` and `Lock`: never compared, serialised, or computed
/// with, so it falls through every structural path with no extra arms.
///
/// WHY THIS EXISTS. Every `postgres_query` opens a TCP connection and completes a
/// SCRAM-SHA-256 exchange. Measured against PostgreSQL 19: 4.7 ms per call, and `select 1`
/// costs the same as reading the whole table — the handshake IS the query time. Removing
/// the read-only transaction's two round trips changed it by 0.01 ms, which is the proof:
/// the round trips were never the cost. A page issuing five queries spent ~24 ms before
/// doing any work, against 0.017 ms for a point lookup in this project's own storage
/// engine — the handshake was 280x an entire local query.
///
/// THERE IS NO `close` TO FORGET. Helix values are reference-counted, not collected, so
/// the socket shuts when the last handle to it goes — and "when it goes out of scope" is a
/// real guarantee here rather than an eventual one. That is the same lifetime rule `Lock`
/// already relies on, and it removes the failure every connection pool eventually grows a
/// leak detector for: a handle nobody remembered to give back.
pub struct Conn {
    #[cfg(feature = "postgres")]
    stream: std::cell::RefCell<Option<Stream>>,
    /// `user@host:port/database`, for diagnostics. Never the password.
    #[cfg(feature = "postgres")]
    label: String,
}

#[cfg(feature = "postgres")]
impl Conn {
    /// Run one statement on this connection.
    ///
    /// `try_borrow_mut` rather than `borrow_mut`: nothing here calls back into Helix while
    /// the borrow is held, so a conflict should be impossible — but "should be impossible"
    /// is what a host abort is made of, and ADR 0024 says user input must never abort the
    /// process. A clean error costs one line.
    fn run(&self, sql: &str, params: &[Option<String>]) -> Result<Vec<ColBuf>, String> {
        let mut guard = self
            .stream
            .try_borrow_mut()
            .map_err(|_| "this connection is already in use".to_string())?;
        let s = guard.as_mut().ok_or("this connection is closed")?;
        run_query(s, sql, params)
    }
}

/// `postgres_open(url)` — one connection, reused for every query made through it.
#[cfg(feature = "postgres")]
pub fn postgres_open(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let err = |m: String| HelixError::new(m, line, col);
    let Some(Value::Str(url)) = args.first() else {
        return Err(err("`postgres_open` takes a connection URL".to_string())
            .hint("e.g. `c = postgres_open(\"postgres://user:pw@host/db\")`."));
    };
    let target = parse_url(url.as_str(), line, col)?;
    let label = format!("{}@{}:{}/{}", target.user, target.host, target.port, target.database);
    let stream = connect(&target).map_err(|m| err(format!("postgres {label}: {m}")))?;
    // AN UNPROTECTED CONNECTION SAYS SO, in every error it ever produces. Only the unusual
    // case is marked: a verified TLS session is what asking for nothing gets you, so
    // annotating it would be noise, while `(plaintext)` appearing in a message is the
    // cheapest possible way for someone to notice an `sslmode=disable` that outlived the
    // afternoon it was added for.
    let label = if stream.is_tls() { label } else { format!("{label} (plaintext)") };
    Ok(Value::Db(std::rc::Rc::new(Conn {
        stream: std::cell::RefCell::new(Some(stream)),
        label,
    })))
}

/// The same verb without the feature.
#[cfg(not(feature = "postgres"))]
pub fn postgres_open(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let _ = args;
    Err(HelixError::new("this build has no PostgreSQL support", line, col)
        .hint("rebuild with `--features postgres`."))
}

#[cfg(feature = "postgres")]
impl Drop for Conn {
    /// Say goodbye and drop the socket, when the last handle to it goes.
    ///
    /// Failure is ignored on purpose: the caller already has its answers, and a connection
    /// that cannot be closed politely is still closed when the descriptor goes. `Drop`
    /// must not raise, and there is nothing a program could do about it if it did.
    fn drop(&mut self) {
        if let Ok(mut guard) = self.stream.try_borrow_mut()
            && let Some(mut s) = guard.take()
        {
            let _ = write_msg(&mut s, Some(b'X'), &[]);
            // And at the TLS layer, so the server sees a clean shutdown rather than a
            // truncated one it has to treat as a possible attack.
            s.close_notify();
        }
    }
}

/// `conn.query(sql, params?)` — the method on an open connection.
#[cfg(feature = "postgres")]
pub fn conn_method(
    c: &std::rc::Rc<Conn>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let err = |m: String| HelixError::new(m, line, col);
    match name {
        "query" => {
            let Some(Value::Str(sql)) = args.first() else {
                return Err(err("`query` takes a SQL string".to_string())
                    .hint("e.g. `c.query(\"select * from users where id = $1\", [7])`."));
            };
            let params: Vec<Value> = match args.get(1) {
                None | Some(Value::Missing) => Vec::new(),
                Some(Value::Array(a)) => a.iter_values().collect(),
                Some(other) => {
                    return Err(err(format!(
                        "`query` parameters must be an array, got {}",
                        crate::value::with_article(other.type_name())
                    )))
                }
            };
            let mut texts = Vec::with_capacity(params.len());
            for (i, p) in params.iter().enumerate() {
                texts.push(param_text(p, i + 1, line, col)?);
            }
            let cols =
                c.run(sql.as_str(), &texts).map_err(|m| err(format!("postgres {}: {m}", c.label)))?;
            let mut built = Vec::with_capacity(cols.len());
            for cb in cols {
                let n = cb.name.clone();
                built.push((
                    n,
                    cb.finish().map_err(|m| err(format!("postgres {}: {m}", c.label)))?,
                ));
            }
            crate::backend::build_frame(built, line, col)
                .map(|df| Value::DataFrame(std::rc::Rc::new(df)))
        }
        other => Err(err(format!("a Connection has no method `{other}`"))
            .hint("a Connection answers `query(sql, params?)`.")),
    }
}

/// The same method surface without the feature — unreachable, because a `Connection` can
/// only come from `postgres_with`, which refuses first. It exists so the dispatch arm
/// compiles in every build.
#[cfg(not(feature = "postgres"))]
pub fn conn_method(
    c: &std::rc::Rc<Conn>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let _ = (c, name, args);
    Err(HelixError::new("this build has no PostgreSQL support", line, col)
        .hint("rebuild with `--features postgres`."))
}

/// `postgres_query(url, sql, params?)` — the builtin's entry point.
///
/// The shape mirrors `sqlite_query` deliberately: a connection, a statement, and
/// parameters as VALUES. Someone who knows one knows the other, and the difference that
/// matters — a URL instead of a path, `$1` instead of `?` — is the difference the two
/// databases actually have.
#[cfg(feature = "postgres")]
pub fn postgres_query(args: &[Value], line: usize, col: usize) -> Result<Df, HelixError> {
    let err = |m: String| HelixError::new(m, line, col);
    let (Some(Value::Str(url)), Some(Value::Str(sql))) = (args.first(), args.get(1)) else {
        return Err(err("`postgres_query` takes a connection URL and a SQL string".to_string())
            .hint("e.g. `postgres_query(\"postgres://user:pw@host/db\", \"select * from users where id = $1\", [7])`."));
    };

    // Parameters bind as VALUES. There is deliberately no way to splice text into the
    // statement, which is what makes injection unrepresentable rather than discouraged
    // (ADR 0038 D2). PostgreSQL numbers its placeholders `$1`, `$2`, where SQLite uses `?`.
    let params: Vec<Value> = match args.get(2) {
        None | Some(Value::Missing) => Vec::new(),
        Some(Value::Array(a)) => a.iter_values().collect(),
        Some(other) => {
            return Err(err(format!(
                "`postgres_query` parameters must be an array, got {}",
                crate::value::with_article(other.type_name())
            )))
        }
    };

    query(url.as_str(), sql.as_str(), &params, line, col)
}

/// The same verb in a build without the feature: it exists, it type-checks, it appears in
/// `helix describe` with its signature and effect, and running it says what to do.
#[cfg(not(feature = "postgres"))]
pub fn postgres_query(args: &[Value], line: usize, col: usize) -> Result<Df, HelixError> {
    let _ = args;
    Err(HelixError::new("this build has no PostgreSQL support", line, col)
        .hint("rebuild with `--features postgres`."))
}

#[cfg(feature = "postgres")]
/// Protocol 3.0, as `libpq` still requests by default.
const PROTOCOL_3_0: i32 = 196_608;

#[cfg(feature = "postgres")]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(feature = "postgres")]
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A parameter, rendered as the text the server will parse.
///
/// Types are left UNSPECIFIED (OID 0) so the server infers each from its use in the
/// statement, which is what `libpq` does for untyped parameters and what makes
/// `where age > $1` work without the caller declaring `int4`.
#[cfg(feature = "postgres")]
fn param_text(v: &Value, n: usize, line: usize, col: usize) -> Result<Option<String>, HelixError> {
    Ok(match v {
        Value::Missing => None,
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(crate::value::fmt_float(*f)),
        Value::Bool(b) => Some(if *b { "t".into() } else { "f".into() }),
        Value::Str(s) => Some((**s).clone()),
        other => {
            return Err(HelixError::new(
                format!(
                    "parameter {n} is {}, which has no SQL form",
                    crate::value::with_article(other.type_name())
                ),
                line,
                col,
            )
            .hint("parameters may be Int, Float, Bool, String, or missing (SQL NULL)."))
        }
    })
}

/// Read messages until one of `stop` arrives, failing on `ErrorResponse`.
///
/// `NoticeResponse` and `ParameterStatus` can arrive at ANY time by the protocol's own
/// rules, so every wait has to tolerate them rather than treating them as the reply.
#[cfg(feature = "postgres")]
fn wait_for(s: &mut Stream, stop: &[u8]) -> Result<Msg, String> {
    loop {
        let m = read_msg(s)?;
        match m.tag {
            b'E' => return Err(error_text(&m)),
            _ if stop.contains(&m.tag) => return Ok(m),
            // Notices, parameter status, backend key, and the messages a query produces
            // that this caller is not waiting on.
            _ => continue,
        }
    }
}

#[cfg(feature = "postgres")]
/// Connect, authenticate, and leave the session ready for a query.
fn connect(t: &Target) -> Result<Stream, String> {
    let addr = format!("{}:{}", t.host, t.port);
    let addrs: Vec<_> = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .map_err(|e| format!("cannot resolve `{addr}`: {e}"))?
        .collect();
    let first = addrs.first().ok_or_else(|| format!("`{addr}` resolved to no address"))?;
    let s = TcpStream::connect_timeout(first, CONNECT_TIMEOUT)
        .map_err(|e| format!("cannot connect to `{addr}`: {e}"))?;
    // A bounded wait, so a server that accepts and then stalls cannot hang the program.
    s.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| format!("setting a read timeout: {e}"))?;
    s.set_write_timeout(Some(READ_TIMEOUT)).map_err(|e| format!("setting a write timeout: {e}"))?;
    // Small messages, and latency is what matters on a query round trip.
    let _ = s.set_nodelay(true);

    // TLS FIRST, before the startup packet — which is the message carrying the user name,
    // and which is immediately followed by the password exchange. The negotiation is one
    // byte and it is not a preference: a server that answers "no" ends the connection
    // here rather than continuing in the clear.
    let mut s = match t.sslmode {
        SslMode::Disable => Stream::Plain(s),
        SslMode::VerifyFull => tls::negotiate(s, &t.host, t.sslrootcert.as_deref())?,
    };

    let mut body = Vec::new();
    body.extend_from_slice(&PROTOCOL_3_0.to_be_bytes());
    put_cstr(&mut body, "user");
    put_cstr(&mut body, &t.user);
    put_cstr(&mut body, "database");
    put_cstr(&mut body, &t.database);
    put_cstr(&mut body, "application_name");
    put_cstr(&mut body, "helix");
    put_cstr(&mut body, "client_encoding");
    put_cstr(&mut body, "UTF8");
    // READ-ONLY FROM THE FIRST BYTE. Sending this as a startup parameter rather than as
    // a `begin transaction read only` means the session is read-only before a single
    // statement can be sent — there is no window, not even a short one — and it costs
    // ZERO round trips where the explicit transaction cost two (begin and commit).
    put_cstr(&mut body, "default_transaction_read_only");
    put_cstr(&mut body, "on");
    body.push(0);
    write_msg(&mut s, None, &body)?;

    authenticate(&mut s, t)?;
    wait_for(&mut s, b"Z")?;
    Ok(s)
}

#[cfg(feature = "postgres")]
fn authenticate(s: &mut Stream, t: &Target) -> Result<(), String> {
    let mut sasl: Option<scram::Scram> = None;
    loop {
        let m = read_msg(s)?;
        match m.tag {
            b'E' => return Err(error_text(&m)),
            b'R' => {
                let mut c = m.cur();
                match c.i32()? {
                    // AuthenticationOk
                    0 => return Ok(()),
                    // SASL: a list of mechanisms. Only SCRAM-SHA-256 is offered back.
                    10 => {
                        let mut names = Vec::new();
                        loop {
                            let n = c.cstr()?;
                            if n.is_empty() {
                                break;
                            }
                            names.push(n);
                        }
                        if !names.iter().any(|n| n == "SCRAM-SHA-256") {
                            return Err(format!(
                                "the server offers only {} for authentication; this client speaks SCRAM-SHA-256",
                                names.join(", ")
                            ));
                        }
                        let mut sc = scram::Scram::new(&t.password);
                        let first = sc.client_first();
                        let mut body = Vec::new();
                        put_cstr(&mut body, "SCRAM-SHA-256");
                        body.extend_from_slice(&(first.len() as i32).to_be_bytes());
                        body.extend_from_slice(first.as_bytes());
                        write_msg(s, Some(b'p'), &body)?;
                        sasl = Some(sc);
                    }
                    // SASLContinue
                    11 => {
                        let sc = sasl.as_mut().ok_or("the server continued a SASL exchange that never started")?;
                        let server_first = std::str::from_utf8(c.rest())
                            .map_err(|_| "the server's SCRAM challenge is not UTF-8".to_string())?
                            .to_string();
                        let final_msg = sc.client_final(&server_first)?;
                        write_msg(s, Some(b'p'), final_msg.as_bytes())?;
                    }
                    // SASLFinal — verified, not assumed.
                    12 => {
                        let sc = sasl.as_ref().ok_or("the server finished a SASL exchange that never started")?;
                        let server_final = std::str::from_utf8(c.rest())
                            .map_err(|_| "the server's SCRAM signature is not UTF-8".to_string())?;
                        sc.verify_server(server_final)?;
                    }
                    // Cleartext and MD5 are refused BY NAME rather than supported. MD5 is
                    // deprecated upstream, and a client that silently downgrades when asked
                    // is the whole problem with offering it.
                    3 => return Err("the server asked for a cleartext password; this client requires SCRAM-SHA-256".into()),
                    5 => return Err("the server asked for MD5 authentication, which is deprecated; set `password_encryption = scram-sha-256`".into()),
                    other => return Err(format!("the server asked for authentication method {other}, which this client does not implement")),
                }
            }
            _ => continue,
        }
    }
}

#[cfg(feature = "postgres")]
/// Run one parameterised statement and collect its rows.
fn run_query(
    s: &mut Stream,
    sql: &str,
    params: &[Option<String>],
) -> Result<Vec<ColBuf>, String> {
    let mut out = Vec::new();

    // Parse: unnamed statement, no declared parameter types (the server infers).
    put_cstr(&mut out, "");
    put_cstr(&mut out, sql);
    out.extend_from_slice(&0i16.to_be_bytes());
    write_msg(s, Some(b'P'), &out)?;

    // Bind: text in, text out.
    out.clear();
    put_cstr(&mut out, ""); // portal
    put_cstr(&mut out, ""); // statement
    out.extend_from_slice(&0i16.to_be_bytes()); // parameter formats: all text
    let n = i16::try_from(params.len()).map_err(|_| "too many parameters".to_string())?;
    out.extend_from_slice(&n.to_be_bytes());
    for p in params {
        match p {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                let len = i32::try_from(v.len()).map_err(|_| "parameter too large".to_string())?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(v.as_bytes());
            }
        }
    }
    out.extend_from_slice(&0i16.to_be_bytes()); // result formats: all text
    write_msg(s, Some(b'B'), &out)?;

    // Describe the portal, so the column names and type OIDs arrive even for zero rows.
    out.clear();
    out.push(b'P');
    put_cstr(&mut out, "");
    write_msg(s, Some(b'D'), &out)?;

    out.clear();
    put_cstr(&mut out, "");
    out.extend_from_slice(&0i32.to_be_bytes()); // unlimited rows
    write_msg(s, Some(b'E'), &out)?;

    write_msg(s, Some(b'S'), &[])?;

    let mut cols: Vec<ColBuf> = Vec::new();
    let mut described = false;
    loop {
        let m = read_msg(s)?;
        match m.tag {
            b'E' => {
                // Drain to the synchronisation point so the connection is left in a
                // known state even though this query is finished.
                let _ = wait_for(s, b"Z");
                return Err(error_text(&m));
            }
            // RowDescription
            b'T' => {
                let mut c = m.cur();
                let n = c.i16()?;
                for _ in 0..n {
                    let name = c.cstr()?;
                    let _table_oid = c.i32()?;
                    let _attnum = c.i16()?;
                    let oid = c.i32()?;
                    let _typlen = c.i16()?;
                    let _typmod = c.i32()?;
                    let _format = c.i16()?;
                    cols.push(ColBuf::new(name, oid));
                }
                described = true;
            }
            // NoData: a statement with no result columns.
            b'n' => described = true,
            // DataRow
            b'D' => {
                let mut c = m.cur();
                let n = usize::try_from(c.i16()?).map_err(|_| "negative column count".to_string())?;
                if n != cols.len() {
                    return Err(format!(
                        "the server sent a row of {n} values for {} columns",
                        cols.len()
                    ));
                }
                for col in cols.iter_mut().take(n) {
                    let v = c.field()?;
                    col.push(v)?;
                }
            }
            // ReadyForQuery — the synchronisation point.
            b'Z' => break,
            _ => continue,
        }
    }
    if !described {
        return Err("the server never described the result".to_string());
    }
    Ok(cols)
}

#[cfg(feature = "postgres")]
/// The connection, the statement, and the frame — ADR 0038 D1/D2/D3 over the network.
fn query(
    url: &str,
    sql: &str,
    params: &[Value],
    line: usize,
    col: usize,
) -> Result<Df, HelixError> {
    let target = parse_url(url, line, col)?;
    let err = |m: String| {
        // The URL carries a password, so it is never echoed in an error. Host and
        // database are what a reader needs to identify the connection.
        HelixError::new(
            format!("postgres {}@{}:{}/{}: {m}", target.user, target.host, target.port, target.database),
            line,
            col,
        )
    };

    let mut texts = Vec::with_capacity(params.len());
    for (i, p) in params.iter().enumerate() {
        texts.push(param_text(p, i + 1, line, col)?);
    }

    // READ-ONLY IS ALREADY ESTABLISHED, in the startup packet, before this or any other
    // statement could be sent (ADR 0038 D3, ADR 0044 D3). It used to be a
    // `begin transaction read only` here and a `commit` after — correct, but three round
    // trips where one will do, and a window (however short) in which the session was not
    // yet read-only. A guarantee that holds from the first byte is both cheaper and
    // stronger than one a client remembers to ask for.
    let mut s = connect(&target).map_err(&err)?;

    let cols = run_query(&mut s, sql, &texts).map_err(&err)?;

    // Best-effort goodbye: the answer is already in hand, so failing to say it must not
    // turn a successful query into an error.
    let _ = write_msg(&mut s, Some(b'X'), &[]);

    let mut built = Vec::with_capacity(cols.len());
    for c in cols {
        let name = c.name.clone();
        built.push((name, c.finish().map_err(&err)?));
    }
    crate::backend::build_frame(built, line, col)
}

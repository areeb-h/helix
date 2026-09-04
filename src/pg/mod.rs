//! PostgreSQL access — the ADR 0038 decisions, over a network connection.
//!
//! `postgres_query(url, sql, params)` returns a **DataFrame**, parameters are **values**,
//! the session is **read-only**, and the body is **feature-gated**. Those are D1–D4 of
//! ADR 0038, unchanged; what differs from SQLite is that the effect is `net` rather than
//! `fs-read`, and that read-only has to be enforced by the SERVER because there is no
//! connection flag to open a socket read-only with.
//!
//! Writes came later (ADR 0047): `postgres_execute` and `postgres_open(url, "write")` open
//! the ONE kind of session that omits the read-only default, and spend `db-write` for it.
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
    /// Opened with `"write"`: the startup packet omitted the read-only default, and the
    /// `db-write` grant was checked at `postgres_open`. `execute` on a read-only connection
    /// refuses BEFORE sending anything, with the spelling that opens a writable one.
    #[cfg(feature = "postgres")]
    writable: bool,
}

#[cfg(feature = "postgres")]
impl Conn {
    /// Run one statement on this connection.
    ///
    /// `try_borrow_mut` rather than `borrow_mut`: nothing here calls back into Helix while
    /// the borrow is held, so a conflict should be impossible — but "should be impossible"
    /// is what a host abort is made of, and ADR 0024 says user input must never abort the
    /// process. A clean error costs one line.
    fn run(&self, sql: &str, params: &[Option<String>]) -> Result<Outcome, String> {
        let mut guard = self
            .stream
            .try_borrow_mut()
            .map_err(|_| "this connection is already in use".to_string())?;
        let s = guard.as_mut().ok_or("this connection is closed")?;
        run_statement(s, sql, params)
    }
}

/// `postgres_open(url)` — one connection, reused for every query made through it.
#[cfg(feature = "postgres")]
pub fn postgres_open(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let err = |m: String| HelixError::new(m, line, col);
    let Some(Value::Str(url)) = args.first() else {
        return Err(err("`postgres_open` takes a connection URL".to_string())
            .hint("e.g. `c = postgres_open(\"postgres://user:pw@host/db\")` — add `\"write\"` for a session that can execute writes."));
    };
    let writable = open_grant(args, line, col)?;
    let target = parse_url(url.as_str(), line, col)?;
    let label = format!("{}@{}:{}/{}", target.user, target.host, target.port, target.database);
    let stream = connect(&target, !writable).map_err(|m| err(format!("postgres {label}: {m}")))?;
    // AN UNPROTECTED CONNECTION SAYS SO, in every error it ever produces. Only the unusual
    // case is marked: a verified TLS session is what asking for nothing gets you, so
    // annotating it would be noise, while `(plaintext)` appearing in a message is the
    // cheapest possible way for someone to notice an `sslmode=disable` that outlived the
    // afternoon it was added for.
    let label = if stream.is_tls() { label } else { format!("{label} (plaintext)") };
    Ok(Value::Db(std::rc::Rc::new(Conn {
        stream: std::cell::RefCell::new(Some(stream)),
        label,
        writable,
    })))
}

/// The same verb without the feature.
#[cfg(not(feature = "postgres"))]
pub fn postgres_open(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let _ = open_grant(args, line, col)?;
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
        "query" | "execute" => {
            let Some(Value::Str(sql)) = args.first() else {
                return Err(err(format!("`{name}` takes a SQL string"))
                    .hint(format!("e.g. `c.{name}(\"select * from users where id = $1\", [7])`.")));
            };
            // A read-only connection refuses BEFORE a byte is sent, with the spelling that
            // opens a writable one. The server would refuse too (SQLSTATE 25006) — a round
            // trip later, and without saying what to do about it.
            if name == "execute" && !c.writable {
                return Err(err(format!(
                    "postgres {}: this connection is read-only, so it cannot execute a statement",
                    c.label
                ))
                .hint("open one that can write: `postgres_open(url, \"write\")` — it needs the `db-write` capability."));
            }
            let params = statement_params(name, args.get(1), line, col)?;
            let texts = param_texts(&params, line, col)?;
            let conn_err = |m: String| err(format!("postgres {}: {m}", c.label));
            let out = c.run(sql.as_str(), &texts).map_err(&conn_err)?;
            if name == "execute" {
                return outcome_value(out, line, col, &conn_err);
            }
            frame_of(out.cols, line, col, &conn_err)
                .map(|df| Value::DataFrame(std::rc::Rc::new(df)))
        }
        other => Err(err(format!(
            "{} has no method `{other}`",
            crate::value::with_article("Connection")
        ))
            .hint("a Connection answers `query(sql, params?)` — and `execute(sql, params?)` when opened with `\"write\"`.")),
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
    let params = statement_params("postgres_query", args.get(2), line, col)?;

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
fn connect(t: &Target, read_only: bool) -> Result<Stream, String> {
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
    // READ-ONLY FROM THE FIRST BYTE — unless this session was opened to write. Sending
    // this as a startup parameter rather than as a `begin transaction read only` means the
    // session is read-only before a single statement can be sent — there is no window,
    // not even a short one — and it costs ZERO round trips where the explicit transaction
    // cost two (begin and commit). A writable session (`postgres_execute`,
    // `postgres_open(url, "write")`) simply omits it — the server's own default is
    // read-write — and the `db-write` grant has been checked before this packet is built
    // (ADR 0047).
    if read_only {
        put_cstr(&mut body, "default_transaction_read_only");
        put_cstr(&mut body, "on");
    }
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

/// What a statement produced: its result columns (none for a statement without a
/// `RETURNING`) and the server's completion tag — `INSERT 0 3`, `UPDATE 7`, `CREATE TABLE`.
#[cfg(feature = "postgres")]
struct Outcome {
    cols: Vec<ColBuf>,
    tag: String,
}

#[cfg(feature = "postgres")]
/// Run one parameterised statement: its rows, and its completion tag.
fn run_statement(
    s: &mut Stream,
    sql: &str,
    params: &[Option<String>],
) -> Result<Outcome, String> {
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
    let mut tag = String::new();
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
            // CommandComplete: what the statement did, and to how many rows.
            b'C' => {
                let mut c = m.cur();
                tag = c.cstr()?;
            }
            // ReadyForQuery — the synchronisation point.
            b'Z' => break,
            _ => continue,
        }
    }
    if !described {
        return Err("the server never described the result".to_string());
    }
    Ok(Outcome { cols, tag })
}

/// The optional second argument of `postgres_open`: `"read"` (the default — read-only from
/// the first byte) or `"write"`. Opening a session that CAN write is the authority, not the
/// statement that later uses it, so the `db-write` grant is checked here — in every build,
/// before any network.
fn open_grant(args: &[Value], line: usize, col: usize) -> Result<bool, HelixError> {
    let writable = match args.get(1) {
        None | Some(Value::Missing) => false,
        Some(Value::Str(m)) if m.as_str() == "read" => false,
        Some(Value::Str(m)) if m.as_str() == "write" => true,
        Some(other) => {
            let got = match other {
                Value::Str(s) => format!("`\"{s}\"`"),
                v => crate::value::with_article(v.type_name()).to_string(),
            };
            return Err(HelixError::new(
                format!(
                    "`postgres_open` takes a URL and an optional mode, `\"read\"` (the default) or `\"write\"`, got {got}"
                ),
                line,
                col,
            )
            .hint("e.g. `postgres_open(url, \"write\")` for a session that can execute writes."));
        }
    };
    if writable {
        crate::capability::gate_effect(
            crate::capability::Effect::DbWrite,
            "postgres_open",
            args,
            line,
            col,
        )?;
    }
    Ok(writable)
}

/// `params?` of a statement verb: an array of values, or nothing.
#[cfg(feature = "postgres")]
fn statement_params(
    verb: &str,
    arg: Option<&Value>,
    line: usize,
    col: usize,
) -> Result<Vec<Value>, HelixError> {
    match arg {
        None | Some(Value::Missing) => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.iter_values().collect()),
        Some(other) => Err(HelixError::new(
            format!(
                "`{verb}` parameters must be an array, got {}",
                crate::value::with_article(other.type_name())
            ),
            line,
            col,
        )),
    }
}

#[cfg(feature = "postgres")]
fn param_texts(params: &[Value], line: usize, col: usize) -> Result<Vec<Option<String>>, HelixError> {
    let mut texts = Vec::with_capacity(params.len());
    for (i, p) in params.iter().enumerate() {
        texts.push(param_text(p, i + 1, line, col)?);
    }
    Ok(texts)
}

/// The rows a statement returned, as a frame — with no columns when it returned none.
#[cfg(feature = "postgres")]
fn frame_of(
    cols: Vec<ColBuf>,
    line: usize,
    col: usize,
    err: &dyn Fn(String) -> HelixError,
) -> Result<Df, HelixError> {
    let mut built = Vec::with_capacity(cols.len());
    for c in cols {
        let name = c.name.clone();
        built.push((name, c.finish().map_err(err)?));
    }
    crate::backend::build_frame(built, line, col)
}

/// Rows affected, read from the completion tag. The tag is the command word, an OID for a
/// one-row INSERT (always 0 since PostgreSQL 12), and the count — so the count is the LAST
/// word for every command that reports one, and a command that reports none (`CREATE
/// TABLE`, `BEGIN`) affected no rows.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
fn rows_affected(tag: &str) -> i64 {
    let mut words = tag.split_ascii_whitespace();
    let Some(cmd) = words.next() else { return 0 };
    if !matches!(cmd, "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "SELECT" | "MOVE" | "FETCH" | "COPY") {
        return 0;
    }
    words.last().and_then(|w| w.parse().ok()).unwrap_or(0)
}

/// `{affected, rows}` — what a write answers (ADR 0047).
#[cfg(feature = "postgres")]
fn outcome_value(
    out: Outcome,
    line: usize,
    col: usize,
    err: &dyn Fn(String) -> HelixError,
) -> Result<Value, HelixError> {
    let affected = rows_affected(&out.tag);
    let rows = frame_of(out.cols, line, col, err)?;
    Ok(Value::Record(std::rc::Rc::new(vec![
        (crate::symbol::Symbol::intern("affected"), Value::Int(affected)),
        (crate::symbol::Symbol::intern("rows"), Value::DataFrame(std::rc::Rc::new(rows))),
    ])))
}

/// `postgres_execute(url, sql, params?)` — run one statement that may WRITE, answering
/// `{affected, rows}`: the rows affected, and the rows returned (a frame — empty unless the
/// statement has a `RETURNING`). Everything is `postgres_query` except the session: the
/// startup packet omits the read-only default, which is why this verb spends the `db-write`
/// capability where `postgres_query` spends `net` (ADR 0047). One statement is one
/// transaction: it commits when it completes, and a failed one changed nothing.
#[cfg(feature = "postgres")]
pub fn postgres_execute(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let (Some(Value::Str(url)), Some(Value::Str(sql))) = (args.first(), args.get(1)) else {
        return Err(HelixError::new(
            "`postgres_execute` takes a connection URL and a SQL string",
            line,
            col,
        )
        .hint("e.g. `postgres_execute(\"postgres://user:pw@host/db\", \"insert into users (name) values ($1)\", [\"Ada\"]).affected`."));
    };
    let params = statement_params("postgres_execute", args.get(2), line, col)?;
    let target = parse_url(url.as_str(), line, col)?;
    let err = |m: String| {
        HelixError::new(
            format!("postgres {}@{}:{}/{}: {m}", target.user, target.host, target.port, target.database),
            line,
            col,
        )
    };
    let texts = param_texts(&params, line, col)?;
    // A WRITABLE SESSION: the startup packet without `default_transaction_read_only`.
    let mut s = connect(&target, false).map_err(&err)?;
    let out = run_statement(&mut s, sql.as_str(), &texts).map_err(&err)?;
    let _ = write_msg(&mut s, Some(b'X'), &[]);
    outcome_value(out, line, col, &err)
}

/// The same verb without the feature: it exists, it type-checks, and running it says so.
#[cfg(not(feature = "postgres"))]
pub fn postgres_execute(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let _ = args;
    Err(HelixError::new("this build has no PostgreSQL support", line, col)
        .hint("rebuild with `--features postgres`."))
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
    let mut s = connect(&target, true).map_err(&err)?;

    let cols = run_statement(&mut s, sql, &texts).map_err(&err)?.cols;

    // Best-effort goodbye: the answer is already in hand, so failing to say it must not
    // turn a successful query into an error.
    let _ = write_msg(&mut s, Some(b'X'), &[]);

    frame_of(cols, line, col, &err)
}

#[cfg(test)]
mod tests {
    use super::rows_affected;

    #[test]
    fn the_completion_tag_names_the_rows_affected() {
        assert_eq!(rows_affected("INSERT 0 3"), 3);
        assert_eq!(rows_affected("INSERT 0 1"), 1);
        assert_eq!(rows_affected("UPDATE 7"), 7);
        assert_eq!(rows_affected("DELETE 0"), 0);
        assert_eq!(rows_affected("MERGE 2"), 2);
        assert_eq!(rows_affected("SELECT 12"), 12);
        assert_eq!(rows_affected("CREATE TABLE"), 0);
        assert_eq!(rows_affected("BEGIN"), 0);
        assert_eq!(rows_affected(""), 0);
        assert_eq!(rows_affected("INSERT oops"), 0);
    }
}

/// A fake server that speaks just enough of the protocol to prove what the client sends —
/// the startup parameter that makes a session read-only, present for a query and ABSENT
/// for a write — and to hand back rows and a completion tag. It is the verification a box
/// without a server allows; the field build runs the real thing.
#[cfg(all(test, feature = "postgres"))]
mod wire_tests {
    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};

    /// One connection's script: whether the startup packet must carry the read-only
    /// default, and what the server answers — columns, rows, completion tag.
    struct Script {
        expect_read_only: bool,
        columns: Vec<&'static str>,
        rows: Vec<Vec<&'static str>>,
        tag: &'static str,
    }

    fn read_one(s: &mut TcpStream, tagged: bool) -> (u8, Vec<u8>) {
        let mut tag = [0u8; 1];
        if tagged {
            s.read_exact(&mut tag).unwrap();
        }
        let mut len = [0u8; 4];
        s.read_exact(&mut len).unwrap();
        let n = i32::from_be_bytes(len) as usize - 4;
        let mut body = vec![0u8; n];
        s.read_exact(&mut body).unwrap();
        (tag[0], body)
    }

    fn send(s: &mut TcpStream, tag: u8, body: &[u8]) {
        write_msg(s, Some(tag), body).unwrap();
    }

    /// Serve one connection; the thread returns the SQL the client sent.
    fn serve(script: Script) -> (u16, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (_, startup) = read_one(&mut s, false);
            let text = String::from_utf8_lossy(&startup).into_owned();
            assert_eq!(
                text.contains("default_transaction_read_only"),
                script.expect_read_only,
                "startup packet: {text:?}"
            );
            send(&mut s, b'R', &0i32.to_be_bytes()); // AuthenticationOk
            send(&mut s, b'Z', b"I"); // ReadyForQuery, idle
            let mut sql = String::new();
            loop {
                let (tag, body) = read_one(&mut s, true);
                match tag {
                    // Parse: unnamed statement (one NUL), then the statement text.
                    b'P' => {
                        let text = body[1..].split(|b| *b == 0).next().unwrap();
                        sql = String::from_utf8_lossy(text).into_owned();
                        send(&mut s, b'1', &[]);
                    }
                    b'B' => send(&mut s, b'2', &[]),
                    b'D' => {
                        if script.columns.is_empty() {
                            send(&mut s, b'n', &[]); // NoData
                        } else {
                            let mut out = Vec::new();
                            out.extend_from_slice(&(script.columns.len() as i16).to_be_bytes());
                            for c in &script.columns {
                                put_cstr(&mut out, c);
                                out.extend_from_slice(&0i32.to_be_bytes()); // table oid
                                out.extend_from_slice(&0i16.to_be_bytes()); // attnum
                                out.extend_from_slice(&23i32.to_be_bytes()); // int4
                                out.extend_from_slice(&4i16.to_be_bytes()); // typlen
                                out.extend_from_slice(&(-1i32).to_be_bytes()); // typmod
                                out.extend_from_slice(&0i16.to_be_bytes()); // text format
                            }
                            send(&mut s, b'T', &out);
                        }
                    }
                    b'E' => {
                        for r in &script.rows {
                            let mut out = Vec::new();
                            out.extend_from_slice(&(r.len() as i16).to_be_bytes());
                            for v in r {
                                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                                out.extend_from_slice(v.as_bytes());
                            }
                            send(&mut s, b'D', &out);
                        }
                        let mut out = Vec::new();
                        put_cstr(&mut out, script.tag);
                        send(&mut s, b'C', &out);
                    }
                    b'S' => send(&mut s, b'Z', b"I"),
                    b'X' => break,
                    other => panic!("unexpected message {:?}", other as char),
                }
            }
            sql
        });
        (port, h)
    }

    fn url(port: u16) -> String {
        format!("postgres://u:pw@127.0.0.1:{port}/db?sslmode=disable")
    }

    fn sv(s: &str) -> Value {
        Value::Str(std::rc::Rc::new(s.to_string()))
    }

    #[test]
    fn a_query_session_is_read_only_from_the_startup_packet() {
        let (port, h) = serve(Script {
            expect_read_only: true,
            columns: vec!["n"],
            rows: vec![vec!["7"]],
            tag: "SELECT 1",
        });
        let df = query(&url(port), "select 7 as n", &[], 1, 1).unwrap();
        assert_eq!(df.row_count(1, 1).unwrap(), 1);
        assert!(matches!(df.column_values("n", 1, 1).unwrap().as_slice(), [Value::Int(7)]));
        assert_eq!(h.join().unwrap(), "select 7 as n");
    }

    #[test]
    fn an_execute_session_omits_the_read_only_default_and_answers_affected() {
        let (port, h) = serve(Script {
            expect_read_only: false,
            columns: vec![],
            rows: vec![],
            tag: "INSERT 0 3",
        });
        let v = postgres_execute(&[sv(&url(port)), sv("insert into t values (1), (2), (3)")], 1, 1)
            .unwrap();
        let Value::Record(fields) = v else { panic!("not a record: {v:?}") };
        let get = |k: &str| {
            fields.iter().find(|(s, _)| s.as_str() == k).map(|(_, v)| v.clone()).unwrap()
        };
        assert!(matches!(get("affected"), Value::Int(3)), "{:?}", get("affected"));
        let Value::DataFrame(rows) = get("rows") else { panic!("rows is not a frame") };
        assert_eq!(rows.row_count(1, 1).unwrap(), 0, "no RETURNING, no rows");
        assert_eq!(h.join().unwrap(), "insert into t values (1), (2), (3)");
    }

    #[test]
    fn a_returning_statement_hands_back_its_rows_in_the_same_round_trip() {
        let (port, h) = serve(Script {
            expect_read_only: false,
            columns: vec!["id"],
            rows: vec![vec!["5"]],
            tag: "INSERT 0 1",
        });
        let v = postgres_execute(
            &[
                sv(&url(port)),
                sv("insert into t (x) values ($1) returning id"),
                Value::Array(std::rc::Rc::new(crate::value::ArrayData::Values(vec![Value::Int(9)]))),
            ],
            1,
            1,
        )
        .unwrap();
        let Value::Record(fields) = v else { panic!("not a record: {v:?}") };
        let rows = fields.iter().find(|(s, _)| s.as_str() == "rows").map(|(_, v)| v.clone()).unwrap();
        let Value::DataFrame(rows) = rows else { panic!("rows is not a frame") };
        assert!(matches!(rows.column_values("id", 1, 1).unwrap().as_slice(), [Value::Int(5)]));
        let affected = fields.iter().find(|(s, _)| s.as_str() == "affected").map(|(_, v)| v.clone());
        assert!(matches!(affected, Some(Value::Int(1))), "{affected:?}");
        h.join().unwrap();
    }

    #[test]
    fn a_read_only_connection_refuses_execute_before_sending_anything() {
        let (port, h) = serve(Script {
            expect_read_only: true,
            columns: vec![],
            rows: vec![],
            tag: "",
        });
        let c = postgres_open(&[sv(&url(port))], 1, 1).unwrap();
        let Value::Db(c) = c else { panic!("not a connection") };
        let err = conn_method(&c, "execute", &[sv("delete from t")], 1, 1).unwrap_err();
        // `Debug` escapes the quotes inside the hint; unescape before matching.
        let text = format!("{err:?}").replace("\\\"", "\"");
        assert!(text.contains("read-only, so it cannot execute"), "{text}");
        assert!(text.contains("postgres_open(url, \"write\")"), "{text}");
        drop(c); // says goodbye; the fake server returns on `X`
        h.join().unwrap();
    }

    #[test]
    fn a_writable_connection_executes_and_still_queries() {
        let (port, h) = serve(Script {
            expect_read_only: false,
            columns: vec![],
            rows: vec![],
            tag: "UPDATE 2",
        });
        let c = postgres_open(&[sv(&url(port)), sv("write")], 1, 1).unwrap();
        let Value::Db(c) = c else { panic!("not a connection") };
        let v = conn_method(&c, "execute", &[sv("update t set x = 1")], 1, 1).unwrap();
        let Value::Record(fields) = v else { panic!("not a record: {v:?}") };
        let affected = fields.iter().find(|(s, _)| s.as_str() == "affected").map(|(_, v)| v.clone());
        assert!(matches!(affected, Some(Value::Int(2))), "{affected:?}");
        drop(c);
        assert_eq!(h.join().unwrap(), "update t set x = 1");
    }
}

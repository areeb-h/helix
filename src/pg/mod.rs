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
use proto::{error_code, error_text, frame_msg, put_cstr, read_msg, send_framed, write_msg, Msg};
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
    /// The statements this connection has prepared on the server, by their text.
    #[cfg(feature = "postgres")]
    prepared: std::cell::RefCell<Prepared>,
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
        let mut prepared =
            self.prepared.try_borrow_mut().map_err(|_| "this connection is already in use".to_string())?;
        run_prepared(s, &mut prepared, sql, params)
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
        prepared: std::cell::RefCell::new(Prepared::default()),
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
        SslMode::Disable => Stream::plain(s),
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

/// How many statements a connection keeps prepared. Past it the least recently used one is
/// closed on the server as the new one is parsed, in the same round trip.
#[cfg(feature = "postgres")]
const MAX_PREPARED: usize = 256;

/// THE STATEMENTS A CONNECTION HAS PREPARED, BY THEIR TEXT (field build, §1.61).
///
/// Every query used to be Parsed from scratch as the unnamed statement: the server parsed,
/// analysed and rewrote the same text on every call. Measured against pgx on one PostgreSQL
/// 17, that was the whole gap to GORM on a small query — 23–57 µs of a ~150 µs round trip —
/// and a library cannot close it itself: SQL-level `EXECUTE p($1)` refuses a bound parameter,
/// so the only client-side cache is one that splices values into text, which is the one
/// thing a query layer must never do.
///
/// A text is Parsed ONCE under a name and thereafter only Bound and Executed. Nothing a
/// caller can see changes: parameter types were already inferred from the text alone (Parse
/// never saw the values), and the three ways a name can go stale are handled where they
/// surface — the statement gone (`26000`: `DEALLOCATE`, `DISCARD ALL`, a pooler's other
/// backend) or its result type changed under it (`0A000`: `ALTER TABLE`) is prepared again,
/// once; a name a user's own `PREPARE` already took (`42P05`) falls back to the unnamed
/// statement, which always works. The one-shot verbs keep the unnamed statement: a connection
/// that lives for one query has nothing to reuse.
#[cfg(feature = "postgres")]
#[derive(Default)]
struct Prepared {
    /// SQL text → the server-side name, and the tick of its last use.
    by_sql: std::collections::HashMap<String, (String, u64)>,
    tick: u64,
    next: u64,
}

#[cfg(feature = "postgres")]
impl Prepared {
    /// The name `sql` is prepared under, marked as just used.
    fn touch(&mut self, sql: &str) -> Option<String> {
        self.tick += 1;
        let tick = self.tick;
        self.by_sql.get_mut(sql).map(|(name, used)| {
            *used = tick;
            name.clone()
        })
    }

    /// A fresh name — and, when the cache is full, the least recently used statement's,
    /// which leaves the cache here and the server in the round trip that follows.
    fn reserve(&mut self) -> (String, Option<String>) {
        self.next += 1;
        let name = format!("_helix_{}", self.next);
        if self.by_sql.len() < MAX_PREPARED {
            return (name, None);
        }
        let oldest = self.by_sql.iter().min_by_key(|(_, (_, used))| *used).map(|(sql, _)| sql.clone());
        let evicted = oldest.and_then(|sql| self.by_sql.remove(&sql)).map(|(old, _)| old);
        (name, evicted)
    }

    fn insert(&mut self, sql: &str, name: String) {
        self.tick += 1;
        self.by_sql.insert(sql.to_string(), (name, self.tick));
    }
}

/// Why an exchange failed: the text for a reader, the SQLSTATE for the cache, and whether
/// the statement was parsed before it did — a statement whose Bind was refused (a parameter
/// of the wrong shape) exists on the server all the same.
#[cfg(feature = "postgres")]
struct Fail {
    text: String,
    code: String,
    parsed: bool,
}

#[cfg(feature = "postgres")]
impl From<String> for Fail {
    fn from(text: String) -> Self {
        Fail { text, code: String::new(), parsed: false }
    }
}

/// Run one statement on a connection that keeps what it prepares.
#[cfg(feature = "postgres")]
fn run_prepared(s: &mut Stream, prepared: &mut Prepared, sql: &str, params: &[Option<String>]) -> Result<Outcome, String> {
    // A HIT: Bind and Execute against the name — no Parse.
    if let Some(name) = prepared.touch(sql) {
        match exchange(s, &name, None, params) {
            // The server no longer has it, or its result type changed under it: forget the
            // name and prepare the text again below — once.
            Err(f) if f.code == "26000" || f.code == "0A000" => {
                prepared.by_sql.remove(sql);
            }
            other => return other.map_err(|f| f.text),
        }
    }
    // A MISS: Parse under a fresh name, closing the statement it displaces.
    let (name, evicted) = prepared.reserve();
    match exchange(s, &name, Some((sql, evicted.as_deref())), params) {
        Ok(out) => {
            prepared.insert(sql, name);
            Ok(out)
        }
        // The name is a user's own prepared statement: the unnamed one always works.
        Err(f) if f.code == "42P05" => exchange(s, "", Some((sql, None)), params).map_err(|f| f.text),
        Err(f) => {
            if f.parsed {
                prepared.insert(sql, name);
            }
            Err(f.text)
        }
    }
}

#[cfg(feature = "postgres")]
/// Run one parameterised statement as the UNNAMED statement: its rows, and its completion
/// tag. What a connection opened for one query uses.
fn run_statement(s: &mut Stream, sql: &str, params: &[Option<String>]) -> Result<Outcome, String> {
    exchange(s, "", Some((sql, None)), params).map_err(|f| f.text)
}

/// One round trip: optionally Close a displaced statement and Parse `sql` under `name` (the
/// empty name is the unnamed statement), then Bind, Describe, Execute and Sync.
#[cfg(feature = "postgres")]
fn exchange(
    s: &mut Stream,
    name: &str,
    parse: Option<(&str, Option<&str>)>,
    params: &[Option<String>],
) -> Result<Outcome, Fail> {
    let mut out = Vec::new();
    // The whole exchange, framed, to go out in ONE write.
    let mut wire: Vec<u8> = Vec::new();

    if let Some((sql, evicted)) = parse {
        // Closing a name the server does not have is not an error, so this needs no answer
        // of its own; `CloseComplete` is skipped with the other messages nobody waits for.
        if let Some(old) = evicted {
            out.push(b'S');
            put_cstr(&mut out, old);
            frame_msg(&mut wire, b'C', &out)?;
            out.clear();
        }
        // Parse: no declared parameter types (the server infers them from the text).
        put_cstr(&mut out, name);
        put_cstr(&mut out, sql);
        out.extend_from_slice(&0i16.to_be_bytes());
        frame_msg(&mut wire, b'P', &out)?;
    }

    // Bind: text in, text out.
    out.clear();
    put_cstr(&mut out, ""); // portal
    put_cstr(&mut out, name); // statement
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
    frame_msg(&mut wire, b'B', &out)?;

    // Describe the portal, so the column names and type OIDs arrive even for zero rows.
    out.clear();
    out.push(b'P');
    put_cstr(&mut out, "");
    frame_msg(&mut wire, b'D', &out)?;

    out.clear();
    put_cstr(&mut out, "");
    out.extend_from_slice(&0i32.to_be_bytes()); // unlimited rows
    frame_msg(&mut wire, b'E', &out)?;

    frame_msg(&mut wire, b'S', &[])?;
    send_framed(s, &wire)?;

    let mut cols: Vec<ColBuf> = Vec::new();
    let mut described = false;
    let mut parsed = false;
    let mut tag = String::new();
    loop {
        let m = read_msg(s)?;
        match m.tag {
            b'E' => {
                // Drain to the synchronisation point so the connection is left in a
                // known state even though this query is finished.
                let _ = wait_for(s, b"Z");
                return Err(Fail { text: error_text(&m), code: error_code(&m), parsed });
            }
            // ParseComplete: the statement exists on the server from here on.
            b'1' => parsed = true,
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
                    return Err(Fail::from(format!(
                        "the server sent a row of {n} values for {} columns",
                        cols.len()
                    )));
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
        return Err(Fail::from("the server never described the result".to_string()));
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

    /// What the fake server saw: the last statement's text, every `Parse`'s statement name in
    /// order (`""` is the unnamed statement), and every name it was asked to `Close`.
    struct Seen {
        sql: String,
        parses: Vec<String>,
        closes: Vec<String>,
    }

    /// Serve one connection; the thread returns what the client sent.
    fn serve(script: Script) -> (u16, std::thread::JoinHandle<Seen>) {
        serve_forgetting(script, None)
    }

    /// The same, with a server that FORGETS its prepared statements before the `forget_at`-th
    /// Bind — what `DEALLOCATE ALL`, `DISCARD ALL` or a pooler's other backend does to a name.
    fn serve_forgetting(script: Script, forget_at: Option<usize>) -> (u16, std::thread::JoinHandle<Seen>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            // Small replies, written one by one: without this Nagle holds each behind the
            // client's delayed ACK, ~40 ms a round trip.
            let _ = s.set_nodelay(true);
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
            let (mut parses, mut closes): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
            let mut known: Vec<String> = Vec::new();
            let mut binds = 0usize;
            // After an error the server discards messages until `Sync`, as the protocol says.
            let mut skipping = false;
            loop {
                let (tag, body) = read_one(&mut s, true);
                if skipping && tag != b'S' && tag != b'X' {
                    continue;
                }
                match tag {
                    // Parse: the statement's name (empty for the unnamed one), then its text.
                    b'P' => {
                        let mut parts = body.split(|b| *b == 0);
                        let name = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
                        sql = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
                        if !name.is_empty() {
                            known.push(name.clone());
                        }
                        parses.push(name);
                        send(&mut s, b'1', &[]);
                    }
                    // Close: `S` and a statement's name. Never an error, known or not.
                    b'C' => {
                        let name = String::from_utf8_lossy(body[1..].split(|b| *b == 0).next().unwrap()).into_owned();
                        known.retain(|k| *k != name);
                        closes.push(name);
                        send(&mut s, b'3', &[]);
                    }
                    // Bind: the portal, then the statement it binds — which must still exist.
                    b'B' => {
                        binds += 1;
                        if forget_at == Some(binds) {
                            known.clear();
                        }
                        let mut parts = body.split(|b| *b == 0);
                        let _portal = parts.next();
                        let stmt = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
                        if !stmt.is_empty() && !known.contains(&stmt) {
                            let mut out = Vec::new();
                            out.push(b'S');
                            put_cstr(&mut out, "ERROR");
                            out.push(b'C');
                            put_cstr(&mut out, "26000");
                            out.push(b'M');
                            put_cstr(&mut out, &format!("prepared statement \"{stmt}\" does not exist"));
                            out.push(0);
                            send(&mut s, b'E', &out);
                            skipping = true;
                        } else {
                            send(&mut s, b'2', &[]);
                        }
                    }
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
                    b'S' => {
                        skipping = false;
                        send(&mut s, b'Z', b"I")
                    }
                    b'X' => break,
                    other => panic!("unexpected message {:?}", other as char),
                }
            }
            Seen { sql, parses, closes }
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
        assert_eq!(h.join().unwrap().sql, "select 7 as n");
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
        assert_eq!(h.join().unwrap().sql, "insert into t values (1), (2), (3)");
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
        assert_eq!(h.join().unwrap().sql, "update t set x = 1");
    }

    /// A connection prepares a statement ONCE (§1.61): the same text again is a Bind and an
    /// Execute against the name it was parsed under, whatever the parameters; a different text
    /// is a statement of its own. The one-shot verbs keep the unnamed statement — a connection
    /// that lives for one query has nothing to reuse.
    #[test]
    fn a_connection_prepares_a_statement_once() {
        let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["7"]], tag: "SELECT 1" });
        let c = postgres_open(&[sv(&url(port))], 1, 1).unwrap();
        let Value::Db(c) = c else { panic!("not a connection") };
        let arr = |n: i64| Value::Array(std::rc::Rc::new(crate::value::ArrayData::Values(vec![Value::Int(n)])));
        for n in [1, 2, 3] {
            conn_method(&c, "query", &[sv("select $1::int as n"), arr(n)], 1, 1).unwrap();
        }
        conn_method(&c, "query", &[sv("select 7 as n")], 1, 1).unwrap();
        conn_method(&c, "query", &[sv("select $1::int as n"), arr(4)], 1, 1).unwrap();
        drop(c);
        let seen = h.join().unwrap();
        assert_eq!(seen.parses.len(), 2, "{:?}", seen.parses);
        assert!(seen.parses.iter().all(|n| !n.is_empty()) && seen.parses[0] != seen.parses[1], "{:?}", seen.parses);
        assert!(seen.closes.is_empty(), "{:?}", seen.closes);

        let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["7"]], tag: "SELECT 1" });
        query(&url(port), "select 7 as n", &[], 1, 1).unwrap();
        assert_eq!(h.join().unwrap().parses, vec![String::new()]);
    }

    /// A statement the server no longer has — `DEALLOCATE ALL`, `DISCARD ALL`, a pooler's other
    /// backend — is prepared again, ONCE, and the call answers as if nothing had happened.
    #[test]
    fn a_statement_the_server_forgot_is_prepared_again() {
        let script = Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["7"]], tag: "SELECT 1" };
        let (port, h) = serve_forgetting(script, Some(2));
        let c = postgres_open(&[sv(&url(port))], 1, 1).unwrap();
        let Value::Db(c) = c else { panic!("not a connection") };
        for _ in 0..3 {
            let v = conn_method(&c, "query", &[sv("select 7 as n")], 1, 1).unwrap();
            let Value::DataFrame(df) = v else { panic!("not a frame") };
            assert!(matches!(df.column_values("n", 1, 1).unwrap().as_slice(), [Value::Int(7)]));
        }
        drop(c);
        // Parsed; forgotten by the server before the second Bind; parsed again under a new name;
        // and the third call is a hit on that one.
        let seen = h.join().unwrap();
        assert_eq!(seen.parses.len(), 2, "{:?}", seen.parses);
        assert_ne!(seen.parses[0], seen.parses[1]);
    }

    /// The cache is bounded: past `MAX_PREPARED` statements the least recently used one is
    /// closed on the server as the new one is parsed, in the same round trip.
    #[test]
    fn the_least_recently_used_statement_is_closed_when_the_cache_is_full() {
        let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["7"]], tag: "SELECT 1" });
        let c = postgres_open(&[sv(&url(port))], 1, 1).unwrap();
        let Value::Db(c) = c else { panic!("not a connection") };
        for i in 0..=MAX_PREPARED {
            conn_method(&c, "query", &[sv(&format!("select {i} as n"))], 1, 1).unwrap();
        }
        drop(c);
        let seen = h.join().unwrap();
        assert_eq!(seen.parses.len(), MAX_PREPARED + 1);
        assert_eq!(seen.closes, vec![seen.parses[0].clone()]);
    }
}

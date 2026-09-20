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
//! framing; `connect` bounds the WAIT, with connect and read timeouts, because a server
//! that accepts a connection and then says nothing would otherwise hang a Helix program
//! with no way to interrupt it.
//!
//! WHERE THINGS ARE. This file is the Helix-facing surface: the three verbs, the connection
//! value and its methods. `connect` opens a session; `statement` runs one on it and keeps
//! what a connection remembers between calls; `stream` is the socket and its read buffer;
//! `proto` the framing; `types` the cells; `tls`, `scram` and `conninfo` what their names say.

/// The connection URL and its security policy — NOT gated, so the gate tests it. See the
/// module note; the `allow` is narrowed to exactly the build where "nothing calls this" is
/// the intended truth, rather than a blanket that could also hide a real dead branch.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
mod conninfo;
#[cfg(feature = "postgres")]
mod connect;
#[cfg(feature = "postgres")]
mod proto;
#[cfg(feature = "postgres")]
mod scram;
#[cfg(feature = "postgres")]
mod statement;
#[cfg(feature = "postgres")]
mod stream;
#[cfg(feature = "postgres")]
mod tls;
#[cfg(feature = "postgres")]
mod types;
#[cfg(all(test, feature = "postgres"))]
mod wire_tests;

use crate::backend::Df;
use crate::error::HelixError;
use crate::value::Value;

#[cfg(feature = "postgres")]
use connect::connect;
#[cfg(feature = "postgres")]
use conninfo::parse_url;
#[cfg(feature = "postgres")]
use proto::write_msg;
#[cfg(feature = "postgres")]
use statement::{run_prepared, run_statement, Outcome, Session};
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
    /// The session, which a transaction's value shares with the connection it was begun on.
    #[cfg(feature = "postgres")]
    shared: std::rc::Rc<Shared>,
    /// `Some` on the value `begin()` answered: which transaction it speaks for.
    #[cfg(feature = "postgres")]
    tx: Option<u64>,
}

/// What a connection value and the transactions begun on it have in common: one session.
#[cfg(feature = "postgres")]
struct Shared {
    state: std::cell::RefCell<State>,
    /// `user@host:port/database`, for diagnostics. Never the password.
    label: String,
    /// Opened with `"write"`: the startup packet omitted the read-only default, and the
    /// `db-write` grant was checked at `postgres_open`. `execute` on a read-only connection
    /// refuses BEFORE sending anything, with the spelling that opens a writable one.
    writable: bool,
    /// The transaction open through a value of its own, if one is — and how many have been
    /// begun, which is what tells one transaction's value from the next.
    open_tx: std::cell::Cell<Option<u64>>,
    begun: std::cell::Cell<u64>,
}

/// A connection is open, or it is closed and says why.
///
/// IT CLOSES ITSELF WHEN IT CAN NO LONGER BE TRUSTED. An exchange that does not end at
/// `ReadyForQuery` — the server went quiet past the read timeout, the socket dropped, a
/// message did not parse — leaves replies unread on the wire, and the next statement would
/// read them as its OWN: not an error but the wrong rows, silently. So the first such failure
/// is also the connection's last act, and every later call says what happened instead of
/// touching the socket. An error the SERVER reports is not one of these: it is read to its
/// end, and the connection carries on.
#[cfg(feature = "postgres")]
enum State {
    Open(Box<Session>),
    Closed(String),
}

/// A TRANSACTION IS A VALUE, AND ITS LIFETIME IS THE VALUE'S (ADR 0047 D5).
///
/// `tx = c.begin()` answers a connection value that speaks for the transaction: it takes
/// `query` and `execute` like any other — a library written against a connection takes it
/// unchanged — and ends with `tx.commit()` or `tx.rollback()`. ONE THAT IS DROPPED WITHOUT
/// COMMITTING ROLLS BACK. Helix values are reference-counted, so "dropped" is a moment, not an
/// eventuality: an error raised between `begin` and `commit` unwinds past `tx`, and the
/// rollback has been sent by the time a `try` around it answers. That is commit-on-success and
/// rollback-on-raise without a callback — which matters here, because a builtin cannot call
/// a closure the same way on the walker and the VM (the reason `postgres_with` was withdrawn,
/// ADR 0044 D7) — and it is the rule the connection itself already lives by: there is no
/// `close` to forget, and no `rollback` either.
///
/// WHILE IT IS OPEN, IT IS THE ONLY WAY IN. The session is one, so a statement sent through
/// the connection's own value would land INSIDE the transaction, silently; it is refused
/// instead, naming the transaction's value. A transaction's value that has ended refuses
/// everything. There is no nesting: `begin` on a transaction's value is refused by name.
#[cfg(feature = "postgres")]
impl Conn {
    /// Run one statement through this value — if it is the one that may speak.
    fn run(&self, sql: &str, params: &[Value]) -> Result<Outcome, String> {
        self.may_speak()?;
        self.shared.run(sql, params)
    }

    /// While a transaction is open, only its own value uses the session.
    fn may_speak(&self) -> Result<(), String> {
        match (self.tx, self.shared.open_tx.get()) {
            (None, None) => Ok(()),
            (Some(mine), Some(open)) if mine == open => Ok(()),
            (None, Some(_)) => Err("a transaction is open on this connection, so statements go through the transaction's own value until it commits or rolls back".to_string()),
            (Some(_), _) => Err("this transaction has ended — it was committed or rolled back".to_string()),
        }
    }
}

#[cfg(feature = "postgres")]
impl Shared {
    /// Where the server says the session stands: `I` idle, `T` in a transaction, `E` in one
    /// that has failed.
    fn status(&self) -> Result<u8, String> {
        match &*self.state.try_borrow().map_err(|_| "this connection is already in use".to_string())? {
            State::Open(session) => Ok(session.status),
            State::Closed(why) => {
                Err(format!("this connection is closed — an earlier statement on it failed with: {why}"))
            }
        }
    }

    /// Run one statement on this session.
    ///
    /// `try_borrow_mut` rather than `borrow_mut`: nothing here calls back into Helix while
    /// the borrow is held, so a conflict should be impossible — but "should be impossible"
    /// is what a host abort is made of, and ADR 0024 says user input must never abort the
    /// process. A clean error costs one line.
    fn run(&self, sql: &str, params: &[Value]) -> Result<Outcome, String> {
        let mut guard =
            self.state.try_borrow_mut().map_err(|_| "this connection is already in use".to_string())?;
        let session = match &mut *guard {
            State::Open(session) => session,
            State::Closed(why) => {
                return Err(format!("this connection is closed — an earlier statement on it failed with: {why}"))
            }
        };
        run_prepared(session, sql, params).map_err(|f| {
            if f.broken {
                // Dropping the session closes the socket. No goodbye: the protocol state is
                // unknown, so there is nothing safe to say.
                *guard = State::Closed(f.text.clone());
            }
            f.text
        })
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
    let session = connect(&target, !writable).map_err(|m| err(format!("postgres {label}: {m}")))?;
    // AN UNPROTECTED CONNECTION SAYS SO, in every error it ever produces. Only the unusual
    // case is marked: a verified TLS session is what asking for nothing gets you, so
    // annotating it would be noise, while `(plaintext)` appearing in a message is the
    // cheapest possible way for someone to notice an `sslmode=disable` that outlived the
    // afternoon it was added for.
    let label = if session.stream.is_tls() { label } else { format!("{label} (plaintext)") };
    Ok(Value::Db(std::rc::Rc::new(Conn {
        shared: std::rc::Rc::new(Shared {
            state: std::cell::RefCell::new(State::Open(Box::new(session))),
            label,
            writable,
            open_tx: std::cell::Cell::new(None),
            begun: std::cell::Cell::new(0),
        }),
        tx: None,
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
    /// A transaction's value that goes without having committed takes what it did with it.
    ///
    /// Failure is ignored, as below: if the rollback cannot be sent the session is closed,
    /// and a session that closes mid-transaction is rolled back by the server.
    fn drop(&mut self) {
        if let Some(mine) = self.tx
            && self.shared.open_tx.get() == Some(mine)
        {
            let _ = self.shared.run("rollback", &[]);
            self.shared.open_tx.set(None);
        }
    }
}

#[cfg(feature = "postgres")]
impl Drop for Shared {
    /// Say goodbye and drop the socket, when the last handle to it goes.
    ///
    /// Failure is ignored on purpose: the caller already has its answers, and a connection
    /// that cannot be closed politely is still closed when the descriptor goes. `Drop`
    /// must not raise, and there is nothing a program could do about it if it did.
    fn drop(&mut self) {
        if let Ok(mut guard) = self.state.try_borrow_mut()
            && let State::Open(session) = &mut *guard
        {
            let _ = write_msg(&mut session.stream, Some(b'X'), &[]);
            // And at the TLS layer, so the server sees a clean shutdown rather than a
            // truncated one it has to treat as a possible attack.
            session.stream.close_notify();
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
            if name == "execute" && !c.shared.writable {
                return Err(err(format!(
                    "postgres {}: this connection is read-only, so it cannot execute a statement",
                    c.shared.label
                ))
                .hint("open one that can write: `postgres_open(url, \"write\")` — it needs the `db-write` capability."));
            }
            let params = statement_params(name, args.get(1), line, col)?;
            let conn_err = |m: String| err(format!("postgres {}: {m}", c.shared.label));
            let out = c.run(sql.as_str(), &params).map_err(&conn_err)?;
            if name == "execute" {
                return outcome_value(out, line, col);
            }
            frame_of(out.cols, line, col).map(|df| Value::DataFrame(std::rc::Rc::new(df)))
        }
        "begin" => {
            let conn_err = |m: String| err(format!("postgres {}: {m}", c.shared.label));
            if c.tx.is_some() {
                return Err(conn_err("a transaction is already open, and Helix does not nest them".to_string())
                    .hint("end this one with `commit()` or `rollback()`, then `begin()` again on the connection."));
            }
            // THE ISOLATION LEVEL IS ONE OF THREE SENTENCES, never text a caller supplied.
            let sql = match args.first() {
                None | Some(Value::Missing) => "begin",
                Some(Value::Str(level)) => match level.as_str() {
                    "read committed" => "begin isolation level read committed",
                    "repeatable read" => "begin isolation level repeatable read",
                    "serializable" => "begin isolation level serializable",
                    other => {
                        return Err(err(format!("`{other}` is not an isolation level")).hint(
                            "`begin()` takes nothing (the server's default, read committed), or one of `\"read committed\"`, `\"repeatable read\"`, `\"serializable\"`.",
                        ))
                    }
                },
                Some(other) => {
                    return Err(err(format!(
                        "`begin` takes an optional isolation level as a String, got {}",
                        crate::value::with_article(other.type_name())
                    )))
                }
            };
            c.may_speak().map_err(&conn_err)?;
            // A `begin` sent as SQL is the caller's to end: a value that rolled it back when
            // dropped would be undoing work it was never given.
            if c.shared.status().map_err(&conn_err)? != b'I' {
                return Err(conn_err("a transaction begun in SQL is open on this connection".to_string())
                    .hint("end it the way it was begun — `execute(\"commit\")` or `execute(\"rollback\")` — or begin transactions with `begin()`."));
            }
            c.shared.run(sql, &[]).map_err(&conn_err)?;
            let id = c.shared.begun.get() + 1;
            c.shared.begun.set(id);
            c.shared.open_tx.set(Some(id));
            Ok(Value::Db(std::rc::Rc::new(Conn { shared: c.shared.clone(), tx: Some(id) })))
        }
        "commit" | "rollback" => {
            let conn_err = |m: String| err(format!("postgres {}: {m}", c.shared.label));
            if c.tx.is_none() {
                return Err(err(format!("`{name}` ends a transaction, and this is the connection itself"))
                    .hint(format!("`tx = c.begin()` opens one; `tx.{name}()` ends it.")));
            }
            c.may_speak().map_err(&conn_err)?;
            // A TRANSACTION THAT HAS FAILED CANNOT COMMIT — the server answers `COMMIT` with
            // `ROLLBACK` and no error, which a caller would take for success. It is rolled
            // back by name instead, and `commit` says so.
            let failed = c.shared.status().map_err(&conn_err)? == b'E';
            let ran = c.shared.run(if name == "commit" && !failed { "commit" } else { "rollback" }, &[]);
            // Whatever the server said, this value has spoken its last.
            c.shared.open_tx.set(None);
            ran.map_err(&conn_err)?;
            if name == "commit" && failed {
                return Err(conn_err("the transaction was rolled back, not committed: a statement in it had failed".to_string())
                    .hint("an error inside a transaction ends it — the statements before it are undone too."));
            }
            Ok(Value::Missing)
        }
        other => Err(err(format!(
            "{} has no method `{other}`",
            crate::value::with_article("Connection")
        ))
            .hint("a Connection answers `query(sql, params?)`, `execute(sql, params?)` when opened with `\"write\"`, and `begin()` — whose value also answers `commit()` and `rollback()`.")),
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

/// `params?` of a statement verb: an array of values, or nothing — each one a value SQL has
/// a form for. Refused here, with the line the call was written on, so that nothing is ever
/// framed for the wire that the server was not going to be sent.
#[cfg(feature = "postgres")]
fn statement_params(
    verb: &str,
    arg: Option<&Value>,
    line: usize,
    col: usize,
) -> Result<Vec<Value>, HelixError> {
    let params: Vec<Value> = match arg {
        None | Some(Value::Missing) => Vec::new(),
        Some(Value::Array(a)) => a.iter_values().collect(),
        Some(other) => {
            return Err(HelixError::new(
                format!(
                    "`{verb}` parameters must be an array, got {}",
                    crate::value::with_article(other.type_name())
                ),
                line,
                col,
            ))
        }
    };
    for (i, p) in params.iter().enumerate() {
        if !matches!(p, Value::Missing | Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_)) {
            return Err(HelixError::new(
                format!(
                    "parameter {} is {}, which has no SQL form",
                    i + 1,
                    crate::value::with_article(p.type_name())
                ),
                line,
                col,
            )
            .hint("parameters may be Int, Float, Bool, String, or missing (SQL NULL)."));
        }
    }
    Ok(params)
}

/// The rows a statement returned, as a frame — with no columns when it returned none.
#[cfg(feature = "postgres")]
fn frame_of(cols: Vec<ColBuf>, line: usize, col: usize) -> Result<Df, HelixError> {
    let mut built = Vec::with_capacity(cols.len());
    for mut c in cols {
        let name = std::mem::take(&mut c.name);
        built.push((name, c.finish()));
    }
    crate::backend::build_frame(built, line, col)
}

/// `{affected, rows}` — what a write answers (ADR 0047).
#[cfg(feature = "postgres")]
fn outcome_value(out: Outcome, line: usize, col: usize) -> Result<Value, HelixError> {
    let rows = frame_of(out.cols, line, col)?;
    Ok(Value::Record(std::rc::Rc::new(vec![
        (crate::symbol::Symbol::intern("affected"), Value::Int(out.affected)),
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
    // A WRITABLE SESSION: the startup packet without `default_transaction_read_only`.
    let mut session = connect(&target, false).map_err(&err)?;
    let out = run_statement(&mut session, sql.as_str(), &params).map_err(|f| err(f.text))?;
    let _ = write_msg(&mut session.stream, Some(b'X'), &[]);
    outcome_value(out, line, col)
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

    // READ-ONLY IS ALREADY ESTABLISHED, in the startup packet, before this or any other
    // statement could be sent (ADR 0038 D3, ADR 0044 D3). It used to be a
    // `begin transaction read only` here and a `commit` after — correct, but three round
    // trips where one will do, and a window (however short) in which the session was not
    // yet read-only. A guarantee that holds from the first byte is both cheaper and
    // stronger than one a client remembers to ask for.
    let mut session = connect(&target, true).map_err(&err)?;

    let cols = run_statement(&mut session, sql, params).map_err(|f| err(f.text))?.cols;

    // Best-effort goodbye: the answer is already in hand, so failing to say it must not
    // turn a successful query into an error.
    let _ = write_msg(&mut session.stream, Some(b'X'), &[]);

    frame_of(cols, line, col)
}

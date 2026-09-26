//! A fake server that speaks just enough of the protocol to prove what the client sends —
//! the startup parameter that makes a session read-only, present for a query and ABSENT
//! for a write — and to hand back rows and a completion tag. It is the verification a box
//! without a server allows; the field build runs the real thing.

use super::proto::{put_cstr, write_msg};
use super::statement::MAX_PREPARED;
use super::{conn_method, postgres_execute, postgres_open, query};
use crate::value::Value;
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
/// order (`""` is the unnamed statement), every name it was asked to `Close`, how many times
/// it was asked to `Describe` a result, and — per Bind — how many columns were asked for in
/// binary.
struct Seen {
    sql: String,
    parses: Vec<String>,
    closes: Vec<String>,
    describes: usize,
    binary_columns: Vec<usize>,
    /// The text of every statement EXECUTED, in order.
    executed: Vec<String>,
    /// The startup packet, NULs and all.
    startup: String,
    /// How many times the client said Sync: once per round trip.
    syncs: usize,
    /// The row limit of every Execute, in order (0 is all of them).
    limits: Vec<i32>,
    /// Every portal the client closed, by name.
    portals_closed: Vec<String>,
}

/// What a server can do beyond answering its script.
#[derive(Default)]
struct Twists {
    /// FORGET every prepared statement before this Bind (1-based) — what `DEALLOCATE ALL`,
    /// `DISCARD ALL` or a pooler's other backend does to a name. (Their NAMES: a portal bound
    /// from one survives, as it does on PostgreSQL 17 — measured.)
    forget_at: Option<usize>,
    /// And forget them all again before this Bind: a second `DEALLOCATE ALL`.
    forget_also_at: Option<usize>,
    /// On this Execute (1-based), answer one row whose one cell is not its column's type — a
    /// refusal this client makes and the server knows nothing of.
    bad_at: Option<usize>,
    /// From the second Execute on, answer these rows instead of the script's.
    later_rows: Option<Vec<Vec<&'static str>>>,
    /// On this Execute (1-based), send one row and HANG UP — no completion, no ReadyForQuery.
    hang_up_at: Option<usize>,
    /// Answer this Execute (1-based) with CopyInResponse, as `COPY … FROM STDIN` does, and wait.
    copy_in_at: Option<usize>,
    /// Fail this Execute (1-based) with a unique violation, as a statement inside a
    /// transaction might.
    fail_at: Option<usize>,
    /// Let the FIRST Execute run this long and then cancel it, as `statement_timeout` does.
    cancel_first_after: Option<std::time::Duration>,
}

/// Serve one connection; the thread returns what the client sent.
fn serve(script: Script) -> (u16, std::thread::JoinHandle<Seen>) {
    serve_with(script, Twists::default())
}

/// The same, with a server that forgets its prepared statements before the `forget_at`-th Bind.
fn serve_forgetting(script: Script, forget_at: Option<usize>) -> (u16, std::thread::JoinHandle<Seen>) {
    serve_with(script, Twists { forget_at, ..Twists::default() })
}

/// The format codes at the end of a Bind: past the portal, the statement, the parameter
/// formats and the parameters themselves.
fn result_formats(body: &[u8]) -> Vec<i16> {
    let i16_at = |at: usize| i16::from_be_bytes([body[at], body[at + 1]]);
    let mut at = 0;
    for _ in 0..2 {
        at += body[at..].iter().position(|b| *b == 0).unwrap() + 1;
    }
    at += 2 + 2 * i16_at(at) as usize;
    let params = i16_at(at);
    at += 2;
    for _ in 0..params {
        let len = i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
        at += 4 + len.max(0) as usize;
    }
    let n = i16_at(at) as usize;
    (0..n).map(|k| i16_at(at + 2 + 2 * k)).collect()
}

fn serve_with(script: Script, twists: Twists) -> (u16, std::thread::JoinHandle<Seen>) {
    let forget_at = twists.forget_at;
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
        // Each statement's text by its name, the one the last Bind bound, every one executed —
        // and where the session stands: `I` idle, `T` in a transaction, `E` in a failed one.
        let mut texts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut bound = String::new();
        let mut executed: Vec<String> = Vec::new();
        let mut status = b'I';
        let (mut parses, mut closes): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        let mut known: Vec<String> = Vec::new();
        let mut binds = 0usize;
        let (mut describes, mut executes) = (0usize, 0usize);
        let mut binary_columns: Vec<usize> = Vec::new();
        // After an error the server discards messages until `Sync`, as the protocol says.
        let mut skipping = false;
        // Waiting for COPY data: a Sync that arrives now was sent before the client could know,
        // and is ignored — as the protocol says.
        let mut copying = false;
        let mut syncs = 0usize;
        // Per portal: its statement's text, the formats its Bind asked for, and how many rows it
        // has sent — a named portal suspends at its limit and is Executed again for the rest.
        let mut portal_text: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut portal_formats: std::collections::HashMap<String, Vec<i16>> = std::collections::HashMap::new();
        let mut sent: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut limits: Vec<i32> = Vec::new();
        let mut portals_closed: Vec<String> = Vec::new();
        // The named portals that exist, each with the statement it was bound from. A portal lives
        // from its Bind until it is closed, its transaction ends, or — the protocol's documented
        // contract, stricter than PostgreSQL 17, which keeps the portal — the statement it was
        // bound from is closed. A client that passes here works against both.
        let mut portal_stmt: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        loop {
            let (tag, body) = read_one(&mut s, true);
            if tag == b'S' {
                syncs += 1;
            }
            if skipping && tag != b'S' && tag != b'X' {
                continue;
            }
            // Waiting for COPY data, anything that is not COPY data (or a Flush or a Sync, which
            // are ignored) is the END OF THE CONNECTION, as it is with the real server: it has
            // read the message's type and will not read the rest, so it has lost its place.
            if copying && !matches!(tag, b'd' | b'c' | b'f' | b'H' | b'S') {
                let mut out = Vec::new();
                out.push(b'S');
                put_cstr(&mut out, "FATAL");
                out.push(b'C');
                put_cstr(&mut out, "08P01");
                out.push(b'M');
                put_cstr(&mut out, "terminating connection because protocol synchronization was lost");
                out.push(0);
                send(&mut s, b'E', &out);
                return Seen { sql, parses, closes, describes, binary_columns, executed, startup: text, syncs, limits, portals_closed };
            }
            // AN EXECUTE RUNS THE PORTAL IT NAMES: its statement's text is what `bound` means from
            // here, whatever was bound last — and a named portal has to exist.
            let mut no_portal: Option<String> = None;
            if tag == b'E' {
                let portal = String::from_utf8_lossy(body.split(|b| *b == 0).next().unwrap()).into_owned();
                if !portal.is_empty() && !portal_stmt.contains_key(&portal) {
                    no_portal = Some(portal);
                } else if let Some(text) = portal_text.get(&portal) {
                    bound = text.clone();
                }
            }
            match tag {
                // Parse: the statement's name (empty for the unnamed one), then its text.
                b'P' => {
                    let mut parts = body.split(|b| *b == 0);
                    let name = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
                    sql = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
                    if name.is_empty() {
                        // A new unnamed statement replaces the old one — and, by the documented
                        // contract, the portals bound from it.
                        portal_stmt.retain(|_, from| !from.is_empty());
                    } else {
                        known.push(name.clone());
                    }
                    texts.insert(name.clone(), sql.clone());
                    parses.push(name);
                    send(&mut s, b'1', &[]);
                }
                // Close: `S` and a statement's name, or `P` and a portal's. Never an error,
                // known or not.
                b'C' => {
                    let name = String::from_utf8_lossy(body[1..].split(|b| *b == 0).next().unwrap()).into_owned();
                    if body[0] == b'P' {
                        portal_stmt.remove(&name);
                        sent.remove(&name);
                        portals_closed.push(name);
                    } else {
                        known.retain(|k| *k != name);
                        // The documented contract: closing a statement closes its portals.
                        portal_stmt.retain(|_, from| *from != name);
                        closes.push(name);
                    }
                    send(&mut s, b'3', &[]);
                }
                // Bind: the portal, then the statement it binds — which must still exist.
                b'B' => {
                    binds += 1;
                    if forget_at == Some(binds) || twists.forget_also_at == Some(binds) {
                        known.clear();
                    }
                    let mut parts = body.split(|b| *b == 0);
                    let portal = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
                    let stmt = String::from_utf8_lossy(parts.next().unwrap()).into_owned();
                    if !stmt.is_empty() && !known.contains(&stmt) {
                        // An error inside a transaction fails it, as the real server's does.
                        if status == b'T' {
                            status = b'E';
                        }
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
                        bound = texts.get(&stmt).cloned().unwrap_or_default();
                        let formats = result_formats(&body);
                        binary_columns.push(formats.iter().filter(|f| **f == 1).count());
                        // A Bind makes the portal afresh, whatever it had sent before.
                        portal_text.insert(portal.clone(), bound.clone());
                        portal_formats.insert(portal.clone(), formats);
                        if !portal.is_empty() {
                            portal_stmt.insert(portal.clone(), stmt);
                        }
                        sent.insert(portal, 0);
                        send(&mut s, b'2', &[]);
                    }
                }
                b'D' => {
                    describes += 1;
                    if script.columns.is_empty() || bound.starts_with("begin") || bound == "commit" || bound == "rollback" {
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
                // A named portal that does not exist: never bound, closed, ended with its
                // transaction, or taken by its statement's Close.
                b'E' if no_portal.is_some() => {
                    executes += 1;
                    if status == b'T' {
                        status = b'E';
                    }
                    let name = no_portal.take().unwrap_or_default();
                    let mut out = Vec::new();
                    out.push(b'S');
                    put_cstr(&mut out, "ERROR");
                    out.push(b'C');
                    put_cstr(&mut out, "34000");
                    out.push(b'M');
                    put_cstr(&mut out, &format!("portal \"{name}\" does not exist"));
                    out.push(0);
                    send(&mut s, b'E', &out);
                    skipping = true;
                }
                b'E' if twists.copy_in_at == Some(executes + 1) => {
                    executes += 1;
                    copying = true;
                    send(&mut s, b'G', &[0, 0, 0]); // text format, no columns
                }
                // CopyFail: the COPY fails with the client's own words, and the rest is skipped.
                b'f' => {
                    copying = false;
                    let why = String::from_utf8_lossy(body.split(|b| *b == 0).next().unwrap()).into_owned();
                    let mut out = Vec::new();
                    out.push(b'S');
                    put_cstr(&mut out, "ERROR");
                    out.push(b'C');
                    put_cstr(&mut out, "57014");
                    out.push(b'M');
                    put_cstr(&mut out, &format!("COPY from stdin failed: {why}"));
                    out.push(0);
                    send(&mut s, b'E', &out);
                    skipping = true;
                }
                b'S' if copying => {}
                // A failed transaction answers nothing but its end; and a statement can be
                // told to fail, which inside a transaction is what fails it.
                b'E' if (status == b'E' && bound != "commit" && bound != "rollback")
                    || twists.fail_at == Some(executes + 1) =>
                {
                    executes += 1;
                    let (code, text) = if status == b'E' {
                        ("25P02", "current transaction is aborted, commands ignored until end of transaction block")
                    } else {
                        ("23505", "duplicate key value violates unique constraint")
                    };
                    // An error inside a transaction fails it; a COMMIT that fails has ENDED it —
                    // rolled back, as a deferred constraint's failure does — portals and all.
                    if bound == "commit" {
                        status = b'I';
                        portal_stmt.clear();
                    } else if status == b'T' {
                        status = b'E';
                    }
                    let mut out = Vec::new();
                    out.push(b'S');
                    put_cstr(&mut out, "ERROR");
                    out.push(b'C');
                    put_cstr(&mut out, code);
                    out.push(b'M');
                    put_cstr(&mut out, text);
                    out.push(0);
                    send(&mut s, b'E', &out);
                    skipping = true;
                }
                // The three statements that move a session between its states: they return
                // nothing, whatever this connection's script says a query returns.
                // `statement_timeout`: the statement ran as long as the server was told to let
                // it, and the server ends it — an ordinary error, read to its end.
                b'E' if twists.cancel_first_after.is_some() && executes == 0 => {
                    executes += 1;
                    std::thread::sleep(twists.cancel_first_after.unwrap());
                    let mut out = Vec::new();
                    out.push(b'S');
                    put_cstr(&mut out, "ERROR");
                    out.push(b'C');
                    put_cstr(&mut out, "57014");
                    out.push(b'M');
                    put_cstr(&mut out, "canceling statement due to statement timeout");
                    out.push(0);
                    send(&mut s, b'E', &out);
                    skipping = true;
                }
                b'E' if bound.starts_with("begin") || bound == "commit" || bound == "rollback" => {
                    executes += 1;
                    executed.push(bound.clone());
                    status = if bound.starts_with("begin") { b'T' } else { b'I' };
                    if status == b'I' {
                        portal_stmt.clear();
                    }
                    let mut out = Vec::new();
                    put_cstr(&mut out, &bound.to_uppercase());
                    send(&mut s, b'C', &out);
                }
                b'E' => {
                    executes += 1;
                    // Execute: the portal, and how many rows (0 is all of them). A named portal
                    // keeps its place between Executes, as the real one does; an unnamed one was
                    // bound afresh just before.
                    let portal = String::from_utf8_lossy(body.split(|b| *b == 0).next().unwrap()).into_owned();
                    let at = portal.len() + 1;
                    let limit = i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
                    limits.push(limit);
                    executed.push(portal_text.get(&portal).cloned().unwrap_or_else(|| bound.clone()));
                    if twists.bad_at == Some(executes) {
                        // One row, its one cell `x` as text: not an int4 whatever format was asked.
                        let mut out = Vec::new();
                        out.extend_from_slice(&1i16.to_be_bytes());
                        out.extend_from_slice(&1i32.to_be_bytes());
                        out.push(b'x');
                        send(&mut s, b'D', &out);
                        let mut out = Vec::new();
                        put_cstr(&mut out, script.tag);
                        send(&mut s, b'C', &out);
                        continue;
                    }
                    let formats = portal_formats.get(&portal).cloned().unwrap_or_default();
                    let rows = match &twists.later_rows {
                        Some(later) if executes > 1 => later,
                        _ => &script.rows,
                    };
                    let from = sent.get(&portal).copied().unwrap_or(0).min(rows.len());
                    let take = if limit > 0 { (limit as usize).min(rows.len() - from) } else { rows.len() - from };
                    for r in &rows[from..from + take] {
                        let mut out = Vec::new();
                        out.extend_from_slice(&(r.len() as i16).to_be_bytes());
                        for (k, v) in r.iter().enumerate() {
                            // One code is every column's; otherwise each has its own.
                            let format = if formats.len() == 1 { formats[0] } else { formats.get(k).copied().unwrap_or(0) };
                            if format == 1 {
                                // Every column here is int4.
                                out.extend_from_slice(&4i32.to_be_bytes());
                                out.extend_from_slice(&v.parse::<i32>().unwrap().to_be_bytes());
                            } else {
                                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                                out.extend_from_slice(v.as_bytes());
                            }
                        }
                        send(&mut s, b'D', &out);
                        if twists.hang_up_at == Some(executes) {
                            return Seen { sql, parses, closes, describes, binary_columns, executed, startup: text, syncs, limits, portals_closed };
                        }
                    }
                    sent.insert(portal, from + take);
                    // The real server suspends the moment the limit is reached — before it
                    // knows whether a row follows — and completes on the Execute that runs short.
                    if limit > 0 && take == limit as usize {
                        send(&mut s, b's', &[]);
                    } else {
                        let mut out = Vec::new();
                        put_cstr(&mut out, script.tag);
                        send(&mut s, b'C', &out);
                    }
                }
                b'S' => {
                    skipping = false;
                    if status == b'I' {
                        portal_stmt.clear();
                    }
                    send(&mut s, b'Z', &[status])
                }
                b'X' => break,
                other => panic!("unexpected message {:?}", other as char),
            }
        }
        Seen { sql, parses, closes, describes, binary_columns, executed, startup: text, syncs, limits, portals_closed }
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

/// WHAT A STATEMENT RETURNS IS LEARNED ONCE. Its first run asks the server to describe the
/// result and reads it as text; every later run neither asks nor waits for that, and takes its
/// fixed-width columns in binary — the same values, which is the whole point of the test.
#[test]
fn a_statement_that_has_run_is_not_described_again_and_its_integers_cross_in_binary() {
    let (port, h) = serve(Script {
        expect_read_only: true,
        columns: vec!["a", "b"],
        rows: vec![vec!["7", "-2147483648"], vec!["-1", "2147483647"]],
        tag: "SELECT 2",
    });
    let c = postgres_open(&[sv(&url(port))], 1, 1).unwrap();
    let Value::Db(c) = c else { panic!("not a connection") };
    for run in 0..3 {
        let v = conn_method(&c, "query", &[sv("select a, b from t")], 1, 1).unwrap();
        let Value::DataFrame(df) = v else { panic!("not a frame") };
        assert!(
            matches!(df.column_values("a", 1, 1).unwrap().as_slice(), [Value::Int(7), Value::Int(-1)]),
            "run {run}"
        );
        assert!(
            matches!(
                df.column_values("b", 1, 1).unwrap().as_slice(),
                [Value::Int(-2147483648), Value::Int(2147483647)]
            ),
            "run {run}"
        );
    }
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.parses.len(), 1);
    assert_eq!(seen.describes, 1, "described on its first run and never again");
    assert_eq!(seen.binary_columns, vec![0, 2, 2], "text the first time, binary after");

    // The one-shot verb has no second run: text, described, unnamed — as it always was.
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["7"]], tag: "SELECT 1" });
    query(&url(port), "select 7 as n", &[], 1, 1).unwrap();
    let seen = h.join().unwrap();
    assert_eq!((seen.describes, seen.binary_columns), (1, vec![0]));
}

/// A CELL THAT IS NOT ITS COLUMN'S TYPE is an error naming the column and the value — and the
/// rest of the result is still read, so the connection is where the next statement expects it.
/// The error used to surface after the read, from a second pass over strings; it now surfaces
/// during it, which is exactly where stopping early would leave rows on the wire for the next
/// statement to mistake for its own.
#[test]
fn a_bad_cell_is_an_error_and_the_connection_carries_on() {
    let script = Script {
        expect_read_only: true,
        columns: vec!["n"],
        rows: vec![vec!["1"], vec!["abc"], vec!["3"]],
        tag: "SELECT 3",
    };
    let twists = Twists { later_rows: Some(vec![vec!["4"], vec!["5"]]), ..Twists::default() };
    let (port, h) = serve_with(script, twists);
    let c = postgres_open(&[sv(&url(port))], 1, 1).unwrap();
    let Value::Db(c) = c else { panic!("not a connection") };
    let err = conn_method(&c, "query", &[sv("select n from t")], 1, 1).unwrap_err();
    assert!(err.message.ends_with("column `n`: `abc` is not an integer"), "{}", err.message);
    for _ in 0..2 {
        let v = conn_method(&c, "query", &[sv("select n from t")], 1, 1).unwrap();
        let Value::DataFrame(df) = v else { panic!("not a frame") };
        assert!(matches!(df.column_values("n", 1, 1).unwrap().as_slice(), [Value::Int(4), Value::Int(5)]));
    }
    drop(c);
    // Parsed once: the statement exists on the server whatever its first result held.
    assert_eq!(h.join().unwrap().parses.len(), 1);
}

/// A CONNECTION THAT CAN NO LONGER BE TRUSTED CLOSES ITSELF. The server hangs up in the middle
/// of a result: that statement fails, and so does every later one on the connection — saying
/// what happened, without touching the socket. The alternative is the failure this exists to
/// prevent: a statement reading its predecessor's unread replies as its own rows.
#[test]
fn a_connection_whose_exchange_did_not_finish_is_never_used_again() {
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["1"], vec!["2"]], tag: "SELECT 2" };
    let (port, h) = serve_with(script, Twists { hang_up_at: Some(1), ..Twists::default() });
    let c = postgres_open(&[sv(&url(port))], 1, 1).unwrap();
    let Value::Db(c) = c else { panic!("not a connection") };
    let first = conn_method(&c, "query", &[sv("select n from t")], 1, 1).unwrap_err();
    // An orderly close or a reset, as the kernel has it — either way the read failed.
    assert!(first.message.contains("reading from the server"), "{}", first.message);
    h.join().unwrap();
    let later = conn_method(&c, "query", &[sv("select 1")], 1, 1).unwrap_err();
    assert!(later.message.contains("this connection is closed"), "{}", later.message);
    assert!(later.message.contains("reading from the server"), "it says why: {}", later.message);
}

/// `COPY … FROM STDIN` makes the server WAIT for rows, and a Helix connection has none to send.
/// Left alone that wait was the whole read timeout, and then a connection nobody could use;
/// the client refuses instead, the COPY fails as an ordinary error, and the connection is
/// where the next statement expects it.
#[test]
fn a_copy_from_stdin_is_refused_at_once_and_the_connection_carries_on() {
    let script = Script { expect_read_only: false, columns: vec!["n"], rows: vec![vec!["9"]], tag: "SELECT 1" };
    let (port, h) = serve_with(script, Twists { copy_in_at: Some(1), ..Twists::default() });
    let c = postgres_open(&[sv(&url(port)), sv("write")], 1, 1).unwrap();
    let Value::Db(c) = c else { panic!("not a connection") };
    let started = std::time::Instant::now();
    let err = conn_method(&c, "execute", &[sv("copy t from stdin")], 1, 1).unwrap_err();
    assert!(err.message.contains("COPY from stdin failed"), "{}", err.message);
    assert!(err.message.contains("does not stream COPY data"), "{}", err.message);
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "refused, not waited out");
    let v = conn_method(&c, "query", &[sv("select 9 as n")], 1, 1).unwrap();
    let Value::DataFrame(df) = v else { panic!("not a frame") };
    assert!(matches!(df.column_values("n", 1, 1).unwrap().as_slice(), [Value::Int(9)]));
    drop(c);
    h.join().unwrap();
}

fn write_conn(port: u16) -> std::rc::Rc<super::Conn> {
    let Value::Db(c) = postgres_open(&[sv(&url(port)), sv("write")], 1, 1).unwrap() else { panic!("not a connection") };
    c
}

fn begin(c: &std::rc::Rc<super::Conn>, args: &[Value]) -> std::rc::Rc<super::Conn> {
    let Value::Db(tx) = conn_method(c, "begin", args, 1, 1).unwrap() else { panic!("`begin` answers a connection value") };
    tx
}

/// A TRANSACTION IS A VALUE (ADR 0047 D5): what goes through it is one transaction on the
/// server, `commit()` ends it, the connection is its own again afterwards, and the value that
/// spoke for the transaction has spoken its last.
#[test]
fn a_transaction_commits_through_its_own_value() {
    let (port, h) = serve(Script { expect_read_only: false, columns: vec![], rows: vec![], tag: "UPDATE 1" });
    let c = write_conn(port);
    let tx = begin(&c, &[]);
    for id in [1, 2] {
        let arr = Value::Array(std::rc::Rc::new(crate::value::ArrayData::Values(vec![Value::Int(id)])));
        conn_method(&tx, "execute", &[sv("update accounts set n = n + 1 where id = $1"), arr], 1, 1).unwrap();
    }
    assert!(matches!(conn_method(&tx, "commit", &[], 1, 1).unwrap(), Value::Missing));
    // Ended: nothing more goes through it, and saying so costs no round trip.
    for verb in ["query", "execute", "commit", "rollback"] {
        let e = conn_method(&tx, verb, &[sv("select 1")], 1, 1).unwrap_err();
        assert!(e.message.contains("this transaction has ended"), "{verb}: {}", e.message);
    }
    conn_method(&c, "execute", &[sv("update accounts set n = 0")], 1, 1).unwrap();
    drop(tx);
    drop(c);
    assert_eq!(
        h.join().unwrap().executed,
        ["begin", "update accounts set n = n + 1 where id = $1", "update accounts set n = n + 1 where id = $1", "commit", "update accounts set n = 0"]
    );
}

/// ONE THAT IS DROPPED WITHOUT COMMITTING ROLLS BACK — which is what makes an error between
/// `begin` and `commit` undo everything, with nothing for a caller to remember.
#[test]
fn a_transaction_dropped_without_committing_rolls_back() {
    let (port, h) = serve(Script { expect_read_only: false, columns: vec![], rows: vec![], tag: "INSERT 0 1" });
    let c = write_conn(port);
    {
        let tx = begin(&c, &[sv("serializable")]);
        conn_method(&tx, "execute", &[sv("insert into t values (1)")], 1, 1).unwrap();
    }
    // The connection is its own again the moment the value is gone.
    conn_method(&c, "execute", &[sv("insert into t values (2)")], 1, 1).unwrap();
    // And an explicit rollback sends exactly one.
    let tx = begin(&c, &[]);
    conn_method(&tx, "execute", &[sv("insert into t values (3)")], 1, 1).unwrap();
    assert!(matches!(conn_method(&tx, "rollback", &[], 1, 1).unwrap(), Value::Missing));
    drop(tx);
    drop(c);
    assert_eq!(
        h.join().unwrap().executed,
        [
            "begin isolation level serializable",
            "insert into t values (1)",
            "rollback",
            "insert into t values (2)",
            "begin",
            "insert into t values (3)",
            "rollback"
        ]
    );
}

/// WHILE IT IS OPEN, ITS VALUE IS THE ONLY WAY IN. The session is one, so a statement through
/// the connection's own value would land inside the transaction, silently — it is refused, and
/// nothing is sent. Nor do transactions nest, nor does the connection itself commit.
#[test]
fn an_open_transaction_is_the_only_way_into_its_session() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["1"]], tag: "SELECT 1" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let e = conn_method(&c, "commit", &[], 1, 1).unwrap_err();
    assert!(e.message.contains("`commit` ends a transaction, and this is the connection itself"), "{}", e.message);
    let e = conn_method(&c, "begin", &[sv("chaos")], 1, 1).unwrap_err();
    assert!(e.message.contains("`chaos` is not an isolation level"), "{}", e.message);

    // A read-only connection begins one too: several queries, one snapshot.
    let tx = begin(&c, &[sv("repeatable read")]);
    let e = conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap_err();
    assert!(e.message.contains("a transaction is open on this connection"), "{}", e.message);
    let e = conn_method(&c, "begin", &[], 1, 1).unwrap_err();
    assert!(e.message.contains("a transaction is open on this connection"), "{}", e.message);
    let e = conn_method(&tx, "begin", &[], 1, 1).unwrap_err();
    assert!(e.message.contains("does not nest"), "{}", e.message);
    conn_method(&tx, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    conn_method(&tx, "commit", &[], 1, 1).unwrap();
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    drop(tx);
    drop(c);
    assert_eq!(
        h.join().unwrap().executed,
        ["begin isolation level repeatable read", "select 1 as n", "commit", "select 1 as n"]
    );
}

/// A TRANSACTION THAT HAS FAILED CANNOT COMMIT. The server would answer `COMMIT` with
/// `ROLLBACK` and no error, which a caller would take for success: it is rolled back by name,
/// and `commit()` raises.
#[test]
fn a_failed_transaction_rolls_back_and_commit_says_so() {
    let script = Script { expect_read_only: false, columns: vec![], rows: vec![], tag: "INSERT 0 1" };
    let (port, h) = serve_with(script, Twists { fail_at: Some(3), ..Twists::default() });
    let c = write_conn(port);
    let tx = begin(&c, &[]);
    conn_method(&tx, "execute", &[sv("insert into t values (1)")], 1, 1).unwrap();
    let e = conn_method(&tx, "execute", &[sv("insert into t values (1) -- again")], 1, 1).unwrap_err();
    assert!(e.message.contains("23505"), "{}", e.message);
    // The server now refuses everything but the end, and says why itself.
    let e = conn_method(&tx, "execute", &[sv("insert into t values (2)")], 1, 1).unwrap_err();
    assert!(e.message.contains("25P02"), "{}", e.message);
    let e = conn_method(&tx, "commit", &[], 1, 1).unwrap_err();
    assert!(e.message.contains("rolled back, not committed"), "{}", e.message);
    conn_method(&c, "execute", &[sv("insert into t values (3)")], 1, 1).unwrap();
    drop(tx);
    drop(c);
    assert_eq!(h.join().unwrap().executed, ["begin", "insert into t values (1)", "rollback", "insert into t values (3)"]);
}

/// HOW LONG A STATEMENT MAY RUN IS THE SERVER'S TO ENFORCE, when the URL names a limit: it goes
/// out in the startup packet, in milliseconds, with no round trip of its own. A URL that says
/// nothing, or `timeout=0`, asks the server for nothing.
#[test]
fn a_timeout_in_the_url_is_asked_of_the_server_in_the_startup_packet() {
    for (extra, want) in [("", None), ("&timeout=0", None), ("&timeout=300", Some("statement_timeout\u{0}300000\u{0}"))] {
        let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["7"]], tag: "SELECT 1" });
        query(&format!("{}{extra}", url(port)), "select 7 as n", &[], 1, 1).unwrap();
        let startup = h.join().unwrap().startup;
        match want {
            Some(w) => assert!(startup.contains(w), "`{extra}`: {startup:?}"),
            None => assert!(!startup.contains("statement_timeout"), "`{extra}`: {startup:?}"),
        }
    }
}

/// A STATEMENT THE SERVER ENDED FOR RUNNING TOO LONG is an ordinary error — it says where the
/// limit lives, and the connection carries on, which is the whole difference from the client
/// giving up: that closes the connection and leaves the server working.
#[test]
fn a_statement_that_outruns_its_timeout_is_an_error_and_the_connection_carries_on() {
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["9"]], tag: "SELECT 1" };
    let twists = Twists { cancel_first_after: Some(std::time::Duration::from_millis(1050)), ..Twists::default() };
    let (port, h) = serve_with(script, twists);
    let Value::Db(c) = postgres_open(&[sv(&format!("{}&timeout=1", url(port)))], 1, 1).unwrap() else { panic!("not a connection") };
    let e = conn_method(&c, "query", &[sv("select pg_sleep(60)")], 1, 1).unwrap_err();
    assert!(e.message.contains("57014"), "{}", e.message);
    assert!(e.message.contains("this connection's URL says `timeout=1`"), "{}", e.message);
    let v = conn_method(&c, "query", &[sv("select 9 as n")], 1, 1).unwrap();
    let Value::DataFrame(df) = v else { panic!("not a frame") };
    assert!(matches!(df.column_values("n", 1, 1).unwrap().as_slice(), [Value::Int(9)]));
    drop(c);
    h.join().unwrap();

    // A cancel that came EARLY was somebody's request, not the limit: the server's words stand alone.
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["9"]], tag: "SELECT 1" };
    let twists = Twists { cancel_first_after: Some(std::time::Duration::from_millis(10)), ..Twists::default() };
    let (port, h) = serve_with(script, twists);
    let Value::Db(c) = postgres_open(&[sv(&format!("{}&timeout=60", url(port)))], 1, 1).unwrap() else { panic!("not a connection") };
    let e = conn_method(&c, "query", &[sv("select pg_sleep(60)")], 1, 1).unwrap_err();
    assert!(e.message.contains("57014") && !e.message.contains("timeout="), "{}", e.message);
    drop(c);
    h.join().unwrap();
}

fn array(vs: Vec<Value>) -> Value {
    Value::Array(std::rc::Rc::new(crate::value::ArrayData::Values(vs)))
}

fn statement(sql: &str, params: Vec<Value>) -> Value {
    Value::Record(std::rc::Rc::new(vec![
        (crate::symbol::Symbol::intern("sql"), sv(sql)),
        (crate::symbol::Symbol::intern("params"), array(params)),
    ]))
}

fn cursor(c: &std::rc::Rc<super::Conn>, sql: &str, batch: i64) -> std::rc::Rc<super::Conn> {
    let Value::Db(cur) = conn_method(c, "cursor", &[sv(sql), Value::Missing, Value::Int(batch)], 1, 1).unwrap() else {
        panic!("`cursor` answers a cursor value")
    };
    cur
}

fn page(cur: &std::rc::Rc<super::Conn>) -> Vec<i64> {
    ints_of(&conn_method(cur, "next", &[], 1, 1).unwrap(), "n")
}

fn numbered(n: usize) -> Vec<Vec<&'static str>> {
    ["1", "2", "3", "4", "5", "6", "7"][..n].iter().map(|s| vec![*s]).collect()
}

/// A CURSOR READS A RESULT A PAGE AT A TIME (ADR 0044 addendum 2026-09-26): the statement is
/// bound to a named portal and Executed with a row limit; each `next()` is one more Execute of
/// the same portal; the page after the last is empty, at no round trip, then and thereafter.
/// Opened on the connection, it has a transaction of its own — begun in the round trip that
/// reads its first page, committed after its last — and the connection answers nothing else
/// meanwhile. A cursor's value answers `next()` and nothing else; `next` on the connection is
/// refused by name.
#[test]
fn a_cursor_reads_a_result_a_page_at_a_time() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: numbered(5), tag: "SELECT 5" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let cur = cursor(&c, "select n from t", 2);
    assert_eq!(cur.type_name(), "Cursor");
    assert_eq!(format!("{}", Value::Db(cur.clone())), "<postgres-cursor>");
    let e = conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap_err();
    assert!(e.message.contains("a cursor is open on this connection"), "{}", e.message);
    assert_eq!(page(&cur), vec![1, 2]);
    assert_eq!(page(&cur), vec![3, 4]);
    assert_eq!(page(&cur), vec![5]);
    assert_eq!(page(&cur), Vec::<i64>::new());
    assert_eq!(page(&cur), Vec::<i64>::new());
    // Read to its end, it has let the connection go.
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    for verb in ["query", "execute", "begin", "commit", "rollback", "cursor"] {
        let e = conn_method(&cur, verb, &[sv("select 1")], 1, 1).unwrap_err();
        assert!(e.message.contains(&format!("a Cursor has no method `{verb}`")), "{verb}: {}", e.message);
    }
    let e = conn_method(&c, "next", &[], 1, 1).unwrap_err();
    assert!(e.message.contains("this is the connection itself"), "{}", e.message);
    let e = conn_method(&cur, "next", &[Value::Int(1)], 1, 1).unwrap_err();
    assert!(e.message.contains("`next` takes no arguments"), "{}", e.message);
    drop(cur);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", "select n from t", "select n from t", "select n from t", "commit", "select 1 as n"]);
    assert_eq!(seen.limits, [2, 2, 2, 0], "a page of two, three times; then the query, all of it");
    assert_eq!(seen.syncs, 5, "the open with its first page, two more pages, the commit, the query");
    assert!(seen.portals_closed.is_empty(), "the transaction's end took the portal with it");
}

/// A RESULT THAT FITS ONE PAGE has been read to its end by the round trip that opened the
/// cursor: its transaction is committed at once, and the connection is free before `next()`
/// was ever called.
#[test]
fn a_result_that_fits_one_page_ends_its_cursor_at_once() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: numbered(3), tag: "SELECT 3" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let cur = cursor(&c, "select n from t", 10);
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    assert_eq!(page(&cur), vec![1, 2, 3]);
    assert_eq!(page(&cur), Vec::<i64>::new());
    drop(cur);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", "select n from t", "commit", "select 1 as n"]);
    assert_eq!(seen.syncs, 3);
}

/// DROPPED BEFORE ITS END, a cursor's value takes its transaction with it — the rollback is
/// what ends the portal — and the connection is its own again the moment the value is gone.
#[test]
fn a_cursor_dropped_before_its_end_takes_its_transaction_with_it() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: numbered(5), tag: "SELECT 5" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    {
        let cur = cursor(&c, "select n from t", 2);
        assert_eq!(page(&cur), vec![1, 2]);
    }
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", "select n from t", "rollback", "select 1 as n"]);
    assert!(seen.portals_closed.is_empty());
}

/// INSIDE A TRANSACTION'S VALUE, a cursor reads beside its statements: the transaction answers
/// queries between pages and ends by its own value; a cursor read to its end has its portal
/// closed and leaves the transaction alone, one dropped early has it closed too — each Close
/// riding with the next exchange, never a round trip of its own — and one whose transaction
/// ended under it says so, after handing over the page it already held.
#[test]
fn a_cursor_inside_a_transaction_reads_beside_its_statements() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: numbered(4), tag: "SELECT 4" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    {
        let tx = begin(&c, &[]);
        let cur = cursor(&tx, "select n from t", 2);
        assert_eq!(page(&cur), vec![1, 2]);
        conn_method(&tx, "query", &[sv("select 9 as n")], 1, 1).unwrap();
        assert_eq!(page(&cur), vec![3, 4]);
        // Four rows in pages of two: the third Execute answers no rows, and the portal is closed.
        assert_eq!(page(&cur), Vec::<i64>::new());
        conn_method(&tx, "query", &[sv("select 9 as n")], 1, 1).unwrap();
        conn_method(&tx, "commit", &[], 1, 1).unwrap();
    }
    {
        let tx = begin(&c, &[]);
        let cur = cursor(&tx, "select n from t", 2);
        assert_eq!(page(&cur), vec![1, 2]);
        drop(cur);
        conn_method(&tx, "query", &[sv("select 9 as n")], 1, 1).unwrap();
        conn_method(&tx, "commit", &[], 1, 1).unwrap();
    }
    {
        let tx = begin(&c, &[]);
        let cur = cursor(&tx, "select n from t", 2);
        conn_method(&tx, "rollback", &[], 1, 1).unwrap();
        assert_eq!(page(&cur), vec![1, 2], "the page that came with the open is already here");
        let e = conn_method(&cur, "next", &[], 1, 1).unwrap_err();
        assert!(e.message.contains("this cursor's transaction has ended"), "{}", e.message);
    }
    drop(c);
    let seen = h.join().unwrap();
    let sql = "select n from t";
    assert_eq!(
        seen.executed,
        ["begin", sql, "select 9 as n", sql, sql, "select 9 as n", "commit", "begin", sql, "select 9 as n", "commit", "begin", sql, "rollback"]
    );
    assert_eq!(seen.portals_closed, ["_helix_cursor_1", "_helix_cursor_2"]);
    assert_eq!(seen.syncs, 11, "6 + 3 + 2 round trips: each BEGIN rode with its transaction's first exchange, and no Close took one of its own");
}

/// A CURSOR'S PAGES CROSS IN BINARY once its statement is known — bound as `query` binds it, so
/// the formats are fixed for every page of the portal — and a statement the connection has not
/// run is described on the open and read as text, as its first run always is.
#[test]
fn a_cursor_pages_in_binary_once_its_statement_is_known() {
    let rows = vec![vec!["7"], vec!["-1"], vec!["2147483647"]];
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows, tag: "SELECT 3" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let first = cursor(&c, "select n from t", 2);
    assert_eq!(page(&first), vec![7, -1]);
    assert_eq!(page(&first), vec![2147483647]);
    let second = cursor(&c, "select n from t", 2);
    assert_eq!(page(&second), vec![7, -1]);
    assert_eq!(page(&second), vec![2147483647]);
    drop(first);
    drop(second);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.describes, 5, "begin and commit every time (transaction control is never cached), the first cursor's statement once, the second's not at all");
    assert_eq!(seen.binary_columns, [0, 0, 0, 0, 1, 0], "text on the first open, binary on the second — the Binds of begin and commit ask nothing");
}

/// A PAGE THAT FAILS ENDS THE CURSOR: the error is the caller's, the transaction begun for the
/// cursor is rolled back at once, the connection is free, and the cursor answers empty pages
/// from then on.
#[test]
fn a_page_that_fails_ends_the_cursor_and_frees_the_connection() {
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: numbered(5), tag: "SELECT 5" };
    let (port, h) = serve_with(script, Twists { fail_at: Some(3), ..Twists::default() });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let cur = cursor(&c, "select n from t", 2);
    assert_eq!(page(&cur), vec![1, 2]);
    let e = conn_method(&cur, "next", &[], 1, 1).unwrap_err();
    assert!(e.message.contains("23505"), "{}", e.message);
    assert_eq!(page(&cur), Vec::<i64>::new());
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    drop(cur);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", "select n from t", "rollback", "select 1 as n"]);
}

/// A CURSOR WHOSE STATEMENT WENT STALE UNDER IT rolls back the transaction its `begin` opened
/// and opens again — once, however often it happens. Found live: `deallocate all` between
/// caching a statement and opening a cursor on it. The cursor is `begin; <statement>` in one
/// flight, so the stale name's `26000` arrives INSIDE the transaction the begin opened and fails
/// it; the retry has to wait for that transaction's rollback — and the rollback has to be the
/// unnamed statement, because a CACHED `rollback` goes stale with everything else. The second
/// open below is where it did: the retry then ran inside the failed transaction (`25P02`).
#[test]
fn a_cursor_whose_statement_went_stale_rolls_back_and_opens_again() {
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: numbered(3), tag: "SELECT 3" };
    // Forgotten before the third Bind (the first cursor's statement, cached by the query) and
    // again before the ninth (the second cursor's, cached by the first one's retry).
    let twists = Twists { forget_at: Some(3), forget_also_at: Some(9), ..Twists::default() };
    let (port, h) = serve_with(script, twists);
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let sql = "select n from t";
    conn_method(&c, "query", &[sv(sql)], 1, 1).unwrap();
    let first = cursor(&c, sql, 2);
    assert_eq!(page(&first), vec![1, 2]);
    drop(first);
    let second = cursor(&c, sql, 2);
    assert_eq!(page(&second), vec![1, 2]);
    assert_eq!(page(&second), vec![3]);
    assert_eq!(page(&second), Vec::<i64>::new());
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    drop(second);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(
        seen.executed,
        [sql, "begin", "rollback", "begin", sql, "rollback", "begin", "rollback", "begin", sql, sql, "commit", "select 1 as n"],
        "each open: its flight fails, an unnamed rollback ends it, the retry opens — and the connection carries on"
    );
    assert_eq!(seen.closes, ["_helix_1", "_helix_2"], "each stale name closed by the rollback that followed it");
}

/// A CURSOR'S STATEMENT IS NEVER THE ONE DISPLACED. The protocol's documented contract is that
/// closing a prepared statement closes the portals bound from it, and the cache's own eviction
/// is such a Close: a transaction that runs more distinct statements between two pages than the
/// cache holds would make the cursor's statement the least recently used. (PostgreSQL 17 keeps
/// the portal — measured — but this server keeps the documented contract.) The pin lasts as long
/// as the portal: once the transaction is over, the statement is the next to go.
#[test]
fn a_cursors_statement_is_never_the_one_displaced() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: numbered(3), tag: "SELECT 3" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let tx = begin(&c, &[]);
    let cur = cursor(&tx, "select n from t", 1);
    assert_eq!(page(&cur), vec![1]);
    for i in 0..MAX_PREPARED {
        conn_method(&tx, "query", &[sv(&format!("select {i} as n"))], 1, 1).unwrap();
    }
    assert_eq!(page(&cur), vec![2], "the portal outlived {MAX_PREPARED} other statements");
    assert_eq!(page(&cur), vec![3]);
    conn_method(&tx, "commit", &[], 1, 1).unwrap();
    for i in MAX_PREPARED..MAX_PREPARED + 2 {
        conn_method(&c, "query", &[sv(&format!("select {i} as n"))], 1, 1).unwrap();
    }
    drop(cur);
    drop(tx);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.closes, ["_helix_2", "_helix_1", "_helix_3"], "spared while its portal lived, the next to go once it had not");
}

/// A CURSOR THAT FAILS TO OPEN LEAVES NO TRANSACTION BEHIND. Its `begin` took, then something went
/// wrong — here this client refusing a cell that is not its column's type, which the server knows
/// nothing of: the transaction is still open there, and a cursor that returned the error without
/// ending it left the connection inside a transaction nobody had begun. It is rolled back, and
/// the connection is its own.
#[test]
fn a_cursor_that_fails_to_open_leaves_no_transaction_behind() {
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: numbered(3), tag: "SELECT 3" };
    // Execute 1 is the begin, 2 the cursor's first page.
    let (port, h) = serve_with(script, Twists { bad_at: Some(2), ..Twists::default() });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let e = conn_method(&c, "cursor", &[sv("select n from t"), Value::Missing, Value::Int(2)], 1, 1).unwrap_err();
    assert!(e.message.contains("column `n`: `x` is not an integer"), "{}", e.message);
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    let tx = begin(&c, &[]);
    conn_method(&tx, "query", &[sv("select 2 as n")], 1, 1).unwrap();
    conn_method(&tx, "commit", &[], 1, 1).unwrap();
    drop(tx);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", "select n from t", "rollback", "select 1 as n", "begin", "select 2 as n", "commit"]);
}

/// A CURSOR'S OWN COMMIT THAT FAILS IS THE LAST PAGE'S ERROR. Over a statement that writes, a
/// deferred constraint or a serialization failure refuses the COMMIT after the last page — and
/// undoes everything the pages said had happened. It used to be dropped, and the last page came
/// back as if all were well.
#[test]
fn a_cursor_whose_transaction_cannot_commit_says_so() {
    let script = Script { expect_read_only: false, columns: vec!["n"], rows: numbered(3), tag: "INSERT 0 3" };
    // Execute 1 is the begin, 2 and 3 the pages, 4 the commit — which fails.
    let (port, h) = serve_with(script, Twists { fail_at: Some(4), ..Twists::default() });
    let c = write_conn(port);
    let sql = "insert into t select g from generate_series(1, 3) g returning g as n";
    let cur = cursor(&c, sql, 2);
    assert_eq!(page(&cur), vec![1, 2]);
    let e = conn_method(&cur, "next", &[], 1, 1).unwrap_err();
    assert!(e.message.contains("23505"), "{}", e.message);
    assert_eq!(page(&cur), Vec::<i64>::new(), "and the cursor is over");
    conn_method(&c, "execute", &[sv("insert into t values (9)")], 1, 1).unwrap();
    drop(cur);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", sql, sql, "insert into t values (9)"]);
}

/// A TRANSACTION'S BEGIN RIDES WITH ITS FIRST EXCHANGE. The server takes a transaction's
/// snapshot at its first statement, not at `BEGIN`, so the two in one round trip are the same
/// transaction — and `begin()` no longer costs a round trip of its own. A first exchange that is
/// a flight carries it at its head.
#[test]
fn a_transactions_begin_rides_with_its_first_exchange() {
    let (port, h) = serve(Script { expect_read_only: false, columns: vec![], rows: vec![], tag: "UPDATE 1" });
    let c = write_conn(port);
    let first = begin(&c, &[sv("serializable")]);
    conn_method(&first, "execute", &[sv("update t set n = 1")], 1, 1).unwrap();
    conn_method(&first, "commit", &[], 1, 1).unwrap();
    let second = begin(&c, &[]);
    let flight = array(vec![sv("update t set n = 2"), sv("update t set n = 3")]);
    conn_method(&second, "execute", std::slice::from_ref(&flight), 1, 1).unwrap();
    conn_method(&second, "commit", &[], 1, 1).unwrap();
    drop(first);
    drop(second);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(
        seen.executed,
        ["begin isolation level serializable", "update t set n = 1", "commit", "begin", "update t set n = 2", "update t set n = 3", "commit"]
    );
    assert_eq!(seen.syncs, 4, "each transaction: its BEGIN with its first exchange, then its COMMIT");
}

/// A TRANSACTION THAT SENDS NOTHING COSTS NOTHING: committed, rolled back or dropped, it never
/// reached the server — and each still ends, leaving the connection its own.
#[test]
fn a_transaction_that_sends_nothing_costs_nothing() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["1"]], tag: "SELECT 1" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let committed = begin(&c, &[]);
    assert!(matches!(conn_method(&committed, "commit", &[], 1, 1).unwrap(), Value::Missing));
    let rolled_back = begin(&c, &[sv("repeatable read")]);
    assert!(matches!(conn_method(&rolled_back, "rollback", &[], 1, 1).unwrap(), Value::Missing));
    {
        let _dropped = begin(&c, &[]);
    }
    for (tx, verb) in [(&committed, "commit"), (&rolled_back, "rollback")] {
        let e = conn_method(tx, verb, &[], 1, 1).unwrap_err();
        assert!(e.message.contains("this transaction has ended"), "{verb}: {}", e.message);
    }
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    drop(committed);
    drop(rolled_back);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["select 1 as n"]);
    assert_eq!(seen.syncs, 1, "three transactions and a query: one round trip");
}

/// A NAME GONE STALE IN A TRANSACTION'S FIRST EXCHANGE is prepared again: the BEGIN rode with it,
/// so the transaction holds nothing of the caller's yet — it is rolled back, and the exchange goes
/// again, once, BEGIN and all. (It used to fail the transaction: the BEGIN had gone alone, and the
/// stale name then failed a transaction already under way.)
#[test]
fn a_stale_name_in_a_transactions_first_exchange_is_prepared_again() {
    let script = Script { expect_read_only: false, columns: vec![], rows: vec![], tag: "UPDATE 1" };
    // Bind 1 caches the update; the server forgets it before Bind 3 — the update again, in the
    // transaction's first exchange (Bind 2 is its BEGIN).
    let (port, h) = serve_with(script, Twists { forget_at: Some(3), ..Twists::default() });
    let c = write_conn(port);
    let sql = "update t set n = n + 1";
    conn_method(&c, "execute", &[sv(sql)], 1, 1).unwrap();
    let tx = begin(&c, &[]);
    conn_method(&tx, "execute", &[sv(sql)], 1, 1).unwrap();
    conn_method(&tx, "commit", &[], 1, 1).unwrap();
    drop(tx);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, [sql, "begin", "rollback", "begin", sql, "commit"]);
    assert_eq!(seen.closes, ["_helix_1"], "the stale name closed with the rollback");
}

/// AN ERROR IN A TRANSACTION'S FIRST FLIGHT names the caller's statement — the BEGIN at its head
/// is not counted — and has failed the transaction, which began: `commit()` says so.
#[test]
fn an_error_in_a_transactions_first_flight_counts_the_callers_statements() {
    let script = Script { expect_read_only: false, columns: vec![], rows: vec![], tag: "INSERT 0 1" };
    // Execute 1 is the BEGIN, 3 the flight's second statement.
    let (port, h) = serve_with(script, Twists { fail_at: Some(3), ..Twists::default() });
    let c = write_conn(port);
    let tx = begin(&c, &[]);
    let flight = array(vec![sv("insert into t values (1)"), sv("insert into t values (1) -- again")]);
    let e = conn_method(&tx, "execute", std::slice::from_ref(&flight), 1, 1).unwrap_err();
    assert!(e.message.contains("statement 2 of 2: duplicate key"), "{}", e.message);
    let e = conn_method(&tx, "commit", &[], 1, 1).unwrap_err();
    assert!(e.message.contains("rolled back, not committed"), "{}", e.message);
    drop(tx);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", "insert into t values (1)", "rollback"]);
}

/// TRANSACTION CONTROL IS NEVER CACHED. A server that forgets its prepared statements between
/// a `begin` and its end (`DEALLOCATE ALL`, a pooler's other backend) used to be answered a
/// stale `commit` — a `26000` INSIDE the transaction, which failed it, with nothing left to
/// end it: the session was stuck. `begin`, `commit` and `rollback` go as the unnamed statement,
/// so a transaction's value and a cursor's own transaction end whatever the server forgot.
#[test]
fn transaction_control_is_never_cached() {
    let script = Script { expect_read_only: false, columns: vec![], rows: vec![], tag: "INSERT 0 1" };
    // Forgotten before the third Bind: `begin`, the insert, then `commit` — which must not care.
    let (port, h) = serve_with(script, Twists { forget_at: Some(3), ..Twists::default() });
    let c = write_conn(port);
    let tx = begin(&c, &[]);
    conn_method(&tx, "execute", &[sv("insert into t values (1)")], 1, 1).unwrap();
    conn_method(&tx, "commit", &[], 1, 1).unwrap();
    conn_method(&c, "execute", &[sv("insert into t values (2)")], 1, 1).unwrap();
    drop(tx);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", "insert into t values (1)", "commit", "insert into t values (2)"]);
    assert_eq!(seen.parses, ["", "_helix_1", "", "_helix_2"], "control unnamed every time; the insert prepared again after the server forgot it");

    // And a cursor's own transaction: forgotten before its `commit` (begin, the statement, commit).
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: numbered(3), tag: "SELECT 3" };
    let (port, h) = serve_with(script, Twists { forget_at: Some(3), ..Twists::default() });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let cur = cursor(&c, "select n from t", 2);
    assert_eq!(page(&cur), vec![1, 2]);
    assert_eq!(page(&cur), vec![3]);
    conn_method(&c, "query", &[sv("select 1 as n")], 1, 1).unwrap();
    drop(cur);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.executed, ["begin", "select n from t", "select n from t", "commit", "select 1 as n"]);
}

/// WHAT `cursor` TAKES is checked before a byte is sent: one statement, its parameters as an
/// Array, and a page size that is a positive Int — each refusal naming the spelling.
#[test]
fn a_cursor_is_checked_before_it_is_sent() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: numbered(1), tag: "SELECT 1" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let refused = |args: &[Value], want: &str| {
        let e = conn_method(&c, "cursor", args, 1, 1).unwrap_err();
        let text = format!("{e:?}").replace("\\\"", "\"");
        assert!(text.contains(want), "{want:?} not in {text}");
    };
    refused(&[], "takes a SQL string");
    refused(&[array(vec![sv("select 1")])], "reads one statement a page at a time");
    refused(&[sv("select 1"), Value::Int(5)], "the page size comes third");
    refused(&[sv("select 1"), sv("x")], "parameters must be an array");
    refused(&[sv("select 1"), Value::Missing, Value::Int(0)], "between 1 and 2147483647 rows");
    refused(&[sv("select 1"), Value::Missing, sv("many")], "a positive number of rows, got a String");
    refused(&[sv("select 1"), Value::Missing, Value::Int(2), Value::Int(3)], "at most three arguments");
    drop(c);
    assert_eq!(h.join().unwrap().syncs, 0, "nothing was sent");
}

fn ints_of(v: &Value, column: &str) -> Vec<i64> {
    let Value::DataFrame(df) = v else { panic!("not a frame: {v:?}") };
    df.column_values(column, 1, 1)
        .unwrap()
        .iter()
        .map(|c| if let Value::Int(n) = c { *n } else { panic!("not an Int: {c:?}") })
        .collect()
}

/// SEVERAL STATEMENTS, ONE ROUND TRIP. `query` handed an Array of statements sends them
/// together and says Sync ONCE; what comes back is an Array of frames, in order. Each
/// statement goes as it would alone — a text met twice is parsed once, and the second time the
/// whole flight runs nothing is parsed or described and the integers cross in binary.
#[test]
fn several_statements_share_one_round_trip() {
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["1"]], tag: "SELECT 1" };
    let twists = Twists { later_rows: Some(vec![vec!["2"], vec!["3"]]), ..Twists::default() };
    let (port, h) = serve_with(script, twists);
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let flight = array(vec![
        sv("select n from a"),
        statement("select n from b where id = $1", vec![Value::Int(7)]),
        statement("select n from b where id = $1", vec![Value::Int(8)]),
    ]);
    for round in 0..2 {
        let Value::Array(answers) = conn_method(&c, "query", std::slice::from_ref(&flight), 1, 1).unwrap() else {
            panic!("a flight answers an Array")
        };
        let answers: Vec<Value> = answers.iter_values().collect();
        assert_eq!(answers.len(), 3);
        // The fake server's first Execute answers one row and every later one two.
        let first = if round == 0 { vec![1] } else { vec![2, 3] };
        assert_eq!(ints_of(&answers[0], "n"), first, "round {round}");
        assert_eq!(ints_of(&answers[1], "n"), vec![2, 3]);
        assert_eq!(ints_of(&answers[2], "n"), vec![2, 3]);
    }
    assert!(matches!(conn_method(&c, "query", &[array(vec![])], 1, 1).unwrap(), Value::Array(a) if a.is_empty()));
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.syncs, 2, "two flights, two round trips — and the empty one none");
    assert_eq!(seen.parses.len(), 2, "two texts, each parsed once: {:?}", seen.parses);
    assert_eq!(seen.describes, 3, "each statement of the first flight, none of the second");
    assert_eq!(seen.binary_columns, vec![0, 0, 0, 1, 1, 1], "text the first time, binary after");
    assert_eq!(seen.executed.len(), 6);
}

/// A FLIGHT IS ALL OR NOTHING, and its error says whose it was. The server skips everything
/// after a failed statement to the flight's one Sync, and rolls back everything before it; the
/// client reads to that Sync, so the connection carries on — remembering exactly what the
/// server now has: the statement parsed before the failure, and not the one after it.
#[test]
fn a_flight_fails_as_one_and_names_the_statement() {
    let script = Script { expect_read_only: false, columns: vec![], rows: vec![], tag: "INSERT 0 1" };
    let (port, h) = serve_with(script, Twists { fail_at: Some(2), ..Twists::default() });
    let c = write_conn(port);
    let flight = array(vec![sv("insert into t values (1)"), sv("insert into t values (1) -- again"), sv("insert into t values (3)")]);
    let e = conn_method(&c, "execute", std::slice::from_ref(&flight), 1, 1).unwrap_err();
    assert!(e.message.contains("statement 2 of 3: "), "{}", e.message);
    assert!(e.message.contains("23505"), "{}", e.message);
    // Again, and nothing fails this time: three answers, each `{affected, rows}`.
    let Value::Array(answers) = conn_method(&c, "execute", &[flight], 1, 1).unwrap() else { panic!("an Array") };
    assert_eq!(answers.len(), 3);
    for a in answers.iter_values() {
        let Value::Record(fields) = a else { panic!("not a record: {a:?}") };
        assert!(fields.iter().any(|(k, v)| k.as_str() == "affected" && matches!(v, Value::Int(1))));
    }
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.syncs, 2);
    // First flight: statements 1 and 2 parsed, 3 skipped by the server. Second: only 3 is new.
    assert_eq!(seen.parses.len(), 3, "{:?}", seen.parses);
    assert_eq!(
        seen.executed,
        ["insert into t values (1)", "insert into t values (1)", "insert into t values (1) -- again", "insert into t values (3)"]
    );
}

/// What a flight is made of is checked before anything is sent, and the error counts as a
/// person does.
#[test]
fn a_flight_is_checked_before_it_is_sent() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["1"]], tag: "SELECT 1" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let record = |fields: Vec<(&str, Value)>| {
        Value::Record(std::rc::Rc::new(fields.into_iter().map(|(k, v)| (crate::symbol::Symbol::intern(k), v)).collect()))
    };
    for (flight, second, needle) in [
        (array(vec![sv("select 1"), Value::Int(5)]), None, "statement 2 is an Int, which is not a statement"),
        (array(vec![record(vec![("params", array(vec![]))])]), None, "statement 1 is a record with no `sql`"),
        (array(vec![record(vec![("sql", Value::Int(1))])]), None, "statement 1's `sql` is an Int, not a String"),
        (
            array(vec![sv("select 1"), statement("select $1", vec![record(vec![])])]),
            None,
            "statement 2: parameter 1 is a Record, which has no SQL form",
        ),
        (array(vec![sv("select 1")]), Some(array(vec![Value::Int(1)])), "each carries its own parameters"),
    ] {
        let mut args = vec![flight];
        args.extend(second);
        let e = conn_method(&c, "query", &args, 1, 1).unwrap_err();
        assert!(e.message.contains(needle), "{needle}: {}", e.message);
    }
    // A record may carry more than a statement needs; and none of the above reached the wire.
    let rendered = record(vec![("sql", sv("select 1 as n")), ("params", array(vec![])), ("n", Value::Int(1))]);
    conn_method(&c, "query", &[array(vec![rendered])], 1, 1).unwrap();
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!((seen.syncs, seen.executed.len()), (1, 1));
}

/// A STATEMENT THE FLIGHT BINDS IS NEVER THE ONE DISPLACED TO MAKE ROOM — not even when it is
/// the least recently used, and not even when it comes AFTER the new statement in the flight.
#[test]
fn a_flight_never_displaces_a_statement_it_binds() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["1"]], tag: "SELECT 1" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    for i in 0..MAX_PREPARED {
        conn_method(&c, "query", &[sv(&format!("select {i} as n"))], 1, 1).unwrap();
    }
    // The cache is full and `select 0` is its oldest. A flight: a NEW statement, then that one.
    let flight = array(vec![sv("select 'new' as n"), sv("select 0 as n")]);
    conn_method(&c, "query", &[flight], 1, 1).unwrap();
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.parses.len(), MAX_PREPARED + 1);
    // Room was made by closing the oldest statement the flight does NOT use: the second.
    assert_eq!(seen.closes, vec![seen.parses[1].clone()]);
}

/// A COPY GOES ALONE. Followed by other statements it makes a real server end the connection
/// (it reads the next statement as COPY data and loses its place), and once the flight is on
/// the wire nothing can be done — so it is refused BEFORE anything is sent, wherever it stands,
/// and the connection is untouched.
#[test]
fn a_copy_in_a_flight_is_refused_before_anything_is_sent() {
    let (port, h) = serve(Script { expect_read_only: false, columns: vec!["n"], rows: vec![vec!["9"]], tag: "SELECT 1" });
    let c = write_conn(port);
    for (flight, whose) in [
        (vec!["select 1 as n", "  /* bulk */ COPY t FROM STDIN"], "statement 2 of 2: "),
        (vec!["copy t from stdin", "select 1 as n"], "statement 1 of 2: "),
    ] {
        let e = conn_method(&c, "execute", &[array(flight.iter().map(|q| sv(q)).collect())], 1, 1).unwrap_err();
        assert!(e.message.contains(whose), "{}", e.message);
        assert!(e.message.contains("cannot share a round trip"), "{}", e.message);
    }
    let v = conn_method(&c, "query", &[sv("select 9 as n")], 1, 1).unwrap();
    assert_eq!(ints_of(&v, "n"), vec![9]);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!((seen.syncs, seen.executed.len()), (1, 1), "only the query after them reached the server");
}

/// EVERY NAME STALE AT ONCE — `DEALLOCATE ALL`, `DISCARD ALL`, a pooler's other backend — found
/// against a live server, where a flight that forgot only the statement the server happened to
/// refuse was refused for the next one. The flight forgets every statement it binds, closes
/// them (one that was still there must not be left behind), and goes again ONCE, all fresh.
#[test]
fn a_flight_whose_names_all_went_stale_goes_again_once() {
    let script = Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["1"]], tag: "SELECT 1" };
    // Binds 1–3 are the first flight; the server forgets everything before the fourth.
    let (port, h) = serve_forgetting(script, Some(4));
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    let flight = array(vec![sv("select 1 as n"), sv("select 2 as n"), sv("select 3 as n")]);
    for _ in 0..3 {
        let Value::Array(answers) = conn_method(&c, "query", std::slice::from_ref(&flight), 1, 1).unwrap() else { panic!("an Array") };
        assert_eq!(answers.len(), 3);
    }
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!(seen.syncs, 4, "three flights, and one of them sent twice");
    assert_eq!(seen.parses.len(), 6, "each text parsed twice: {:?}", seen.parses);
    // The three stale names were closed at the head of the second attempt, none twice.
    assert_eq!(seen.closes, seen.parses[..3].to_vec());
}

/// A flight of ONE is that statement — the same bytes, the same cache, no second code path.
#[test]
fn a_flight_of_one_is_that_statement() {
    let (port, h) = serve(Script { expect_read_only: true, columns: vec!["n"], rows: vec![vec!["5"]], tag: "SELECT 1" });
    let Value::Db(c) = postgres_open(&[sv(&url(port))], 1, 1).unwrap() else { panic!("not a connection") };
    conn_method(&c, "query", &[sv("select 5 as n")], 1, 1).unwrap();
    let Value::Array(answers) = conn_method(&c, "query", &[array(vec![sv("select 5 as n")])], 1, 1).unwrap() else { panic!("an Array") };
    let answers: Vec<Value> = answers.iter_values().collect();
    assert_eq!(ints_of(&answers[0], "n"), vec![5]);
    drop(c);
    let seen = h.join().unwrap();
    assert_eq!((seen.parses.len(), seen.describes, seen.syncs), (1, 1, 2), "a hit on the statement already prepared");
}

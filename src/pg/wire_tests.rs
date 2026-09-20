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
}

/// What a server can do beyond answering its script.
#[derive(Default)]
struct Twists {
    /// FORGET every prepared statement before this Bind (1-based) — what `DEALLOCATE ALL`,
    /// `DISCARD ALL` or a pooler's other backend does to a name.
    forget_at: Option<usize>,
    /// From the second Execute on, answer these rows instead of the script's.
    later_rows: Option<Vec<Vec<&'static str>>>,
    /// On this Execute (1-based), send one row and HANG UP — no completion, no ReadyForQuery.
    hang_up_at: Option<usize>,
    /// Answer the FIRST Execute with CopyInResponse, as `COPY … FROM STDIN` does, and wait.
    copy_in_first: bool,
    /// Fail this Execute (1-based) with a unique violation, as a statement inside a
    /// transaction might.
    fail_at: Option<usize>,
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
        // The formats the last Bind asked for: none means every column as text.
        let mut formats: Vec<i16> = Vec::new();
        // After an error the server discards messages until `Sync`, as the protocol says.
        let mut skipping = false;
        // Waiting for COPY data: a Sync that arrives now was sent before the client could know,
        // and is ignored — as the protocol says.
        let mut copying = false;
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
                    texts.insert(name.clone(), sql.clone());
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
                        bound = texts.get(&stmt).cloned().unwrap_or_default();
                        formats = result_formats(&body);
                        binary_columns.push(formats.iter().filter(|f| **f == 1).count());
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
                b'E' if twists.copy_in_first && executes == 0 => {
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
                    if status == b'T' {
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
                b'E' if bound.starts_with("begin") || bound == "commit" || bound == "rollback" => {
                    executes += 1;
                    executed.push(bound.clone());
                    status = if bound.starts_with("begin") { b'T' } else { b'I' };
                    let mut out = Vec::new();
                    put_cstr(&mut out, &bound.to_uppercase());
                    send(&mut s, b'C', &out);
                }
                b'E' => {
                    executes += 1;
                    executed.push(bound.clone());
                    let rows = match &twists.later_rows {
                        Some(later) if executes > 1 => later,
                        _ => &script.rows,
                    };
                    for r in rows {
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
                            return Seen { sql, parses, closes, describes, binary_columns, executed };
                        }
                    }
                    let mut out = Vec::new();
                    put_cstr(&mut out, script.tag);
                    send(&mut s, b'C', &out);
                }
                b'S' => {
                    skipping = false;
                    send(&mut s, b'Z', &[status])
                }
                b'X' => break,
                other => panic!("unexpected message {:?}", other as char),
            }
        }
        Seen { sql, parses, closes, describes, binary_columns, executed }
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
    let (port, h) = serve_with(script, Twists { copy_in_first: true, ..Twists::default() });
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
    assert!(matches!(conn_method(&tx, "rollback", &[], 1, 1).unwrap(), Value::Missing));
    drop(tx);
    drop(c);
    assert_eq!(
        h.join().unwrap().executed,
        ["begin isolation level serializable", "insert into t values (1)", "rollback", "insert into t values (2)", "begin", "rollback"]
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

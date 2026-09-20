//! One statement, one round trip: what is sent, what is kept between calls, and how the answer
//! is read.
//!
//! A [`Session`] is a connection past its handshake. It keeps three things a query would
//! otherwise pay for every time: the statements it has PREPARED (their server-side names, so
//! the server parses a text once), what each one RETURNS (its columns, so the server is not
//! asked to describe them again and the fixed-width ones can cross in binary), and the BUFFER
//! an exchange is framed in (so sending a query allocates nothing).

use std::collections::HashMap;
use std::io::Write as _;
use std::rc::Rc;

use super::proto::{begin_msg, end_msg, error_code, error_text, put_cstr, send_framed, Cur};
use super::stream::Stream;
use super::types::{binary_width, ColBuf};
use crate::value::Value;

/// A connection past its handshake, and what it remembers between statements.
pub struct Session {
    pub stream: Stream,
    prepared: Prepared,
    /// One exchange's frontend messages, framed where they are sent from.
    wire: Vec<u8>,
    /// Whether this server prints `float8` with round-trip digits (PostgreSQL 12 and later),
    /// which is what makes a binary `float8` the same value as its text (`types`).
    exact_float_text: bool,
    /// Where the server says the session stands, from its last `ReadyForQuery`: `I` idle, `T`
    /// in a transaction, `E` in a transaction that has failed and will answer nothing but its
    /// end.
    pub status: u8,
}

/// A framing buffer is kept between exchanges up to this size and let go past it, so one
/// 50 MB parameter does not stay allocated for the life of the connection.
const KEEP_WIRE: usize = 1024 * 1024;

impl Session {
    pub fn new(stream: Stream, exact_float_text: bool) -> Session {
        Session { stream, prepared: Prepared::default(), wire: Vec::new(), exact_float_text, status: b'I' }
    }
}

/// What a statement produced: its result columns (none for a statement without a
/// `RETURNING`), and the rows it affected — read from the server's completion tag.
pub struct Outcome {
    pub cols: Vec<ColBuf>,
    pub affected: i64,
}

/// What a statement returns: each column's name and type OID. Learned from the server's
/// `RowDescription` the first time a statement runs, and kept with its name.
pub struct RowDesc {
    cols: Vec<(String, i32)>,
}

impl RowDesc {
    /// The buffers a result is read into — each column in the format its Bind asked for.
    fn bufs(&self, exact_float_text: bool) -> Vec<ColBuf> {
        self.cols
            .iter()
            .map(|(name, oid)| ColBuf::with_format(name.clone(), *oid, binary_width(*oid, exact_float_text)))
            .collect()
    }
}

/// How many statements a connection keeps prepared. Past it the least recently used one is
/// closed on the server as the new one is parsed, in the same round trip.
pub const MAX_PREPARED: usize = 256;

/// One prepared statement: the number its server-side name is made from, when it was last
/// used, and — once it has run — what it returns.
struct Entry {
    id: u64,
    used: u64,
    desc: Option<Rc<RowDesc>>,
}

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
///
/// WHAT IT RETURNS IS KEPT WITH IT. The first run asks the server to describe the result, as
/// every run used to; the answer is remembered, so later runs neither ask again nor wait for
/// it, and — knowing each column's type before Bind is sent — they ask for the fixed-width
/// ones in binary. That memory cannot go stale unnoticed: a named statement's result type is
/// FIXED on the server (`fixed_result`), which compares names, types, typmods and collations
/// when it replans and answers `0A000` rather than return a different shape — the same code
/// that already sends this cache back to Parse.
#[derive(Default)]
struct Prepared {
    by_sql: HashMap<String, Entry>,
    tick: u64,
    next: u64,
}

impl Prepared {
    /// The statement `sql` is prepared as, marked as just used.
    fn touch(&mut self, sql: &str) -> Option<(u64, Option<Rc<RowDesc>>)> {
        self.tick += 1;
        let tick = self.tick;
        self.by_sql.get_mut(sql).map(|e| {
            e.used = tick;
            (e.id, e.desc.clone())
        })
    }

    /// A fresh statement number — and, when the cache is full, the least recently used
    /// statement's, which leaves the cache here and the server in the round trip that follows.
    fn reserve(&mut self) -> (u64, Option<u64>) {
        self.next += 1;
        if self.by_sql.len() < MAX_PREPARED {
            return (self.next, None);
        }
        let oldest = self.by_sql.iter().min_by_key(|(_, e)| e.used).map(|(sql, _)| sql.clone());
        let evicted = oldest.and_then(|sql| self.by_sql.remove(&sql)).map(|e| e.id);
        (self.next, evicted)
    }

    fn insert(&mut self, sql: &str, id: u64, desc: Option<Rc<RowDesc>>) {
        self.tick += 1;
        self.by_sql.insert(sql.to_string(), Entry { id, used: self.tick, desc });
    }

    /// A statement that was prepared before it ever ran (its first Bind was refused) has now
    /// run, and said what it returns.
    fn describe(&mut self, sql: &str, desc: Rc<RowDesc>) {
        if let Some(e) = self.by_sql.get_mut(sql) {
            e.desc = Some(desc);
        }
    }
}

/// Why an exchange failed: the text for a reader, the SQLSTATE for the cache, whether the
/// statement was parsed before it did — a statement whose Bind was refused (a parameter of the
/// wrong shape) exists on the server all the same — and whether the CONNECTION survived.
///
/// `broken` is the difference between an error and a wrong answer. An exchange that ends at
/// `ReadyForQuery` leaves the connection where the next one expects it, whatever the server
/// said on the way. One that ends anywhere else — a timeout, a closed socket, bytes that are
/// not the protocol — leaves replies unread, and the next statement on that connection would
/// read THEM as its own. Such a connection is never used again (`Conn::run` closes it).
pub struct Fail {
    pub text: String,
    code: String,
    parsed: bool,
    pub broken: bool,
}

impl Fail {
    /// Refused before a byte was sent: the connection is exactly where it was.
    fn unsent(text: String) -> Fail {
        Fail { text, code: String::new(), parsed: false, broken: false }
    }
}

/// Anything that goes wrong between the send and `ReadyForQuery` — a read that fails, a
/// message that does not parse — leaves the connection in an unknown state.
impl From<String> for Fail {
    fn from(text: String) -> Self {
        Fail { text, code: String::new(), parsed: false, broken: true }
    }
}

/// Which statement an exchange binds: the unnamed one, or one this connection prepared.
#[derive(Clone, Copy)]
enum Name {
    Unnamed,
    Cached(u64),
}

/// A statement's server-side name, as the NUL-terminated string the protocol takes.
fn put_name(out: &mut Vec<u8>, name: Name) {
    if let Name::Cached(id) = name {
        // Writing to a `Vec` cannot fail.
        let _ = write!(out, "_helix_{id}");
    }
    out.push(0);
}

/// Run one statement on a connection that keeps what it prepares.
pub fn run_prepared(se: &mut Session, sql: &str, params: &[Value]) -> Result<Outcome, Fail> {
    // A HIT: Bind and Execute against the name — no Parse, and once it has run, no Describe.
    if let Some((id, desc)) = se.prepared.touch(sql) {
        match exchange(se, Name::Cached(id), None, params, desc.as_ref()) {
            // The server no longer has it, or its result type changed under it: forget the
            // name and prepare the text again below — once.
            Err(f) if !f.broken && (f.code == "26000" || f.code == "0A000") => {
                se.prepared.by_sql.remove(sql);
                // UNLESS THAT ERROR JUST ENDED A TRANSACTION. Inside one, any error is the
                // end of it: preparing again would only be answered `25P02`, which says
                // nothing about why. The cause is reported instead, with what to do.
                if se.status == b'E' {
                    let text = format!(
                        "{} — the statement was prepared before a change that made it stale, and an error inside a transaction ends the transaction: roll back and run it again",
                        f.text
                    );
                    return Err(Fail { text, ..f });
                }
            }
            Ok((out, learned)) => {
                if let Some(d) = learned {
                    se.prepared.describe(sql, d);
                }
                return Ok(out);
            }
            Err(f) => return Err(f),
        }
    }
    // A MISS: Parse under a fresh name, closing the statement it displaces.
    let (id, evicted) = se.prepared.reserve();
    match exchange(se, Name::Cached(id), Some((sql, evicted)), params, None) {
        Ok((out, learned)) => {
            se.prepared.insert(sql, id, learned);
            Ok(out)
        }
        // The name is a user's own prepared statement: the unnamed one always works.
        Err(f) if !f.broken && f.code == "42P05" => {
            exchange(se, Name::Unnamed, Some((sql, None)), params, None).map(|(out, _)| out)
        }
        Err(f) => {
            if f.parsed && !f.broken {
                se.prepared.insert(sql, id, None);
            }
            Err(f)
        }
    }
}

/// Run one parameterised statement as the UNNAMED statement: its rows, and what it affected.
/// What a connection opened for one query uses.
pub fn run_statement(se: &mut Session, sql: &str, params: &[Value]) -> Result<Outcome, Fail> {
    exchange(se, Name::Unnamed, Some((sql, None)), params, None).map(|(out, _)| out)
}

/// A parameter, rendered as the text the server will parse, straight into the Bind message.
///
/// Types are left UNSPECIFIED (OID 0) so the server infers each from its use in the
/// statement, which is what `libpq` does for untyped parameters and what makes
/// `where age > $1` work without the caller declaring `int4`. `mod.rs` has already refused
/// every value with no SQL form, with the line it was written on; this is total all the same.
fn put_param(out: &mut Vec<u8>, v: &Value) -> Result<(), String> {
    if matches!(v, Value::Missing) {
        out.extend_from_slice(&(-1i32).to_be_bytes());
        return Ok(());
    }
    let at = out.len();
    out.extend_from_slice(&[0u8; 4]);
    match v {
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) => out.extend_from_slice(crate::value::fmt_float(*f).as_bytes()),
        Value::Bool(b) => out.push(if *b { b't' } else { b'f' }),
        Value::Str(s) => out.extend_from_slice(s.as_bytes()),
        other => return Err(format!("{} has no SQL form", crate::value::with_article(other.type_name()))),
    }
    let len = i32::try_from(out.len() - at - 4).map_err(|_| "parameter too large".to_string())?;
    if let Some(slot) = out.get_mut(at..at + 4) {
        slot.copy_from_slice(&len.to_be_bytes());
    }
    Ok(())
}

/// Frame one exchange into `wire`: optionally Close a displaced statement and Parse `sql` under
/// `name`, then Bind, Describe (only when what the statement returns is not yet `known`),
/// Execute and Sync.
fn frame(
    wire: &mut Vec<u8>,
    name: Name,
    parse: Option<(&str, Option<u64>)>,
    params: &[Value],
    known: Option<&Rc<RowDesc>>,
    exact_float_text: bool,
) -> Result<(), String> {
    wire.clear();
    if let Some((sql, evicted)) = parse {
        // Closing a name the server does not have is not an error, so this needs no answer
        // of its own; `CloseComplete` is skipped with the other messages nobody waits for.
        if let Some(old) = evicted {
            let at = begin_msg(wire, b'C');
            wire.push(b'S');
            put_name(wire, Name::Cached(old));
            end_msg(wire, at)?;
        }
        // Parse: no declared parameter types (the server infers them from the text).
        let at = begin_msg(wire, b'P');
        put_name(wire, name);
        put_cstr(wire, sql);
        wire.extend_from_slice(&0i16.to_be_bytes());
        end_msg(wire, at)?;
    }

    // Bind: parameters as text; results as text, except the fixed-width columns of a
    // statement whose columns are known.
    let at = begin_msg(wire, b'B');
    wire.push(0); // the unnamed portal
    put_name(wire, name);
    wire.extend_from_slice(&0i16.to_be_bytes()); // parameter formats: all text
    let n = i16::try_from(params.len()).map_err(|_| "too many parameters".to_string())?;
    wire.extend_from_slice(&n.to_be_bytes());
    for p in params {
        put_param(wire, p)?;
    }
    let binary = |d: &Rc<RowDesc>| {
        d.cols.iter().map(move |(_, oid)| binary_width(*oid, exact_float_text).is_some()).collect::<Vec<bool>>()
    };
    match known.map(binary) {
        // One code per column — the only form that lets two columns differ.
        Some(codes) if codes.contains(&true) => {
            let n = i16::try_from(codes.len()).map_err(|_| "too many result columns".to_string())?;
            wire.extend_from_slice(&n.to_be_bytes());
            for b in codes {
                wire.extend_from_slice(&i16::from(b).to_be_bytes());
            }
        }
        _ => wire.extend_from_slice(&0i16.to_be_bytes()), // result formats: all text
    }
    end_msg(wire, at)?;

    // Describe the portal, so the column names and type OIDs arrive even for zero rows —
    // the first time. After that they are known, and the server is spared the asking.
    if known.is_none() {
        let at = begin_msg(wire, b'D');
        wire.push(b'P');
        wire.push(0);
        end_msg(wire, at)?;
    }

    let at = begin_msg(wire, b'E');
    wire.push(0); // the unnamed portal
    wire.extend_from_slice(&0i32.to_be_bytes()); // unlimited rows
    end_msg(wire, at)?;

    let at = begin_msg(wire, b'S');
    end_msg(wire, at)
}

/// Read to `ReadyForQuery` after an error, so the connection is left where the next statement
/// expects it, and answer where the server says the session now stands. Whatever else arrives
/// on the way is not this caller's.
fn drain_to_ready(s: &mut Stream) -> Result<u8, String> {
    loop {
        let (tag, body) = s.next_msg()?;
        if tag == b'Z' {
            return Ok(body.first().copied().unwrap_or(b'I'));
        }
    }
}

/// One round trip. Answers what the statement produced and — when the server was asked to
/// describe the result — what it returns, for the caller to keep.
fn exchange(
    se: &mut Session,
    name: Name,
    parse: Option<(&str, Option<u64>)>,
    params: &[Value],
    known: Option<&Rc<RowDesc>>,
) -> Result<(Outcome, Option<Rc<RowDesc>>), Fail> {
    // The whole exchange, framed, to go out in ONE write. Nothing has been sent until it is
    // whole, so a parameter that cannot be framed leaves the connection untouched.
    frame(&mut se.wire, name, parse, params, known, se.exact_float_text).map_err(Fail::unsent)?;
    let sent = send_framed(&mut se.stream, &se.wire);
    if se.wire.capacity() > KEEP_WIRE {
        se.wire = Vec::new();
    }
    sent?;

    let mut cols: Vec<ColBuf> = known.map(|d| d.bufs(se.exact_float_text)).unwrap_or_default();
    let mut described = known.is_some();
    let mut learned: Option<Rc<RowDesc>> = None;
    let mut parsed = false;
    let mut affected = 0i64;
    // The first cell that was not its column's type. The rows after it are still READ — the
    // connection has to reach `ReadyForQuery` — and the error is reported once it has.
    let mut bad_cell: Option<String> = None;
    loop {
        let (tag, body) = se.stream.next_msg()?;
        match tag {
            b'E' => {
                let (text, code) = (error_text(body), error_code(body));
                // Drain to the synchronisation point so the connection is left in a known
                // state even though this query is finished. If that fails it is not.
                let drained = drain_to_ready(&mut se.stream);
                if let Ok(status) = drained {
                    se.status = status;
                }
                return Err(Fail { text, code, parsed, broken: drained.is_err() });
            }
            // ParseComplete: the statement exists on the server from here on.
            b'1' => parsed = true,
            // RowDescription
            b'T' => {
                let mut c = Cur::new(body);
                let n = c.i16()?;
                let mut desc = Vec::new();
                for _ in 0..n {
                    let name = c.cstr()?;
                    let _table_oid = c.i32()?;
                    let _attnum = c.i16()?;
                    let oid = c.i32()?;
                    let _typlen = c.i16()?;
                    let _typmod = c.i32()?;
                    let _format = c.i16()?;
                    desc.push((name, oid));
                }
                // Asked for without format codes, so every column of THIS run is text.
                cols = desc.iter().map(|(name, oid)| ColBuf::new(name.clone(), *oid)).collect();
                learned = Some(Rc::new(RowDesc { cols: desc }));
                described = true;
            }
            // NoData: a statement with no result columns.
            b'n' => {
                learned = Some(Rc::new(RowDesc { cols: Vec::new() }));
                described = true;
            }
            // DataRow
            b'D' => {
                if bad_cell.is_some() {
                    continue;
                }
                let mut c = Cur::new(body);
                let n = usize::try_from(c.i16()?).map_err(|_| "negative column count".to_string())?;
                if n != cols.len() {
                    return Err(Fail::from(format!(
                        "the server sent a row of {n} values for {} columns",
                        cols.len()
                    )));
                }
                for col in cols.iter_mut() {
                    let v = c.field()?;
                    if let Err(e) = col.push(v) {
                        bad_cell = Some(e);
                        break;
                    }
                }
            }
            // CommandComplete: what the statement did, and to how many rows.
            b'C' => affected = rows_affected(Cur::new(body).cstr_ref()?),
            // CopyInResponse: the statement is a `COPY … FROM STDIN`, and the server now waits
            // for rows this connection has no way to be handed. Left alone, that wait is the
            // whole read timeout and then a closed connection; refused, it is an ordinary
            // error. Copy-in mode swallowed the Sync that went out with the statement, so the
            // refusal brings its own.
            b'G' => {
                se.wire.clear();
                let at = begin_msg(&mut se.wire, b'f');
                put_cstr(&mut se.wire, "a Helix connection does not stream COPY data — load rows with `insert`");
                end_msg(&mut se.wire, at)?;
                let at = begin_msg(&mut se.wire, b'S');
                end_msg(&mut se.wire, at)?;
                send_framed(&mut se.stream, &se.wire)?;
            }
            // ReadyForQuery — the synchronisation point, and where the session stands.
            b'Z' => {
                se.status = body.first().copied().unwrap_or(b'I');
                break;
            }
            _ => continue,
        }
    }
    // Both of these were read to `ReadyForQuery`: the connection is sound.
    if !described {
        let text = "the server never described the result".to_string();
        return Err(Fail { text, code: String::new(), parsed, broken: false });
    }
    if let Some(text) = bad_cell {
        return Err(Fail { text, code: String::new(), parsed, broken: false });
    }
    Ok((Outcome { cols, affected }, learned))
}

/// Rows affected, read from the completion tag. The tag is the command word, an OID for a
/// one-row INSERT (always 0 since PostgreSQL 12), and the count — so the count is the LAST
/// word for every command that reports one, and a command that reports none (`CREATE
/// TABLE`, `BEGIN`) affected no rows.
fn rows_affected(tag: &str) -> i64 {
    let mut words = tag.split_ascii_whitespace();
    let Some(cmd) = words.next() else { return 0 };
    if !matches!(cmd, "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "SELECT" | "MOVE" | "FETCH" | "COPY") {
        return 0;
    }
    words.last().and_then(|w| w.parse().ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// What a query sends, byte for byte: a statement this connection has run before is a
    /// Bind, an Execute and a Sync — no Parse, no Describe — asking for its fixed-width
    /// columns in binary; one it has not is Parse, Bind (all text), Describe, Execute, Sync.
    #[test]
    fn a_known_statement_is_bound_and_executed_and_nothing_else() {
        let tags = |wire: &[u8]| {
            let (mut at, mut out) = (0usize, String::new());
            while let Some(head) = wire.get(at..at + 5) {
                out.push(head[0] as char);
                at += 1 + i32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
            }
            assert_eq!(at, wire.len(), "the framing covers the buffer exactly");
            out
        };
        let mut wire = Vec::new();
        frame(&mut wire, Name::Cached(7), Some(("select 1", Some(3))), &[Value::Int(5)], None, true).unwrap();
        assert_eq!(tags(&wire), "CPBDES");
        assert!(wire.windows(9).any(|w| w == b"_helix_7\0") && wire.windows(9).any(|w| w == b"_helix_3\0"));

        let desc = Rc::new(RowDesc { cols: vec![("id".into(), 20), ("name".into(), 25), ("score".into(), 701)] });
        frame(&mut wire, Name::Cached(7), None, &[Value::Int(5), Value::Missing], Some(&desc), true).unwrap();
        assert_eq!(tags(&wire), "BES");
        // Bind's tail: three result formats — binary, text, binary.
        let bind_len = 1 + i32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]) as usize;
        assert_eq!(&wire[bind_len - 8..bind_len], &[0, 3, 0, 1, 0, 0, 0, 1]);
        // And its parameters: `5` as text, then NULL.
        assert!(wire[..bind_len].windows(9).any(|w| w == [0, 0, 0, 1, b'5', 0xff, 0xff, 0xff, 0xff]));

        // Before PostgreSQL 12 a float's text is rounded, so it stays text; and a statement
        // with nothing fixed-width asks for nothing.
        frame(&mut wire, Name::Cached(7), None, &[], Some(&desc), false).unwrap();
        let bind_len = 1 + i32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]) as usize;
        assert_eq!(&wire[bind_len - 8..bind_len], &[0, 3, 0, 1, 0, 0, 0, 0]);
        let text_only = Rc::new(RowDesc { cols: vec![("name".into(), 25)] });
        frame(&mut wire, Name::Unnamed, None, &[], Some(&text_only), true).unwrap();
        let bind_len = 1 + i32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]) as usize;
        assert_eq!(&wire[bind_len - 2..bind_len], &[0, 0]);
    }
}

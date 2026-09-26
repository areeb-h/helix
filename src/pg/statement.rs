//! One statement, one round trip: what is sent, what is kept between calls, and how the answer
//! is read.
//!
//! A [`Session`] is a connection past its handshake. It keeps three things a query would
//! otherwise pay for every time: the statements it has PREPARED (their server-side names, so
//! the server parses a text once), what each one RETURNS (its columns, so the server is not
//! asked to describe them again and the fixed-width ones can cross in binary), and the BUFFER
//! an exchange is framed in (so sending a query allocates nothing).
//!
//! AND SEVERAL STATEMENTS CAN SHARE ONE ROUND TRIP ([`run_flight`]). A small query is 7–13 µs
//! of this client's code inside a 150–190 µs round trip, so nothing done to a single statement
//! makes a page of five queries faster — sending the five together does. Each is framed as it
//! would be alone, and ONE Sync ends them all, which also makes the flight one transaction:
//! every statement takes effect or none does.
//!
//! AND A RESULT CAN BE READ A PAGE AT A TIME (`super::cursor`): bound to a NAMED portal and
//! Executed with a row limit, a statement answers that many rows and `PortalSuspended`, and
//! [`run_page`] asks the same portal for the next page — one round trip each, the same plan
//! and the same formats the whole read would have had.

use std::collections::HashMap;
use std::io::Write as _;
use std::rc::Rc;

use super::proto::{begin_msg, end_msg, error_code, error_text, put_cstr, send_framed, Cur};
use super::stream::Stream;
use super::types::{binary_width, ColBuf};
use crate::value::{ArrayData, Value};

/// A connection past its handshake, and what it remembers between statements.
pub struct Session {
    pub stream: Stream,
    prepared: Prepared,
    /// One exchange's frontend messages, framed where they are sent from.
    wire: Vec<u8>,
    /// Whether this server prints `float8` with round-trip digits (PostgreSQL 12 and later),
    /// which is what makes a binary `float8` the same value as its text (`types`).
    exact_float_text: bool,
    /// Where the server says the session stands, from its last `ReadyForQuery` (`saw_ready`):
    /// `I` idle, `T` in a transaction, `E` in a transaction that has failed and will answer
    /// nothing but its end.
    pub status: u8,
    /// How long the server was asked to let one statement run on this session (`timeout=N`);
    /// `None` when it was not asked.
    pub limit: Option<std::time::Duration>,
    /// How many named portals this session has opened: what the next one's name is made from.
    pub portals: u64,
    /// What this connection has let go of and the server still holds, closed at the head of the
    /// NEXT exchange — whatever it is — at no round trip of its own (`Owed`).
    owed: Vec<Owed>,
}

/// Something the server holds that this connection has let go of (`Session::owe`).
#[derive(Clone, Copy)]
pub enum Owed {
    /// A prepared statement forgotten because its name went stale where it could not be
    /// prepared again — inside a transaction an error is the end of it. A name forgotten here
    /// and left there would sit on the server for the life of the connection.
    Statement(u64),
    /// A cursor's portal, let go of inside a transaction that carries on: read to its end (the
    /// server holds a finished portal until it is closed or its transaction ends) or dropped
    /// before.
    Portal(u64),
}

/// A framing buffer is kept between exchanges up to this size and let go past it, so one
/// 50 MB parameter does not stay allocated for the life of the connection.
const KEEP_WIRE: usize = 1024 * 1024;

impl Session {
    pub fn new(stream: Stream, exact_float_text: bool) -> Session {
        Session {
            stream,
            prepared: Prepared::default(),
            wire: Vec::new(),
            exact_float_text,
            status: b'I',
            limit: None,
            portals: 0,
            owed: Vec::new(),
        }
    }

    /// Whether this server prints `float8` with round-trip digits — what decides if a binary
    /// `float8` is the same value as its text.
    pub fn exact_float_text(&self) -> bool {
        self.exact_float_text
    }

    /// A cursor's portal was bound from statement `id`: keep that statement from being displaced
    /// while the portal lives (`super::cursor` says why).
    pub fn pin(&mut self, id: u64) {
        self.prepared.pinned.push(id);
    }

    /// The portal bound from `id` has been let go of: one pin fewer.
    pub fn unpin(&mut self, id: u64) {
        if let Some(at) = self.prepared.pinned.iter().position(|p| *p == id) {
            self.prepared.pinned.swap_remove(at);
        }
    }

    /// Close `what` with the next exchange.
    pub fn owe(&mut self, what: Owed) {
        self.owed.push(what);
    }

    /// `ReadyForQuery`: where the server says the session now stands. OUT OF A TRANSACTION NO
    /// PORTAL SURVIVES — so no statement needs a pin any longer, and no portal a Close.
    fn saw_ready(&mut self, status: u8) {
        self.status = status;
        if status == b'I' {
            self.prepared.pinned.clear();
            self.owed.retain(|what| matches!(what, Owed::Statement(_)));
        }
    }
}

/// What a statement produced: its result columns (none for a statement without a
/// `RETURNING`), and the rows it affected — read from the server's completion tag.
pub struct Outcome {
    pub cols: Vec<ColBuf>,
    pub affected: i64,
    /// The portal was SUSPENDED at its row limit — a cursor's page — rather than run to its
    /// end.
    pub suspended: bool,
    /// What the statement returns, when known: remembered from an earlier run, or learned
    /// from this one. A cursor reads its later pages into buffers of this shape.
    pub desc: Option<Rc<RowDesc>>,
    /// Bind asked for the fixed-width columns in binary — it could, because what the statement
    /// returns was known before it went.
    pub binary: bool,
    /// The prepared statement the Bind named — `None` for the unnamed one. A cursor pins it.
    pub bound: Option<u64>,
}

/// What a statement returns: each column's name and type OID. Learned from the server's
/// `RowDescription` the first time a statement runs, and kept with its name.
pub struct RowDesc {
    cols: Vec<(String, i32)>,
}

impl RowDesc {
    /// The buffers a result is read into — each column in the format its Bind asked for: the
    /// fixed-width ones in binary when `binary`, everything else as text.
    pub fn bufs(&self, binary: bool, exact_float_text: bool) -> Vec<ColBuf> {
        self.cols
            .iter()
            .map(|(name, oid)| {
                let width = binary_width(*oid, exact_float_text).filter(|_| binary);
                ColBuf::with_format(name.clone(), *oid, width)
            })
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
    /// Statements an open cursor's portal was bound from, once per cursor (`Session::pin`):
    /// never the one displaced to make room while the portal lives.
    pinned: Vec<u64>,
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
    /// statement's, to be closed on the server in the round trip that follows. `spoken_for` are
    /// statements the same flight binds or has already chosen to displace, and a statement an
    /// open cursor's portal was bound from is never displaced either (`pinned`); with every one
    /// of them spoken for there is nobody to displace, and the caller uses the unnamed statement.
    ///
    /// CHOOSING IS NOT YET FORGETTING. The victim stays in the cache until the `Close` that
    /// names it has been on the wire ([`Prepared::forget`]) — a statement forgotten here and
    /// never closed there would sit on the server for the life of the connection, and one
    /// closed there but remembered here is a `26000` waiting to happen.
    fn reserve(&mut self, spoken_for: &[u64]) -> Option<(u64, Option<u64>)> {
        let room = self.by_sql.len() + spoken_for.len().saturating_sub(self.hits_among(spoken_for)) < MAX_PREPARED;
        if room {
            self.next += 1;
            return Some((self.next, None));
        }
        let victim = self
            .by_sql
            .values()
            .filter(|e| !spoken_for.contains(&e.id) && !self.pinned.contains(&e.id))
            .min_by_key(|e| e.used)
            .map(|e| e.id)?;
        self.next += 1;
        Some((self.next, Some(victim)))
    }

    /// How many of `ids` are statements this cache holds (the rest are new in the same flight).
    fn hits_among(&self, ids: &[u64]) -> usize {
        self.by_sql.values().filter(|e| ids.contains(&e.id)).count()
    }

    /// The server has been told to close this one.
    fn forget(&mut self, id: u64) {
        self.by_sql.retain(|_, e| e.id != id);
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
    /// Refused before a byte was sent: the server has seen none of it.
    unsent: bool,
}

impl Fail {
    /// The server CANCELLED the statement (`57014`) — its own timeout, or someone's request.
    pub fn cancelled(&self) -> bool {
        self.code == "57014"
    }

    /// The statement's name went STALE under it: the server no longer has it (`26000`), or
    /// its result type changed (`0A000`). Prepared again, once.
    pub fn stale(&self) -> bool {
        self.code == "26000" || self.code == "0A000"
    }

    /// The name is TAKEN by a user's own prepared statement (`42P05`): the unnamed one works.
    pub fn taken(&self) -> bool {
        self.code == "42P05"
    }

    /// The exchange reached `ReadyForQuery`, and what came back was not usable: the connection
    /// is where the next statement expects it, and this says what was wrong.
    pub fn answered(text: String) -> Fail {
        Fail { text, code: String::new(), parsed: false, broken: false, unsent: false }
    }

    /// Refused before a byte was sent: the connection is exactly where it was.
    fn unsent(text: String) -> Fail {
        Fail { text, code: String::new(), parsed: false, broken: false, unsent: true }
    }
}

/// Anything that goes wrong between the send and `ReadyForQuery` — a read that fails, a
/// message that does not parse — leaves the connection in an unknown state.
impl From<String> for Fail {
    fn from(text: String) -> Self {
        Fail { text, code: String::new(), parsed: false, broken: true, unsent: false }
    }
}

/// Which statement an exchange binds: the unnamed one, or one this connection prepared.
#[derive(Clone, Copy)]
enum Name {
    Unnamed,
    Cached(u64),
}

impl Name {
    /// The prepared statement's number, when it is one this connection keeps.
    fn cached(self) -> Option<u64> {
        match self {
            Name::Cached(id) => Some(id),
            Name::Unnamed => None,
        }
    }
}

/// A statement's server-side name, as the NUL-terminated string the protocol takes.
fn put_name(out: &mut Vec<u8>, name: Name) {
    if let Name::Cached(id) = name {
        // Writing to a `Vec` cannot fail.
        let _ = write!(out, "_helix_{id}");
    }
    out.push(0);
}

/// Which portal an Execute runs, and how many rows it asks for: the unnamed portal and all of
/// them for a statement; a named one and a page of them for a cursor (`super::cursor`), which
/// comes back for the next page while the server says the portal was suspended.
#[derive(Clone, Copy)]
pub struct Fetch {
    pub portal: Portal,
    /// Rows an Execute asks for; 0 is all of them.
    pub limit: u32,
}

impl Fetch {
    /// The whole result through the unnamed portal: what a statement is.
    pub const ALL: Fetch = Fetch { portal: Portal::Unnamed, limit: 0 };
}

/// The unnamed portal, or one this connection named for a cursor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Portal {
    Unnamed,
    Named(u64),
}

/// A portal's name, as the NUL-terminated string the protocol takes.
fn put_portal(out: &mut Vec<u8>, portal: Portal) {
    if let Portal::Named(id) = portal {
        let _ = write!(out, "_helix_cursor_{id}");
    }
    out.push(0);
}

/// Run one statement on a connection that keeps what it prepares.
pub fn run_prepared(se: &mut Session, sql: &str, params: &[Value]) -> Result<Outcome, Fail> {
    run_fetching(se, sql, params, Fetch::ALL)
}

/// The same, through the portal `fetch` names and for as many rows as it asks: how a cursor's
/// first page is read (the portal stays, suspended, for [`run_page`]).
pub fn run_fetching(se: &mut Session, sql: &str, params: &[Value], fetch: Fetch) -> Result<Outcome, Fail> {
    // A statement found stale below, to be closed by the exchange that replaces it: a name
    // whose result type changed (`0A000`) is useless and STILL THERE, and would otherwise sit
    // on the server for the life of the connection. Closing one that is gone is not an error.
    let mut stale: Option<u64> = None;
    // A HIT: Bind and Execute against the name — no Parse, and once it has run, no Describe.
    if let Some((id, desc)) = se.prepared.touch(sql) {
        match exchange(se, Framing { name: Name::Cached(id), parse: None, known: desc.as_ref(), fetch }, params) {
            // The server no longer has it, or its result type changed under it: forget the
            // name and prepare the text again below — once.
            Err(f) if !f.broken && f.stale() => {
                se.prepared.by_sql.remove(sql);
                stale = Some(id);
                // UNLESS THAT ERROR JUST ENDED A TRANSACTION. Inside one, any error is the
                // end of it: preparing again would only be answered `25P02`, which says
                // nothing about why. The cause is reported instead, with what to do — and the
                // stale name is closed by the next exchange, whatever that is.
                if se.status == b'E' {
                    se.owe(Owed::Statement(id));
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
    let Some((id, evicted)) = se.prepared.reserve(&[]) else {
        return run_statement(se, sql, params);
    };
    // One `Close` rides with the Parse: the stale statement, whose leaving made the room —
    // or, with nothing stale, whatever was displaced.
    let ran = exchange(se, Framing { name: Name::Cached(id), parse: Some((sql, stale.or(evicted))), known: None, fetch }, params);
    // The `Close` went first, so whatever became of the rest, it was read — unless nothing went.
    if let (None, Some(old)) = (stale, evicted)
        && !matches!(&ran, Err(f) if f.unsent)
    {
        se.prepared.forget(old);
    }
    match ran {
        Ok((out, learned)) => {
            se.prepared.insert(sql, id, learned);
            Ok(out)
        }
        // The name is a user's own prepared statement: the unnamed one always works.
        Err(f) if !f.broken && f.taken() => {
            exchange(se, Framing { name: Name::Unnamed, parse: Some((sql, None)), known: None, fetch }, params).map(|(out, _)| out)
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
    exchange(se, Framing { name: Name::Unnamed, parse: Some((sql, None)), known: None, fetch: Fetch::ALL }, params).map(|(out, _)| out)
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
        Value::Bytes(b) => put_bytea(out, b),
        Value::Array(a) => put_array(out, a, 1)?,
        other => return Err(format!("{} has no SQL form", crate::value::with_article(other.type_name()))),
    }
    let len = i32::try_from(out.len() - at - 4).map_err(|_| "parameter too large".to_string())?;
    if let Some(slot) = out.get_mut(at..at + 4) {
        slot.copy_from_slice(&len.to_be_bytes());
    }
    Ok(())
}

/// `bytea`, in the hex form every server since 9.0 reads: `\x` and two digits a byte.
fn put_bytea(out: &mut Vec<u8>, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.extend_from_slice(b"\\x");
    for b in bytes {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 15)]);
    }
}

/// How deep an array parameter may nest: PostgreSQL's own limit on dimensions (`MAXDIM`).
pub const MAX_ARRAY_DEPTH: usize = 6;

/// AN ARRAY, AS THE ARRAY LITERAL `= any($1)` BINDS.
///
/// `where id in ($1, $2, …)` spends a parameter per value, so its text changes with the COUNT —
/// a different prepared statement for every length of list — and it stops working at 65 535.
/// `where id = any($1)` is one statement and one parameter for any number of values, and this
/// is that parameter. The field build's ORM wrote this grammar itself, in Helix, with a fast
/// path through `to_json` held to a careful slow one; it is the driver's to get right once.
///
/// The grammar is small and has one trap. Elements are separated by commas inside braces; a
/// number or a boolean is written bare; `missing` is the bare word `NULL`; and a String is
/// ALWAYS quoted, with `\` and `"` escaped — always, because a bare element is where the
/// traps live: `NULL` would be a null, `a,b` two elements, `{` a nesting, and leading spaces
/// would vanish. A quoted element is its text and nothing else. It is data for the server's
/// array parser, never SQL, which is why a String is safe here where splicing one into the
/// statement would not be. A nested Array is a further dimension; the server holds the
/// rectangle to account.
fn put_array(out: &mut Vec<u8>, a: &ArrayData, depth: usize) -> Result<(), String> {
    if depth > MAX_ARRAY_DEPTH {
        return Err(format!("an Array nested more than {MAX_ARRAY_DEPTH} deep has no SQL form"));
    }
    out.push(b'{');
    for (i, v) in a.iter_values().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        match &v {
            Value::Missing => out.extend_from_slice(b"NULL"),
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(f) => out.extend_from_slice(crate::value::fmt_float(*f).as_bytes()),
            Value::Bool(b) => out.push(if *b { b't' } else { b'f' }),
            Value::Str(s) => put_quoted(out, s.as_bytes()),
            Value::Bytes(b) => {
                let mut hex = Vec::with_capacity(2 + 2 * b.len());
                put_bytea(&mut hex, b);
                put_quoted(out, &hex);
            }
            Value::Array(inner) => put_array(out, inner, depth + 1)?,
            other => return Err(format!("{} has no SQL form", crate::value::with_article(other.type_name()))),
        }
    }
    out.push(b'}');
    Ok(())
}

/// One array element, quoted: `\` and `"` escaped, everything else as it is.
fn put_quoted(out: &mut Vec<u8>, text: &[u8]) {
    out.push(b'"');
    for &b in text {
        if b == b'"' || b == b'\\' {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b'"');
}

/// How one statement goes out: which name it binds, whether (under what text, closing whom)
/// it is Parsed first, what it is known to return, and through which portal how many rows are
/// asked for.
#[derive(Clone, Copy)]
struct Framing<'a> {
    name: Name,
    parse: Option<(&'a str, Option<u64>)>,
    known: Option<&'a Rc<RowDesc>>,
    fetch: Fetch,
}

/// Frame one exchange into `wire`: every Close owed from earlier exchanges, one statement's
/// messages, and Sync.
fn frame(wire: &mut Vec<u8>, owed: &[Owed], how: Framing<'_>, params: &[Value], exact_float_text: bool) -> Result<(), String> {
    wire.clear();
    frame_closes(wire, owed)?;
    frame_statement(wire, how, params, exact_float_text)?;
    let at = begin_msg(wire, b'S');
    end_msg(wire, at)
}

/// Close what this connection let go of: a prepared statement (`S`) or a portal (`P`). Closing
/// a name the server does not have is not an error, so this needs no answer of its own;
/// `CloseComplete` is skipped with the other messages nobody waits for.
fn frame_close(wire: &mut Vec<u8>, what: Owed) -> Result<(), String> {
    let at = begin_msg(wire, b'C');
    match what {
        Owed::Statement(id) => {
            wire.push(b'S');
            put_name(wire, Name::Cached(id));
        }
        Owed::Portal(id) => {
            wire.push(b'P');
            put_portal(wire, Portal::Named(id));
        }
    }
    end_msg(wire, at)
}

/// Every Close owed, at the head of an exchange.
fn frame_closes(wire: &mut Vec<u8>, owed: &[Owed]) -> Result<(), String> {
    owed.iter().try_for_each(|what| frame_close(wire, *what))
}

/// Append one statement's messages to `wire`: optionally Close a displaced statement and Parse
/// `sql` under `name`, then Bind (to the portal `fetch` names), Describe (only when what the
/// statement returns is not yet `known`) and Execute (for as many rows as `fetch` asks). No
/// Sync: a flight of several ends with one.
fn frame_statement(wire: &mut Vec<u8>, how: Framing<'_>, params: &[Value], exact_float_text: bool) -> Result<(), String> {
    let Framing { name, parse, known, fetch } = how;
    if let Some((sql, evicted)) = parse {
        // The statement this one displaces goes first.
        if let Some(old) = evicted {
            frame_close(wire, Owed::Statement(old))?;
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
    put_portal(wire, fetch.portal);
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
        put_portal(wire, fetch.portal);
        end_msg(wire, at)?;
    }
    frame_execute(wire, fetch)
}

/// Execute: the portal, and how many rows (0 is all of them).
fn frame_execute(wire: &mut Vec<u8>, fetch: Fetch) -> Result<(), String> {
    let at = begin_msg(wire, b'E');
    put_portal(wire, fetch.portal);
    let limit = i32::try_from(fetch.limit).map_err(|_| "a page of more rows than an Execute can ask for".to_string())?;
    wire.extend_from_slice(&limit.to_be_bytes());
    end_msg(wire, at)
}

/// Refuse a `COPY … FROM STDIN` the server has just started waiting on (see `exchange`).
fn refuse_copy(se: &mut Session) -> Result<(), String> {
    se.wire.clear();
    let at = begin_msg(&mut se.wire, b'f');
    put_cstr(&mut se.wire, "a Helix connection does not stream COPY data — load rows with `insert`");
    end_msg(&mut se.wire, at)?;
    let at = begin_msg(&mut se.wire, b'S');
    end_msg(&mut se.wire, at)?;
    send_framed(&mut se.stream, &se.wire)
}

/// ONE STATEMENT'S ANSWER, as its messages arrive — the same reader whether the statement went
/// alone or in a flight.
struct Answer {
    cols: Vec<ColBuf>,
    described: bool,
    /// What the statement was known to return before it went — so its fixed-width columns
    /// were asked for in binary.
    known: Option<Rc<RowDesc>>,
    /// What the statement returns, when the server was asked and said.
    learned: Option<Rc<RowDesc>>,
    /// ParseComplete arrived: the statement exists on the server from here on.
    parsed: bool,
    affected: i64,
    /// PortalSuspended arrived: the row limit was reached, and the portal stays for the next
    /// page.
    suspended: bool,
    /// The first cell that was not its column's type. The rows after it are still READ — the
    /// connection has to reach `ReadyForQuery` — and the error is reported once it has.
    bad_cell: Option<String>,
}

impl Answer {
    fn new(known: Option<&Rc<RowDesc>>, exact_float_text: bool) -> Answer {
        Answer {
            cols: known.map(|d| d.bufs(true, exact_float_text)).unwrap_or_default(),
            described: known.is_some(),
            known: known.cloned(),
            learned: None,
            parsed: false,
            affected: 0,
            suspended: false,
            bad_cell: None,
        }
    }

    /// A page of a portal described when it was opened: read into buffers of the shape and
    /// formats its first page had.
    fn page(cols: Vec<ColBuf>) -> Answer {
        Answer { cols, described: true, known: None, learned: None, parsed: false, affected: 0, suspended: false, bad_cell: None }
    }

    /// Take one message. Answers whether it was the statement's LAST — CommandComplete, or
    /// EmptyQueryResponse for a statement with nothing in it. A message that is not part of an
    /// answer (a notice, a parameter status, BindComplete, CloseComplete) is nobody's.
    fn take(&mut self, tag: u8, body: &[u8]) -> Result<bool, String> {
        match tag {
            b'1' => self.parsed = true,
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
                self.cols = desc.iter().map(|(name, oid)| ColBuf::new(name.clone(), *oid)).collect();
                self.learned = Some(Rc::new(RowDesc { cols: desc }));
                self.described = true;
            }
            // NoData: a statement with no result columns.
            b'n' => {
                self.learned = Some(Rc::new(RowDesc { cols: Vec::new() }));
                self.described = true;
            }
            // DataRow
            b'D' => {
                if self.bad_cell.is_some() {
                    return Ok(false);
                }
                let mut c = Cur::new(body);
                let n = usize::try_from(c.i16()?).map_err(|_| "negative column count".to_string())?;
                if n != self.cols.len() {
                    return Err(format!("the server sent a row of {n} values for {} columns", self.cols.len()));
                }
                for col in self.cols.iter_mut() {
                    let v = c.field()?;
                    if let Err(e) = col.push(v) {
                        self.bad_cell = Some(e);
                        break;
                    }
                }
            }
            // CommandComplete: what the statement did, and to how many rows.
            b'C' => {
                self.affected = rows_affected(Cur::new(body).cstr_ref()?);
                return Ok(true);
            }
            // PortalSuspended: the row limit was reached; the rest of the result waits for the
            // next Execute of the same portal.
            b's' => {
                self.suspended = true;
                return Ok(true);
            }
            // EmptyQueryResponse stands in for CommandComplete.
            b'I' => return Ok(true),
            _ => {}
        }
        Ok(false)
    }

    /// What the statement produced — once the connection has reached `ReadyForQuery`, so
    /// neither refusal here costs the connection.
    fn finish(self) -> Result<(Outcome, Option<Rc<RowDesc>>), Fail> {
        let failed = |text: String, parsed: bool| Fail { text, code: String::new(), parsed, broken: false, unsent: false };
        if !self.described {
            return Err(failed("the server never described the result".to_string(), self.parsed));
        }
        if let Some(text) = self.bad_cell {
            return Err(failed(text, self.parsed));
        }
        let binary = self.known.is_some();
        let desc = self.known.or_else(|| self.learned.clone());
        let out = Outcome { cols: self.cols, affected: self.affected, suspended: self.suspended, desc, binary, bound: None };
        Ok((out, self.learned))
    }
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
fn exchange(se: &mut Session, how: Framing<'_>, params: &[Value]) -> Result<(Outcome, Option<Rc<RowDesc>>), Fail> {
    // The whole exchange, framed, to go out in ONE write — every Close owed from earlier at its
    // head. Nothing has been sent until it is whole, so a parameter that cannot be framed leaves
    // the connection untouched, and the Closes owed for the next.
    let owed = std::mem::take(&mut se.owed);
    if let Err(text) = frame(&mut se.wire, &owed, how, params, se.exact_float_text) {
        se.owed.extend(owed);
        return Err(Fail::unsent(text));
    }
    let mut answer = Answer::new(how.known, se.exact_float_text);
    send_and_read(se, &mut answer)?;
    let (mut out, learned) = answer.finish()?;
    out.bound = how.name.cached();
    Ok((out, learned))
}

/// One more page of a suspended portal: every Close owed, then Execute and Sync — its rows read
/// into `cols`, buffers of the shape and formats its first page had — and whether the portal
/// was suspended again or has now run to its end (`Outcome::suspended`).
pub fn run_page(se: &mut Session, fetch: Fetch, cols: Vec<ColBuf>) -> Result<Outcome, Fail> {
    let owed = std::mem::take(&mut se.owed);
    se.wire.clear();
    let framed = frame_closes(&mut se.wire, &owed).and_then(|()| frame_execute(&mut se.wire, fetch)).and_then(|()| {
        let at = begin_msg(&mut se.wire, b'S');
        end_msg(&mut se.wire, at)
    });
    if let Err(text) = framed {
        se.owed.extend(owed);
        return Err(Fail::unsent(text));
    }
    let mut answer = Answer::page(cols);
    send_and_read(se, &mut answer)?;
    answer.finish().map(|(out, _)| out)
}

/// Send what `wire` holds — one exchange, ending in Sync — and read its answer to
/// `ReadyForQuery`.
fn send_and_read(se: &mut Session, answer: &mut Answer) -> Result<(), Fail> {
    let sent = send_framed(&mut se.stream, &se.wire);
    if se.wire.capacity() > KEEP_WIRE {
        se.wire = Vec::new();
    }
    sent?;
    loop {
        let (tag, body) = se.stream.next_msg()?;
        match tag {
            b'E' => {
                let (text, code) = (error_text(body), error_code(body));
                // Drain to the synchronisation point so the connection is left in a known
                // state even though this query is finished. If that fails it is not.
                let drained = drain_to_ready(&mut se.stream);
                if let Ok(status) = drained {
                    se.saw_ready(status);
                }
                return Err(Fail { text, code, parsed: answer.parsed, broken: drained.is_err(), unsent: false });
            }
            // CopyInResponse: the statement is a `COPY … FROM STDIN`, and the server now waits
            // for rows this connection has no way to be handed. Left alone, that wait is the
            // whole read timeout and then a closed connection; refused, it is an ordinary
            // error. Copy-in mode swallowed the Sync that went out with the statement, so the
            // refusal brings its own.
            b'G' => refuse_copy(se)?,
            // ReadyForQuery — the synchronisation point, and where the session stands.
            b'Z' => {
                let status = body.first().copied().unwrap_or(b'I');
                se.saw_ready(status);
                return Ok(());
            }
            _ => {
                answer.take(tag, body)?;
            }
        }
    }
}

/// One statement of a flight: its text, its parameters, and through which portal how many rows
/// are asked for (`Fetch::ALL` for a statement).
#[derive(Clone, Copy)]
pub struct Item<'a> {
    pub sql: &'a str,
    pub params: &'a [Value],
    pub fetch: Fetch,
    /// Run as the UNNAMED statement, cached by nobody: transaction control (`begin`, `commit`,
    /// `rollback`). A cached name can go stale — `DEALLOCATE ALL`, a pooler's other backend —
    /// and a statement that goes stale INSIDE a transaction cannot be prepared again there;
    /// for the statement that was to END the transaction, that would leave the session in one
    /// that has failed, with nothing left to end it. Parsing `commit` afresh costs the server
    /// microseconds; a session stuck in an aborted transaction costs the caller everything
    /// after.
    pub fresh: bool,
}

/// Why a flight failed, and at which statement (counted from 0) when it was one's doing.
pub struct FlightFail {
    pub at: Option<usize>,
    pub fail: Fail,
}

/// How one statement of a flight goes out.
struct Plan {
    name: Name,
    /// Parsed in this flight, and — cached — under this number.
    parse: bool,
    new_id: Option<u64>,
    evict: Option<u64>,
    known: Option<Rc<RowDesc>>,
}

/// SEVERAL STATEMENTS, ONE ROUND TRIP, ONE TRANSACTION.
///
/// Each statement is framed exactly as it would be alone — Parse only if this connection has
/// not prepared its text, Describe only if what it returns is not yet known, binary where that
/// is known to be the same value — and the flight ends with ONE Sync. That Sync is what makes
/// it a transaction without anyone saying `begin`: the server commits at Sync, and a statement
/// that fails makes it skip every later one and roll back every earlier one. So a flight is
/// all or nothing, and what comes back is every statement's answer or one error that names
/// the statement it belongs to.
///
/// THE CACHE IS PLANNED BEFORE ANYTHING IS FRAMED: a text met twice in one flight is parsed
/// once; a statement the flight binds is never the one displaced to make room for another;
/// and when every cached statement is spoken for, the rest go as the unnamed statement. A name
/// that turns out stale (`26000`, `0A000`) or taken (`42P05`) is dealt with as it is for one
/// statement, by sending again, once — which the rollback makes safe: nothing of the first
/// attempt happened. Inside a transaction's value there is no again; see `run_prepared`.
///
/// IT IS SENT WHILE ITS ANSWER IS TAKEN IN (`Stream::send_draining`), because the first
/// statement's rows are on their way while the last is still being written.
pub fn run_flight(se: &mut Session, items: &[Item<'_>]) -> Result<Vec<Outcome>, FlightFail> {
    // A flight of one is that statement: nothing to share, nothing that can deadlock.
    if let [only] = items {
        let ran = if only.fresh {
            exchange(se, Framing { name: Name::Unnamed, parse: Some((only.sql, None)), known: None, fetch: only.fetch }, only.params)
                .map(|(out, _)| out)
        } else {
            run_fetching(se, only.sql, only.params, only.fetch)
        };
        return ran.map(|out| vec![out]).map_err(|fail| FlightFail { at: Some(0), fail });
    }
    // A COPY GOES ALONE. `COPY … FROM STDIN` makes the server read what follows as COPY data;
    // what follows in a flight is the next statement, and a real server does not skip that to
    // the Sync — it has read one byte of a message it will not finish, has lost its place in
    // the stream, and ENDS THE CONNECTION (found against PostgreSQL 17; the fake server had been
    // written to be kinder). Once the flight is on the wire nothing can be done about it, so it
    // is refused before anything is — every form of COPY, since telling `FROM STDIN` from
    // `FROM 'file'` needs the server's own parser, and a COPY's data is nothing this
    // connection carries either way.
    if let Some(k) = items.iter().position(|item| starts_with_copy(item.sql)) {
        let text = "a `COPY` cannot share a round trip — the server reads whatever follows it as COPY data — so it goes on its own".to_string();
        return Err(FlightFail { at: Some(k), fail: Fail::unsent(text) });
    }
    let mut named = true;
    let (mut went_stale, mut was_taken) = (false, false);
    loop {
        let failed = match fly(se, items, named) {
            Ok(outs) => return Ok(outs),
            Err(f) => f,
        };
        let (stale, taken) = (failed.fail.stale(), failed.fail.taken());
        if failed.fail.broken || !(stale || taken) || (stale && went_stale) || (taken && was_taken) {
            return Err(failed);
        }
        if stale {
            // ONE STALE NAME IS RARELY ALONE. `DEALLOCATE ALL`, `DISCARD ALL`, a pooler's other
            // backend: every name went together, and a flight that forgot only the one the
            // server happened to refuse would be refused for the next, and the next. So every
            // cached statement the flight binds is forgotten — and CLOSED by the next exchange
            // (the next attempt, or whatever follows an error that ended a transaction), so
            // that one which was still there (`0A000` spares the others) is not left behind.
            // Closing a name that is gone is not an error.
            for item in items {
                if let Some(e) = se.prepared.by_sql.remove(item.sql) {
                    se.owe(Owed::Statement(e.id));
                }
            }
        }
        // The error ended a transaction somebody else began: there is no sending again.
        if se.status == b'E' {
            let text = if stale {
                format!(
                    "{} — the statement was prepared before a change that made it stale, and an error inside a transaction ends the transaction: roll back and run it again",
                    failed.fail.text
                )
            } else {
                failed.fail.text.clone()
            };
            return Err(FlightFail { at: failed.at, fail: Fail { text, ..failed.fail } });
        }
        named = named && !taken;
        went_stale |= stale;
        was_taken |= taken;
    }
}

/// Whether a statement's first word is COPY — past whitespace, `--` comments and `/* */`
/// comments, which nest in PostgreSQL.
fn starts_with_copy(sql: &str) -> bool {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else if let Some(after) = rest.strip_prefix("/*") {
            let (mut depth, mut at) = (1usize, 0usize);
            let bytes = after.as_bytes();
            while depth > 0 {
                match (bytes.get(at), bytes.get(at + 1)) {
                    (Some(b'/'), Some(b'*')) => (depth, at) = (depth + 1, at + 2),
                    (Some(b'*'), Some(b'/')) => (depth, at) = (depth - 1, at + 2),
                    (Some(_), _) => at += 1,
                    // Unterminated: the server will say so; it is not a COPY here.
                    (None, _) => return false,
                }
            }
            rest = after.get(at..).unwrap_or("");
        } else {
            break;
        }
        rest = rest.trim_start();
    }
    let word_ends = |c: char| !(c.is_alphanumeric() || c == '_');
    rest.get(..4).is_some_and(|w| w.eq_ignore_ascii_case("copy")) && rest.get(4..).is_some_and(|r| r.chars().next().is_none_or(word_ends))
}

/// One attempt at a flight.
fn fly(se: &mut Session, items: &[Item<'_>], named: bool) -> Result<Vec<Outcome>, FlightFail> {
    let whole = |fail: Fail| FlightFail { at: None, fail };

    // PLAN. First every statement this connection already has — all of them, so that none is
    // the one displaced to make room for a new statement EARLIER in the same flight — then the
    // new ones, `spoken_for` growing with each name taken and each victim chosen.
    let hits: Vec<Option<(u64, Option<Rc<RowDesc>>)>> =
        items.iter().map(|item| if item.fresh { None } else { se.prepared.touch(item.sql) }).collect();
    let mut spoken_for: Vec<u64> = hits.iter().flatten().map(|(id, _)| *id).collect();
    let mut plans: Vec<Plan> = Vec::with_capacity(items.len());
    for (i, (item, hit)) in items.iter().zip(hits).enumerate() {
        let twin = items.get(..i).unwrap_or(&[]).iter().position(|earlier| earlier.sql == item.sql);
        let plan = if item.fresh {
            Plan { name: Name::Unnamed, parse: true, new_id: None, evict: None, known: None }
        } else if let Some((id, known)) = hit {
            Plan { name: Name::Cached(id), parse: false, new_id: None, evict: None, known }
        } else if let Some(id) = twin.and_then(|j| plans.get(j)).and_then(|p| p.new_id) {
            // The same text, earlier in this flight: parsed there, bound here.
            Plan { name: Name::Cached(id), parse: false, new_id: None, evict: None, known: None }
        } else {
            let reserved = if named { se.prepared.reserve(&spoken_for) } else { None };
            match reserved {
                Some((id, evict)) => {
                    spoken_for.push(id);
                    spoken_for.extend(evict);
                    Plan { name: Name::Cached(id), parse: true, new_id: Some(id), evict, known: None }
                }
                None => Plan { name: Name::Unnamed, parse: true, new_id: None, evict: None, known: None },
            }
        };
        plans.push(plan);
    }

    // FRAME. Nothing has been sent until the flight is whole — and the Closes owed from earlier,
    // which ride at its head, stay owed to the next exchange if it is not.
    se.wire.clear();
    let owed = std::mem::take(&mut se.owed);
    let framed = frame_closes(&mut se.wire, &owed).map_err(|text| whole(Fail::unsent(text))).and_then(|()| {
        for (k, (item, plan)) in items.iter().zip(&plans).enumerate() {
            let parse = plan.parse.then_some((item.sql, plan.evict));
            let how = Framing { name: plan.name, parse, known: plan.known.as_ref(), fetch: item.fetch };
            frame_statement(&mut se.wire, how, item.params, se.exact_float_text)
                .map_err(|text| FlightFail { at: Some(k), fail: Fail::unsent(text) })?;
        }
        let at = begin_msg(&mut se.wire, b'S');
        end_msg(&mut se.wire, at).map_err(|text| whole(Fail::unsent(text)))
    });
    if let Err(failed) = framed {
        se.owed.extend(owed);
        return Err(failed);
    }
    let sent = se.stream.send_draining(&se.wire);
    if se.wire.capacity() > KEEP_WIRE {
        se.wire = Vec::new();
    }
    sent.map_err(|text| whole(Fail::from(text)))?;

    // READ. Answers arrive in order; the one being read is the one an error belongs to.
    let mut answers: Vec<Answer> = plans.iter().map(|p| Answer::new(p.known.as_ref(), se.exact_float_text)).collect();
    let mut cur = 0usize;
    let mut refused: Option<(usize, String, String)> = None;
    loop {
        let (tag, body) = se.stream.next_msg().map_err(|text| whole(Fail::from(text)))?;
        match tag {
            // The server skips everything after this to the Sync; so does the reading.
            b'E' => {
                if refused.is_none() {
                    refused = Some((cur, error_text(body), error_code(body)));
                }
            }
            // A `COPY … FROM STDIN` that got past `starts_with_copy`, as the LAST statement:
            // the server would wait for ever, its copy-in mode having swallowed the flight's
            // Sync, so it is refused with a Sync of its own, as it is for one statement.
            // (Anywhere else the server has already ended the connection, and the read that
            // finds out is the next one.)
            b'G' if cur + 1 == items.len() => refuse_copy(se).map_err(|text| whole(Fail::from(text)))?,
            b'Z' => {
                let status = body.first().copied().unwrap_or(b'I');
                se.saw_ready(status);
                break;
            }
            _ => {
                if refused.is_none()
                    && let Some(answer) = answers.get_mut(cur)
                    && answer.take(tag, body).map_err(|text| whole(Fail::from(text)))?
                {
                    cur += 1;
                }
            }
        }
    }

    // REMEMBER what the server now has, whatever became of the flight: a statement parsed
    // before the one that failed exists (preparing is not undone by a rollback), and a victim
    // whose Close was reached is gone. Past the failure the server read nothing.
    let reached = refused.as_ref().map_or(items.len(), |(k, _, _)| k + 1);
    for ((item, plan), answer) in items.iter().zip(&plans).zip(&answers).take(reached) {
        if let Some(old) = plan.evict {
            se.prepared.forget(old);
        }
        match (plan.new_id, &answer.learned) {
            (Some(id), learned) if answer.parsed => se.prepared.insert(item.sql, id, learned.clone()),
            (None, Some(learned)) if !plan.parse => se.prepared.describe(item.sql, learned.clone()),
            _ => {}
        }
    }

    if let Some((k, text, code)) = refused {
        let parsed = answers.get(k).is_some_and(|a| a.parsed);
        return Err(FlightFail { at: Some(k), fail: Fail { text, code, parsed, broken: false, unsent: false } });
    }
    if cur != items.len() {
        let text = format!("the server answered {cur} of {} statements", items.len());
        return Err(whole(Fail { text, code: String::new(), parsed: false, broken: false, unsent: false }));
    }
    let mut outs = Vec::with_capacity(answers.len());
    for (k, (answer, plan)) in answers.into_iter().zip(&plans).enumerate() {
        let mut out = answer.finish().map_err(|fail| FlightFail { at: Some(k), fail })?.0;
        out.bound = plan.name.cached();
        outs.push(out);
    }
    Ok(outs)
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
        fn all<'a>(name: Name, parse: Option<(&'a str, Option<u64>)>, known: Option<&'a Rc<RowDesc>>) -> Framing<'a> {
            Framing { name, parse, known, fetch: Fetch::ALL }
        }
        let mut wire = Vec::new();
        frame(&mut wire, &[], all(Name::Cached(7), Some(("select 1", Some(3))), None), &[Value::Int(5)], true).unwrap();
        assert_eq!(tags(&wire), "CPBDES");
        assert!(wire.windows(9).any(|w| w == b"_helix_7\0") && wire.windows(9).any(|w| w == b"_helix_3\0"));

        let desc = Rc::new(RowDesc { cols: vec![("id".into(), 20), ("name".into(), 25), ("score".into(), 701)] });
        frame(&mut wire, &[], all(Name::Cached(7), None, Some(&desc)), &[Value::Int(5), Value::Missing], true).unwrap();
        assert_eq!(tags(&wire), "BES");
        // Bind's tail: three result formats — binary, text, binary.
        let bind_len = 1 + i32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]) as usize;
        assert_eq!(&wire[bind_len - 8..bind_len], &[0, 3, 0, 1, 0, 0, 0, 1]);
        // And its parameters: `5` as text, then NULL.
        assert!(wire[..bind_len].windows(9).any(|w| w == [0, 0, 0, 1, b'5', 0xff, 0xff, 0xff, 0xff]));

        // Before PostgreSQL 12 a float's text is rounded, so it stays text; and a statement
        // with nothing fixed-width asks for nothing.
        frame(&mut wire, &[], all(Name::Cached(7), None, Some(&desc)), &[], false).unwrap();
        let bind_len = 1 + i32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]) as usize;
        assert_eq!(&wire[bind_len - 8..bind_len], &[0, 3, 0, 1, 0, 0, 0, 0]);
        let text_only = Rc::new(RowDesc { cols: vec![("name".into(), 25)] });
        frame(&mut wire, &[], all(Name::Unnamed, None, Some(&text_only)), &[], true).unwrap();
        let bind_len = 1 + i32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]) as usize;
        assert_eq!(&wire[bind_len - 2..bind_len], &[0, 0]);

        // A CURSOR'S FIRST PAGE: bound to a named portal, Executed for a page of rows — and what
        // earlier exchanges let go of closed at the head: a statement found stale, a portal a
        // cursor was done with. Its later pages are an Execute of the same portal and a Sync.
        let page = Fetch { portal: Portal::Named(4), limit: 500 };
        let owed = [Owed::Statement(9), Owed::Portal(11)];
        frame(&mut wire, &owed, Framing { name: Name::Cached(7), parse: None, known: Some(&desc), fetch: page }, &[], true).unwrap();
        assert_eq!(tags(&wire), "CCBES");
        let holds = |w: &[u8], part: &[u8]| w.windows(part.len()).any(|x| x == part);
        assert!(holds(&wire, b"S_helix_9\0"), "the statement's Close names it");
        assert!(holds(&wire, b"P_helix_cursor_11\0"), "the portal's Close names it");
        assert_eq!(wire.windows(16).filter(|w| *w == b"_helix_cursor_4\0").count(), 2, "Bind and Execute name the portal");
        assert_eq!(&wire[wire.len() - 9..wire.len() - 5], &500i32.to_be_bytes(), "Execute's row limit, then the Sync");
        wire.clear();
        frame_execute(&mut wire, page).unwrap();
        assert_eq!(tags(&wire), "E");
        assert!(wire.ends_with(b"_helix_cursor_4\0\0\0\x01\xf4"));
    }

    fn param(v: Value) -> String {
        let mut out = Vec::new();
        put_param(&mut out, &v).unwrap();
        let len = i32::from_be_bytes([out[0], out[1], out[2], out[3]]) as usize;
        assert_eq!(len, out.len() - 4, "the length prefix covers the text exactly");
        String::from_utf8(out[4..].to_vec()).unwrap()
    }

    fn arr(vs: Vec<Value>) -> Value {
        Value::Array(Rc::new(ArrayData::Values(vs)))
    }

    fn st(s: &str) -> Value {
        Value::Str(Rc::new(s.to_string()))
    }

    /// An Array is the array literal `= any($1)` binds: numbers bare, `missing` the bare word
    /// NULL, and a String ALWAYS quoted — so the text `NULL`, a comma, a brace, a quote, a
    /// backslash and leading spaces are each one element's text and nothing else.
    #[test]
    fn an_array_is_the_array_literal_any_binds() {
        assert_eq!(param(Value::Array(Rc::new(ArrayData::Ints(vec![1, -2, 3])))), "{1,-2,3}");
        assert_eq!(param(Value::Array(Rc::new(ArrayData::Floats(vec![1.5, f64::NAN, f64::INFINITY])))), "{1.5,NaN,inf}");
        assert_eq!(param(arr(vec![])), "{}");
        assert_eq!(param(arr(vec![Value::Bool(true), Value::Missing, Value::Bool(false)])), "{t,NULL,f}");
        assert_eq!(
            param(arr(vec![st("a,b"), st("q\"t"), st("back\\slash"), st("NULL"), st("{x}"), st("  lead"), st(""), Value::Missing])),
            r#"{"a,b","q\"t","back\\slash","NULL","{x}","  lead","",NULL}"#
        );
        // A nested Array is a further dimension, to PostgreSQL's own limit of six.
        assert_eq!(param(arr(vec![arr(vec![Value::Int(1), Value::Int(2)]), arr(vec![Value::Int(3), Value::Int(4)])])), "{{1,2},{3,4}}");
        let mut deep = arr(vec![Value::Int(1)]);
        for _ in 0..MAX_ARRAY_DEPTH - 1 {
            deep = arr(vec![deep]);
        }
        assert_eq!(param(deep.clone()), "{{{{{{1}}}}}}");
        let mut out = Vec::new();
        let e = put_param(&mut out, &arr(vec![deep])).unwrap_err();
        assert!(e.contains("nested more than 6 deep"), "{e}");
        // And `bytea` is hex — bare as a parameter, quoted (its backslash escaped) as an element.
        let bytes = Value::Bytes(Rc::new(vec![0, 10, 255]));
        assert_eq!(param(bytes.clone()), "\\x000aff");
        assert_eq!(param(arr(vec![bytes])), r#"{"\\x000aff"}"#);
    }

    #[test]
    fn a_copy_is_recognised_past_whitespace_and_comments() {
        for sql in [
            "copy t from stdin",
            "  COPY t (a, b) FROM STDIN WITH (FORMAT csv)",
            "-- load it\n  Copy t from stdin",
            "/* one /* nested */ still a comment */ copy(select 1) to stdout",
            "copy",
        ] {
            assert!(starts_with_copy(sql), "{sql}");
        }
        for sql in [
            "select 'copy t from stdin'",
            "copyright_holders_insert()",
            "copy_of_t",
            "-- copy t from stdin\nselect 1",
            "/* copy t from stdin */ select 1",
            "/* never closed copy",
            "",
            "cöpy",
        ] {
            assert!(!starts_with_copy(sql), "{sql}");
        }
    }
}

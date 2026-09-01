//! PostgreSQL frontend/backend protocol v3 framing — the byte layer, and nothing else.
//!
//! Protocol 3.0 has been stable since 2003. PostgreSQL 18 introduced 3.2 (256-bit cancel
//! keys) and 19 carries it, but the change is backward compatible and `libpq` itself still
//! requests 3.0 by default — so speaking 3.0 reaches every server from 7.4 to 19. That is
//! why this module has no version negotiation: there is nothing to negotiate.
//!
//! TOTALITY IS THE WHOLE DESIGN HERE (ADR 0024). Every byte in this file comes off a
//! socket, which means it is attacker-shaped input in exactly the way a CSV file is not:
//! a hostile or broken server can send any length prefix it likes. So there is no slice
//! indexing, no `unwrap`, and no arithmetic that can wrap into a huge allocation —
//! `Cur` bounds-checks every read and a message length is validated before it is used to
//! size a buffer. A malformed reply is a Helix error naming what was wrong, never an
//! abort.

use std::io::{Read, Write};

/// The largest message this client will allocate for, in bytes.
///
/// The protocol's length prefix is a signed 32-bit count, so a server (or something
/// pretending to be one) can claim 2 GB and make a client reserve it before a single
/// byte of body arrives. 64 MB is far above any real `RowDescription` or `DataRow` and
/// far below a denial of service.
const MAX_MSG: usize = 64 * 1024 * 1024;

/// One backend message: its type byte and its body, length prefix already stripped.
pub struct Msg {
    pub tag: u8,
    pub body: Vec<u8>,
}

impl Msg {
    pub fn cur(&self) -> Cur<'_> {
        Cur { b: &self.body, at: 0 }
    }
}

/// A bounds-checked cursor over a message body.
///
/// Every accessor returns `Result`, because the alternative is indexing a slice with a
/// length the server chose.
pub struct Cur<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or("message field length overflowed")?;
        let s = self.b.get(self.at..end).ok_or_else(|| {
            format!("truncated message: wanted {n} bytes at offset {}, {} remain", self.at, self.b.len().saturating_sub(self.at))
        })?;
        self.at = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub fn i16(&mut self) -> Result<i16, String> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub fn i32(&mut self) -> Result<i32, String> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A NUL-terminated string. Invalid UTF-8 is an error rather than a lossy
    /// replacement: a column name or an error message that is not what the server sent
    /// would be a quiet lie in a diagnostic.
    pub fn cstr(&mut self) -> Result<String, String> {
        let rest = self.b.get(self.at..).ok_or("truncated message")?;
        let n = rest.iter().position(|&c| c == 0).ok_or("unterminated string in message")?;
        let s = std::str::from_utf8(&rest[..n]).map_err(|_| "message string is not UTF-8".to_string())?.to_string();
        self.at += n + 1;
        Ok(s)
    }

    /// `len` bytes, or `None` for the protocol's -1 "this value is NULL".
    pub fn field(&mut self) -> Result<Option<&'a [u8]>, String> {
        let n = self.i32()?;
        if n == -1 {
            return Ok(None);
        }
        let n = usize::try_from(n).map_err(|_| format!("negative field length {n}"))?;
        Ok(Some(self.take(n)?))
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let r = self.b.get(self.at..).unwrap_or(&[]);
        self.at = self.b.len();
        r
    }
}

/// Read one backend message: 1 tag byte, then a 4-byte length that INCLUDES itself.
pub fn read_msg(r: &mut impl Read) -> Result<Msg, String> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head).map_err(|e| format!("reading from the server: {e}"))?;
    let tag = head[0];
    let len = i32::from_be_bytes([head[1], head[2], head[3], head[4]]);
    // The length counts itself, so anything under 4 is malformed rather than merely empty.
    let body_len = len
        .checked_sub(4)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| format!("message '{}' has an impossible length {len}", tag as char))?;
    if body_len > MAX_MSG {
        return Err(format!(
            "message '{}' claims {body_len} bytes, over the {MAX_MSG}-byte limit",
            tag as char
        ));
    }
    let mut body = vec![0u8; body_len];
    r.read_exact(&mut body).map_err(|e| format!("reading message '{}': {e}", tag as char))?;
    Ok(Msg { tag, body })
}

/// Write one frontend message. `tag` is `None` only for the startup and SSL-request
/// packets, which are the two the protocol sends untagged.
pub fn write_msg(w: &mut impl Write, tag: Option<u8>, body: &[u8]) -> Result<(), String> {
    let len = i32::try_from(body.len() + 4).map_err(|_| "message too large to send".to_string())?;
    let mut out = Vec::with_capacity(body.len() + 5);
    if let Some(t) = tag {
        out.push(t);
    }
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    w.write_all(&out).map_err(|e| format!("sending to the server: {e}"))?;
    w.flush().map_err(|e| format!("sending to the server: {e}"))
}

/// Append a NUL-terminated string, the protocol's only string form.
pub fn put_cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// The fields of an `ErrorResponse` / `NoticeResponse`, rendered as one line.
///
/// The server sends a set of tagged fields; `M` (the primary message) is the one a user
/// needs, and `C` (SQLSTATE) is what makes an error identifiable rather than merely
/// readable. `D` and `H` are included when present because "detail" and "hint" are
/// exactly the parts that turn a rejection into a fix.
pub fn error_text(m: &Msg) -> String {
    let mut cur = m.cur();
    let (mut msg, mut code, mut detail, mut hint) = (String::new(), String::new(), String::new(), String::new());
    while let Ok(f) = cur.u8() {
        // A zero field type is the terminator; anything unreadable after it means the
        // message was truncated, and a partial diagnostic beats none.
        if f == 0 {
            break;
        }
        let Ok(v) = cur.cstr() else { break };
        match f {
            b'M' => msg = v,
            b'C' => code = v,
            b'D' => detail = v,
            b'H' => hint = v,
            _ => {}
        }
    }
    if msg.is_empty() {
        msg = "the server reported an error with no message".to_string();
    }
    let mut out = msg;
    if !code.is_empty() {
        out = format!("{out} (SQLSTATE {code})");
    }
    if !detail.is_empty() {
        out = format!("{out} — {detail}");
    }
    if !hint.is_empty() {
        out = format!("{out} — {hint}");
    }
    out
}

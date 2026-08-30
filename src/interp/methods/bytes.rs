//! Methods on `Bytes` — the binary counterpart to `String` (ADR 0042).
//!
//! Deliberately mirrors the `String` surface where the operation means the same thing
//! (`length`/`count`, `take`/`drop`, `concat`, `write_to`/`append_to`) so a reader who knows
//! one knows the other. The methods that differ are the ones where TEXT and BYTES genuinely
//! differ: `byte_at` answers an `Int` in 0..=255 where `char_at` answers a one-character
//! string, and `to_string()` can FAIL where a string's identity cannot.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

use super::arity;

/// Lowercase hex, the form `Bytes` prints and `to_hex` returns.
pub(crate) fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        // `write!` to a String cannot fail, but it returns a Result the ratchet would count.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0x0f) as usize] as char);
    }
    s
}

fn type_err(who: &str, expected: &str, got: &Value, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!("`{who}` expects {expected}, but got {}", crate::value::with_article(got.type_name())),
        line,
        col,
    )
}

fn int_arg(v: &Value, who: &str, what: &str, line: usize, col: usize) -> Result<i64, HelixError> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(type_err(who, what, other, line, col)),
    }
}

pub(crate) fn bytes_method(
    b: &Rc<Vec<u8>>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "length" | "count" => {
            arity(name, args, 0, line, col)?;
            Ok(Value::Int(b.len() as i64))
        }
        "is_empty" => {
            arity(name, args, 0, line, col)?;
            Ok(Value::Bool(b.is_empty()))
        }
        // `char_at`'s counterpart. An `Int` in 0..=255, and `missing` past the end for the
        // same reason `char_at` gives `missing`: an out-of-range read has no honest answer,
        // and ADR 0001 propagates the absence instead of inventing a zero.
        "byte_at" => {
            arity(name, args, 1, line, col)?;
            let i = int_arg(&args[0], "byte_at", "an Int index", line, col)?;
            if i < 0 || i as usize >= b.len() {
                return Ok(Value::Missing);
            }
            Ok(Value::Int(b[i as usize] as i64))
        }
        // Saturating like the String twins: `take` past the end is the whole value, and
        // `drop` past the end is empty. Neither is an error, because a slice that runs off
        // the end is how a final partial page reads.
        "take" | "drop" => {
            arity(name, args, 1, line, col)?;
            let n = int_arg(&args[0], name, "an Int count", line, col)?;
            let n = n.max(0).min(b.len() as i64) as usize;
            let out = if name == "take" { b[..n].to_vec() } else { b[n..].to_vec() };
            Ok(Value::Bytes(Rc::new(out)))
        }
        "slice" => {
            arity(name, args, 2, line, col)?;
            let s = int_arg(&args[0], "slice", "an Int start", line, col)?.max(0) as usize;
            let e = int_arg(&args[1], "slice", "an Int end", line, col)?.max(0) as usize;
            let s = s.min(b.len());
            let e = e.max(s).min(b.len());
            Ok(Value::Bytes(Rc::new(b[s..e].to_vec())))
        }
        "concat" => {
            arity(name, args, 1, line, col)?;
            match &args[0] {
                Value::Bytes(o) => {
                    let mut out = Vec::with_capacity(b.len() + o.len());
                    out.extend_from_slice(b);
                    out.extend_from_slice(o);
                    Ok(Value::Bytes(Rc::new(out)))
                }
                other => Err(type_err("concat", "Bytes", other, line, col)
                    .hint("convert text first: `s.to_bytes()`.")),
            }
        }
        "to_hex" => {
            arity(name, args, 0, line, col)?;
            Ok(Value::Str(Rc::new(to_hex(b))))
        }
        "to_base64" => {
            arity(name, args, 0, line, col)?;
            use base64::Engine;
            let enc = base64::engine::general_purpose::STANDARD.encode(b.as_slice());
            Ok(Value::Str(Rc::new(enc)))
        }
        // THE ONE THAT CAN FAIL, and the reason `Bytes` exists as a separate type. Arbitrary
        // bytes are not text: this refuses by name rather than substituting U+FFFD, which
        // would silently change the data on the way out.
        "to_string" => {
            arity(name, args, 0, line, col)?;
            match std::str::from_utf8(b) {
                Ok(s) => Ok(Value::Str(Rc::new(s.to_string()))),
                Err(e) => Err(HelixError::new(
                    format!("these bytes are not valid UTF-8 (byte {})", e.valid_up_to()),
                    line,
                    col,
                )
                .hint(
                    "a String is UTF-8 by definition. For arbitrary bytes use `to_hex()` or \
                     `to_base64()`, which never fail.",
                )),
            }
        }
        "write_to" | "append_to" => {
            arity(name, args, 1, line, col)?;
            let path = match &args[0] {
                Value::Str(p) => p,
                other => return Err(type_err(name, "a string path", other, line, col)),
            };
            use std::io::Write;
            let r = if name == "write_to" {
                std::fs::write(path.as_str(), b.as_slice())
            } else {
                std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path.as_str())
                    .and_then(|mut f| f.write_all(b))
            };
            r.map_err(|e| {
                HelixError::new(format!("could not write `{path}`: {e}"), line, col)
            })?;
            Ok(Value::Int(b.len() as i64))
        }
        // `write_at`'s Bytes twin — the one that makes a page-oriented store binary.
        "write_at" => {
            arity(name, args, 2, line, col)?;
            let path = match &args[0] {
                Value::Str(p) => p,
                other => return Err(type_err("write_at", "a string path", other, line, col)),
            };
            let off = int_arg(&args[1], "write_at", "an Int offset", line, col)?;
            if off < 0 {
                return Err(HelixError::new(
                    format!("`write_at` needs a non-negative offset, got {off}"),
                    line,
                    col,
                ));
            }
            use std::io::{Seek, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path.as_str())
                .map_err(|e| HelixError::new(format!("could not open `{path}`: {e}"), line, col))?;
            f.seek(std::io::SeekFrom::Start(off as u64)).map_err(|e| {
                HelixError::new(format!("could not seek `{path}` to {off}: {e}"), line, col)
            })?;
            f.write_all(b).map_err(|e| {
                HelixError::new(format!("could not write `{path}`: {e}"), line, col)
            })?;
            Ok(Value::Int(b.len() as i64))
        }
        other => Err(HelixError::new(
            format!("type Bytes has no method `{other}`"),
            line,
            col,
        )
        .hint(
            "Bytes has length/count, is_empty, byte_at, take, drop, slice, concat, to_hex, \
             to_base64, to_string, write_to, append_to and write_at.",
        )),
    }
}

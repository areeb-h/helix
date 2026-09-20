//! PostgreSQL type OIDs → Helix column data, a cell at a time and straight off the wire.
//!
//! TEXT IS THE FORMAT EVERY COLUMN CAN BE READ IN, and the one a column is read in the first
//! time its statement runs. That is a deliberate choice, not a shortcut: it makes the
//! unknown-type case TOTAL. A column this table has never heard of — `uuid`, `jsonb`,
//! `tsrange`, an extension type, a domain over any of them — still reads, as the text the
//! server produced. That is ADR 0033 Stage 2's rule for foreign parquet dtypes, applied for the
//! same reason: refusing a column because the reader lacks an opinion about it is worse than
//! handing back what it says.
//!
//! BINARY IS FOR THE FIVE TYPES WHERE IT IS THE SAME VALUE, CHEAPER. Once a statement's columns
//! are known (its second run on a connection — see `statement`), `int2`/`int4`/`int8`, `bool`
//! and `float8` are asked for in binary. For the integers and `bool` that saves a parse; for
//! `float8` it saves the SERVER printing the shortest decimal that round-trips, which is most of
//! what a float-heavy result costs and nothing a client can speed up. It is only ever a
//! different encoding of the identical value: `float8` text has been exact since PostgreSQL 12
//! (shortest round-trip digits), which is the version binary floats are gated on; `float4` and
//! `numeric` STAY text, because their text is what Helix's Float means by them (`1.1`, not the
//! `1.100000023841858` a widened 32-bit float is, and a decimal parsed once, correctly
//! rounded). Every decoder below is fixed-width, so there is no new failure mode that is not
//! "the server sent the wrong number of bytes".
//!
//! NOTHING IS ALLOCATED PER CELL. A cell used to become a `String` — validated, copied,
//! pushed, and for a number parsed again at the end and freed — so a 1 000-row, 7-column result
//! was 7 000 allocations before the frame existed, and half the read's wall time was this file.
//! Numbers are parsed from the bytes they arrived in into the vectors the engine keeps, and
//! text is interned as it arrives (`backend::strbuild`): one allocation per DISTINCT value.
//!
//! The types below are the ones with a Helix EQUIVALENT, so they become numbers and
//! booleans rather than strings. Everything else is text, and says so in `describe`.

use crate::backend::strbuild::StrBuilder;
use crate::backend::ColData;

// From `pg_type.h`; these OIDs are fixed by the catalog and have not moved in decades.
const BOOL: i32 = 16;
const INT8: i32 = 20;
const INT2: i32 = 21;
const INT4: i32 = 23;
const FLOAT4: i32 = 700;
const FLOAT8: i32 = 701;
const NUMERIC: i32 = 1700;

/// How a column's values should be read.
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Int,
    Float,
    Bool,
    Text,
}

/// The Helix reading for a type OID. Anything unlisted is [`Kind::Text`].
///
/// `numeric` is FLOAT, and that is a lossy choice made on purpose: Postgres `numeric` is
/// arbitrary-precision decimal and Helix has no exact-decimal column, so the options were
/// a float (loses precision past 2^53) or text (loses arithmetic). Money columns are the
/// motivating case and arithmetic is what people do with them, so float wins — and the
/// column's type is visible in `describe`, which is where a reader can see the trade.
pub fn kind_of(oid: i32) -> Kind {
    match oid {
        BOOL => Kind::Bool,
        INT2 | INT4 | INT8 => Kind::Int,
        FLOAT4 | FLOAT8 | NUMERIC => Kind::Float,
        _ => Kind::Text,
    }
}

/// How many bytes `oid` is in binary, when binary is the same value as its text — `None` for
/// every type that stays text. `exact_float_text` is whether this server prints `float8` with
/// round-trip digits (PostgreSQL 12 and later): only then are the two encodings one value, and
/// a statement's first run (text) and its later ones (binary) must never disagree.
pub fn binary_width(oid: i32, exact_float_text: bool) -> Option<usize> {
    match oid {
        BOOL => Some(1),
        INT2 => Some(2),
        INT4 => Some(4),
        INT8 => Some(8),
        FLOAT8 if exact_float_text => Some(8),
        _ => None,
    }
}

/// The cells of one column, in the shape the engine keeps them. An invalid slot holds the
/// placeholder the engine expects there (`0`, `0.0`, `false`).
enum Cells {
    Int { vals: Vec<i64>, valid: Vec<bool> },
    Float { vals: Vec<f64>, valid: Vec<bool> },
    Bool { vals: Vec<bool>, valid: Vec<bool> },
    Text(StrBuilder),
}

/// One column's worth of values, accumulated as they arrive.
pub struct ColBuf {
    pub name: String,
    /// The width this column's values arrive in when it was asked for in binary.
    binary: Option<usize>,
    cells: Cells,
}

impl ColBuf {
    /// A column read as text.
    pub fn new(name: String, oid: i32) -> ColBuf {
        ColBuf::with_format(name, oid, None)
    }

    /// A column read in the format its Bind asked for: `binary` is [`binary_width`]'s answer.
    pub fn with_format(name: String, oid: i32, binary: Option<usize>) -> ColBuf {
        let cells = match kind_of(oid) {
            Kind::Int => Cells::Int { vals: Vec::new(), valid: Vec::new() },
            Kind::Float => Cells::Float { vals: Vec::new(), valid: Vec::new() },
            Kind::Bool => Cells::Bool { vals: Vec::new(), valid: Vec::new() },
            Kind::Text => Cells::Text(StrBuilder::with_capacity(0)),
        };
        ColBuf { name, binary, cells }
    }

    /// Take one cell. A value the server sent that does not read as its own declared type is
    /// an ERROR naming the column and the value, never a silent `missing` — the two are
    /// different claims, and ADR 0001 reserves `missing` for absence.
    pub fn push(&mut self, v: Option<&[u8]>) -> Result<(), String> {
        let Some(b) = v else {
            match &mut self.cells {
                Cells::Int { vals, valid } => {
                    vals.push(0);
                    valid.push(false);
                }
                Cells::Float { vals, valid } => {
                    vals.push(0.0);
                    valid.push(false);
                }
                Cells::Bool { vals, valid } => {
                    vals.push(false);
                    valid.push(false);
                }
                Cells::Text(t) => t.push_missing(),
            }
            return Ok(());
        };
        if let Some(width) = self.binary
            && b.len() != width
        {
            return Err(format!(
                "column `{}`: the server sent {} bytes for a {width}-byte binary value",
                self.name,
                b.len()
            ));
        }
        let name = &self.name;
        match (&mut self.cells, self.binary) {
            (Cells::Int { vals, valid }, None) => {
                let n = parse_int(b)
                    .ok_or_else(|| format!("column `{name}`: `{}` is not an integer", String::from_utf8_lossy(b)))?;
                vals.push(n);
                valid.push(true);
            }
            (Cells::Int { vals, valid }, Some(_)) => {
                // Sign-extended from the width the header check above already held it to.
                let n = match *b {
                    [a, c] => i64::from(i16::from_be_bytes([a, c])),
                    [a, c, d, e] => i64::from(i32::from_be_bytes([a, c, d, e])),
                    [a, c, d, e, f, g, h, i] => i64::from_be_bytes([a, c, d, e, f, g, h, i]),
                    _ => return Err(format!("column `{name}`: a binary integer of {} bytes", b.len())),
                };
                vals.push(n);
                valid.push(true);
            }
            (Cells::Float { vals, valid }, None) => {
                // `NaN` and `Infinity` are values Postgres float columns really hold, and Rust
                // parses both spellings, so they cross intact.
                let f = std::str::from_utf8(b)
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| format!("column `{name}`: `{}` is not a number", String::from_utf8_lossy(b)))?;
                vals.push(f);
                valid.push(true);
            }
            (Cells::Float { vals, valid }, Some(_)) => {
                let &[a, c, d, e, f, g, h, i] = b else {
                    return Err(format!("column `{name}`: a binary float of {} bytes", b.len()));
                };
                let f = f64::from_be_bytes([a, c, d, e, f, g, h, i]);
                // Text has one NaN; binary carries a sign and a payload. One value, one spelling.
                vals.push(if f.is_nan() { f64::NAN } else { f });
                valid.push(true);
            }
            (Cells::Bool { vals, valid }, None) => {
                vals.push(match b {
                    b"t" | b"true" | b"TRUE" => true,
                    b"f" | b"false" | b"FALSE" => false,
                    other => {
                        return Err(format!(
                            "column `{name}`: `{}` is not a boolean",
                            String::from_utf8_lossy(other)
                        ))
                    }
                });
                valid.push(true);
            }
            (Cells::Bool { vals, valid }, Some(_)) => {
                vals.push(match *b {
                    [0] => false,
                    [1] => true,
                    _ => return Err(format!("column `{name}`: a binary boolean that is neither 0 nor 1")),
                });
                valid.push(true);
            }
            (Cells::Text(t), _) => {
                let s = std::str::from_utf8(b)
                    .map_err(|_| format!("column `{name}` holds bytes that are not UTF-8"))?;
                t.push_str(s);
            }
        }
        Ok(())
    }

    /// Convert to a Helix column.
    pub fn finish(self) -> ColData {
        match self.cells {
            Cells::Int { vals, valid } => ColData::IntValid(vals, valid),
            Cells::Float { vals, valid } => ColData::FloatValid(vals, valid),
            Cells::Bool { vals, valid } => {
                // `ColData` has no nullable boolean, so a column carrying NULL cannot be
                // a Bool column without inventing a value for the null. Rather than pick
                // one, such a column reads as text — `"t"` / `"f"` / missing, the server's own
                // spelling — which is lossless and visibly a string. Adding a nullable Bool
                // is the real fix and belongs with both backends, not smuggled in here.
                if valid.iter().all(|ok| *ok) {
                    ColData::Bool(vals)
                } else {
                    let mut t = StrBuilder::with_capacity(vals.len());
                    for (v, ok) in vals.iter().zip(&valid) {
                        match (ok, v) {
                            (false, _) => t.push_missing(),
                            (true, true) => t.push_str("t"),
                            (true, false) => t.push_str("f"),
                        }
                    }
                    ColData::StrBuilt(t)
                }
            }
            Cells::Text(t) => ColData::StrBuilt(t),
        }
    }
}

/// A decimal integer as the server prints one, read from its bytes. Accumulated NEGATIVELY so
/// that `-9223372036854775808` — which has no positive twin — parses; overflow is `None`.
fn parse_int(b: &[u8]) -> Option<i64> {
    let (negative, digits) = match b.split_first()? {
        (b'-', rest) => (true, rest),
        (b'+', rest) => (false, rest),
        _ => (false, b),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for &c in digits {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_sub(i64::from(d))?;
    }
    if negative { Some(n) } else { n.checked_neg() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_integer_reads_from_its_bytes_exactly_as_the_standard_parser_reads_its_text() {
        for s in [
            "0", "-0", "7", "-7", "+7", "42", "2147483647", "-2147483648", "9223372036854775807",
            "-9223372036854775808", "9223372036854775808", "-9223372036854775809", "", "-", "+", "1a",
            " 1", "1 ", "--1", "99999999999999999999", "0007",
        ] {
            assert_eq!(parse_int(s.as_bytes()), s.parse::<i64>().ok(), "`{s}`");
        }
    }

    fn one(oid: i32, binary: Option<usize>, cells: &[Option<&[u8]>]) -> Result<ColData, String> {
        let mut c = ColBuf::with_format("c".to_string(), oid, binary);
        for v in cells {
            c.push(*v)?;
        }
        Ok(c.finish())
    }

    /// The two encodings are ONE value: whatever a column holds, reading it as text and
    /// reading it in binary build the same column — which is what lets a statement's first
    /// run and its later ones differ in format and in nothing else.
    #[test]
    fn a_binary_cell_is_the_value_its_text_is() {
        let ints: [i64; 7] = [0, 1, -1, 32767, -32768, 2147483647, -2147483648];
        for (oid, width) in [(INT2, 2usize), (INT4, 4), (INT8, 8)] {
            for n in ints.iter().copied().chain([i64::MAX, i64::MIN]) {
                let fits = match width {
                    2 => i16::try_from(n).is_ok(),
                    4 => i32::try_from(n).is_ok(),
                    _ => true,
                };
                if !fits {
                    continue;
                }
                let bin = n.to_be_bytes();
                let text = n.to_string();
                let a = one(oid, None, &[Some(text.as_bytes()), None]).unwrap();
                let b = one(oid, Some(width), &[Some(&bin[8 - width..]), None]).unwrap();
                match (a, b) {
                    (ColData::IntValid(av, ao), ColData::IntValid(bv, bo)) => {
                        assert_eq!((av, ao), (bv, bo), "oid {oid}, {n}")
                    }
                    _ => panic!("an integer column is `IntValid`"),
                }
            }
        }
        // Floats: the shortest round-trip text PostgreSQL 12+ prints, against the bits.
        for f in [0.0f64, -0.0, 1.5, 0.1, 1e300, 5e-324, f64::MAX, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, -f64::NAN] {
            let text = if f.is_nan() {
                "NaN".to_string()
            } else if f.is_infinite() {
                if f > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() }
            } else {
                format!("{f:?}")
            };
            let a = one(FLOAT8, None, &[Some(text.as_bytes())]).unwrap();
            let b = one(FLOAT8, Some(8), &[Some(&f.to_be_bytes())]).unwrap();
            match (a, b) {
                (ColData::FloatValid(av, _), ColData::FloatValid(bv, _)) => {
                    assert_eq!(av[0].to_bits(), bv[0].to_bits(), "{text}")
                }
                _ => panic!("a float column is `FloatValid`"),
            }
        }
        for (text, bin, want) in [(&b"t"[..], [1u8], true), (&b"f"[..], [0u8], false)] {
            let a = one(BOOL, None, &[Some(text)]).unwrap();
            let b = one(BOOL, Some(1), &[Some(&bin)]).unwrap();
            assert!(matches!((a, b), (ColData::Bool(x), ColData::Bool(y)) if x == [want] && y == [want]));
        }
    }

    /// Only the five types whose binary IS their text's value are asked for in binary, and
    /// `float8` only from a server whose text is exact.
    #[test]
    fn binary_is_asked_for_only_where_it_is_the_same_value() {
        assert_eq!(binary_width(INT2, true), Some(2));
        assert_eq!(binary_width(INT4, false), Some(4));
        assert_eq!(binary_width(INT8, false), Some(8));
        assert_eq!(binary_width(BOOL, false), Some(1));
        assert_eq!(binary_width(FLOAT8, true), Some(8));
        assert_eq!(binary_width(FLOAT8, false), None, "before PostgreSQL 12 float text is rounded");
        assert_eq!(binary_width(FLOAT4, true), None, "`1.1`, not the 32-bit float widened");
        assert_eq!(binary_width(NUMERIC, true), None);
        assert_eq!(binary_width(25, true), None, "text");
        assert_eq!(binary_width(2950, true), None, "uuid reads as the text the server prints");
    }

    /// A value that is not its column's type is an error naming both — in either format.
    #[test]
    fn a_cell_that_is_not_its_type_is_an_error_naming_the_column_and_the_value() {
        assert_eq!(one(INT4, None, &[Some(b"abc")]).err().unwrap(), "column `c`: `abc` is not an integer");
        assert_eq!(one(FLOAT8, None, &[Some(b"x1")]).err().unwrap(), "column `c`: `x1` is not a number");
        assert_eq!(one(BOOL, None, &[Some(b"maybe")]).err().unwrap(), "column `c`: `maybe` is not a boolean");
        assert_eq!(one(25, None, &[Some(&[0xff, 0xfe])]).err().unwrap(), "column `c` holds bytes that are not UTF-8");
        assert_eq!(
            one(INT4, Some(4), &[Some(&[0, 0, 1])]).err().unwrap(),
            "column `c`: the server sent 3 bytes for a 4-byte binary value"
        );
        assert_eq!(
            one(BOOL, Some(1), &[Some(&[2])]).err().unwrap(),
            "column `c`: a binary boolean that is neither 0 nor 1"
        );
    }

    /// A boolean column holding a NULL still reads as the server's own text, as before.
    #[test]
    fn a_boolean_column_with_a_null_reads_as_text() {
        for binary in [None, Some(1)] {
            let (t, f): (&[u8], &[u8]) = if binary.is_some() { (&[1], &[0]) } else { (b"t", b"f") };
            let ColData::StrBuilt(b) = one(BOOL, binary, &[Some(t), None, Some(f)]).unwrap() else {
                panic!("a nullable boolean is text")
            };
            let (dict, codes, valid) = b.into_parts();
            let cells: Vec<Option<&str>> =
                codes.iter().zip(&valid).map(|(c, ok)| ok.then(|| dict[*c as usize].as_str())).collect();
            assert_eq!(cells, [Some("t"), None, Some("f")]);
        }
    }
}

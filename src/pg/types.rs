//! PostgreSQL type OIDs → Helix column data.
//!
//! Results are requested in TEXT format, so every value arrives as the string the server
//! would print. That is a deliberate choice, not a shortcut:
//!
//!   * It makes the unknown-type case TOTAL. A column this table has never heard of —
//!     `uuid`, `jsonb`, `tsrange`, an extension type, a domain over any of them — still
//!     reads, as the text the server produced. That is ADR 0033 Stage 2's rule for
//!     foreign parquet dtypes, applied for the same reason: refusing a column because
//!     the reader lacks an opinion about it is worse than handing back what it says.
//!   * Binary format would need a decoder per OID and a new failure mode per decoder,
//!     for a saving that is invisible next to the network round trip.
//!
//! The types below are the ones with a Helix EQUIVALENT, so they become numbers and
//! booleans rather than strings. Everything else is text, and says so in `describe`.

use crate::backend::ColData;

// From `pg_type.h`; these OIDs are fixed by the catalog and have not moved in decades.
const BOOL: i32 = 16;
const INT8: i32 = 20;
const INT2: i32 = 21;
const INT4: i32 = 23;
const FLOAT4: i32 = 700;
const FLOAT8: i32 = 701;
const NUMERIC: i32 = 1700;

/// How a column's text values should be read.
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

/// One column's worth of text values, accumulated as they arrive.
pub struct ColBuf {
    pub name: String,
    kind: Kind,
    vals: Vec<Option<String>>,
}

impl ColBuf {
    pub fn new(name: String, oid: i32) -> ColBuf {
        ColBuf { name, kind: kind_of(oid), vals: Vec::new() }
    }

    pub fn push(&mut self, v: Option<&[u8]>) -> Result<(), String> {
        match v {
            None => self.vals.push(None),
            Some(b) => {
                let s = std::str::from_utf8(b)
                    .map_err(|_| format!("column `{}` holds bytes that are not UTF-8", self.name))?;
                self.vals.push(Some(s.to_string()));
            }
        }
        Ok(())
    }

    /// Convert to a Helix column.
    ///
    /// A value the server sent that does not parse as its own declared type is an ERROR
    /// naming the column and the value, never a silent `missing` — the two are different
    /// claims, and ADR 0001 reserves `missing` for absence.
    pub fn finish(self) -> Result<ColData, String> {
        let ColBuf { name, kind, vals } = self;
        Ok(match kind {
            Kind::Int => {
                let mut out = Vec::with_capacity(vals.len());
                for v in &vals {
                    out.push(match v {
                        None => None,
                        Some(s) => Some(s.parse::<i64>().map_err(|_| {
                            format!("column `{name}`: `{s}` is not an integer")
                        })?),
                    });
                }
                ColData::IntOpt(out)
            }
            Kind::Float => {
                let mut out = Vec::with_capacity(vals.len());
                for v in &vals {
                    out.push(match v {
                        None => None,
                        // `NaN` and `Infinity` are values Postgres float columns really
                        // hold, and Rust parses both spellings, so they cross intact.
                        Some(s) => Some(s.parse::<f64>().map_err(|_| {
                            format!("column `{name}`: `{s}` is not a number")
                        })?),
                    });
                }
                ColData::Float(out)
            }
            Kind::Bool => {
                // `ColData` has no nullable boolean, so a column carrying NULL cannot be
                // a Bool column without inventing a value for the null. Rather than pick
                // one, such a column reads as text — `"t"` / `"f"` / missing — which is
                // lossless and visibly a string. Adding `ColData::BoolOpt` is the real
                // fix and belongs with both backends, not smuggled in here.
                if vals.iter().any(|v| v.is_none()) {
                    ColData::StrOpt(vals)
                } else {
                    let mut out = Vec::with_capacity(vals.len());
                    for v in &vals {
                        let s = v.as_deref().unwrap_or("");
                        out.push(match s {
                            "t" | "true" | "TRUE" => true,
                            "f" | "false" | "FALSE" => false,
                            other => {
                                return Err(format!("column `{name}`: `{other}` is not a boolean"))
                            }
                        });
                    }
                    ColData::Bool(out)
                }
            }
            Kind::Text => ColData::StrOpt(vals),
        })
    }
}

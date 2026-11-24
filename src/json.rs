//! JSON ↔ Helix values, for consuming and producing API / data payloads.
//!
//! `parse_json(str)` turns a JSON string into Helix values — object → record,
//! array → array, string → String, integer → Int, fractional → Float, `true`/
//! `false` → Bool, `null` → `missing`. `to_json(value)` does the reverse. Pure
//! compute, no network, always available.

use std::rc::Rc;

use crate::symbol::Symbol;
use crate::value::Value;

/// Upper bound on the number of Helix values a single `parse_json` may
/// materialize — matches the interpreter's other collection caps (range, tensor).
/// serde already bounds nesting depth (128 levels), so this guards total width:
/// a multi-GB array/object can't silently turn into an out-of-memory abort.
const MAX_ELEMENTS: usize = 100_000_000;

/// Parse a JSON string into a Helix value.
pub fn parse(s: &str) -> Result<Value, String> {
    let v: serde_json::Value = serde_json::from_str(s).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut budget = MAX_ELEMENTS;
    from_serde(v, &mut budget)
}

/// Convert one serde node, decrementing `budget` per Helix value produced and
/// failing cleanly once the document would exceed [`MAX_ELEMENTS`] (instead of
/// allocating until the allocator aborts).
fn from_serde(v: serde_json::Value, budget: &mut usize) -> Result<Value, String> {
    use serde_json::Value as J;
    if *budget == 0 {
        return Err(format!("JSON has too many elements (limit {MAX_ELEMENTS})"));
    }
    *budget -= 1;
    Ok(match v {
        J::Null => Value::Missing,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        J::String(s) => Value::Str(Rc::new(s)),
        J::Array(xs) => {
            let mut out = Vec::with_capacity(xs.len().min(*budget));
            for x in xs {
                out.push(from_serde(x, budget)?);
            }
            Value::array(out)
        }
        // JSON objects become records; keys are arbitrary strings (identifier keys
        // are reachable as `r.field`). Keys are interned, so cap distinct keys too —
        // a file of millions of unique keys would otherwise leak the interner.
        J::Object(map) => {
            let mut fields = Vec::with_capacity(map.len().min(*budget));
            for (k, v) in map {
                let key = Symbol::try_intern(&k).ok_or_else(|| {
                    "JSON has too many distinct field names to intern".to_string()
                })?;
                fields.push((key, from_serde(v, budget)?));
            }
            Value::record(fields)
        }
    })
}

/// Why [`stringify`] produced no text.
#[derive(Debug)]
pub(crate) enum Refused {
    /// The text would pass the one-string cap (`MAX_STRING_LEN`).
    TooLong,
    /// The allocator refused to grow the text to this many bytes.
    NoMemory(usize),
    /// The value cannot be JSON (a function, say), in the serializer's words.
    Other(String),
}

/// Serialize a Helix value to a JSON string.
///
/// Written straight into one buffer with a guarded walk ([`crate::deep::nested`]). It used
/// to build a `serde_json::Value` tree and serialize that, and every stage of that recursed
/// once per level — building, serializing and freeing — so `to_json` of a deeply nested
/// value killed the process. The bytes are serde_json's own: each scalar goes through
/// `serde_json` (its number formatting and string escaping), objects are emitted with keys
/// in the order `serde_json::Map` (a `BTreeMap` in every build — no crate enables
/// `preserve_order`) iterates them, and where two dict keys spell the same string the later
/// one wins, as an insert into that map would. `stringify_matches_the_serde_route` below
/// checks it against the old route.
///
/// The text is held to the one-string cap AS IT IS WRITTEN ([`Capped`]): escaping can grow a
/// string sixfold, and a value that holds one sub-value many times prints exponentially, so
/// the size is not knowable up front (ADR 0024).
pub(crate) fn stringify(v: &Value) -> Result<String, Refused> {
    let mut out = Capped { buf: Vec::new(), refused: None };
    match write_json(&mut out, v) {
        // Every byte came from `serde_json` or is ASCII punctuation, so the check passes.
        Ok(()) => String::from_utf8(out.buf).map_err(|e| Refused::Other(format!("could not serialize to JSON: {e}"))),
        Err(e) => Err(out.refused.take().unwrap_or(Refused::Other(e))),
    }
}

/// A `Vec<u8>` writer that refuses the write that would pass `MAX_STRING_LEN`, and a growth
/// the allocator refuses, freeing what it holds — the caller only reports the refusal.
struct Capped {
    buf: Vec<u8>,
    refused: Option<Refused>,
}

impl std::io::Write for Capped {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let need = self.buf.len().saturating_add(data.len());
        if need > crate::interp::MAX_STRING_LEN {
            self.refused = Some(Refused::TooLong);
        } else if need > self.buf.capacity() {
            let to = crate::interp::growth::capped_capacity(self.buf.capacity(), need);
            if self.buf.try_reserve_exact(to - self.buf.len()).is_err() {
                self.refused = Some(Refused::NoMemory(to));
            }
        }
        if self.refused.is_some() {
            self.buf = Vec::new();
            return Err(std::io::Error::other("JSON text refused"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Punctuation, through the cap like everything else.
fn put(out: &mut Capped, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    out.write_all(bytes).map_err(|e| e.to_string())
}

fn json_leaf<T: serde::Serialize + ?Sized>(out: &mut Capped, x: &T) -> Result<(), String> {
    serde_json::to_writer(out, x).map_err(|e| format!("could not serialize to JSON: {e}"))
}

fn write_json(out: &mut Capped, v: &Value) -> Result<(), String> {
    match v {
        Value::Missing => put(out, b"null")?,
        Value::Bool(b) => put(out, if *b { b"true" } else { b"false" })?,
        Value::Int(i) => json_leaf(out, i)?,
        // JSON has no NaN/Infinity — they serialize as `null` (the JSON convention, and
        // what serde_json writes for a non-finite `f64`).
        Value::Float(f) => json_leaf(out, f)?,
        // JSON has no rational type. Integer-valued ratios serialize as integers;
        // others as their nearest f64 (matching how Float serializes).
        Value::Rational(r) => {
            use num_traits::ToPrimitive;
            if r.is_integer() {
                match r.to_i64() {
                    Some(i) => json_leaf(out, &i)?,
                    None => put(out, b"null")?,
                }
            } else {
                match r.to_f64() {
                    Some(f) => json_leaf(out, &f)?,
                    None => put(out, b"null")?,
                }
            }
        }
        Value::Str(s) | Value::Dna(s) => json_leaf(out, s.as_str())?,
        Value::Array(xs) => write_json_array(out, &xs.to_values())?,
        Value::Tuple(xs) => write_json_array(out, xs)?,
        Value::Record(fields) => {
            let mut entries: Vec<(&str, &Value)> = fields.iter().map(|(k, val)| (k.as_str(), val)).collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            write_json_object(out, entries.into_iter())?;
        }
        // A Dict serializes as a JSON object — the natural way to build a dynamic-keyed
        // payload (`{ properties: params.to_dict() }.to_json()`). JSON object keys must be
        // strings, so a non-string dict key is stringified.
        Value::Dict(map) => {
            let map = map.map();
            let mut entries: Vec<(String, &Value)> = map
                .iter()
                .map(|(k, val)| {
                    let key = match k {
                        crate::value::DictKey::Str(s) | crate::value::DictKey::Dna(s) => (**s).clone(),
                        crate::value::DictKey::Int(i) => i.to_string(),
                        crate::value::DictKey::Bool(b) => b.to_string(),
                    };
                    (key, val)
                })
                .collect();
            // Stable, so keys that stringify alike keep the map's order; the last of
            // each run is the one a map insert would have kept.
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut kept: Vec<(&str, &Value)> = Vec::with_capacity(entries.len());
            for (k, val) in &entries {
                match kept.last_mut() {
                    Some(last) if last.0 == k.as_str() => last.1 = val,
                    _ => kept.push((k.as_str(), val)),
                }
            }
            write_json_object(out, kept.into_iter())?;
        }
        other => return Err(format!("can't serialize {} to JSON", crate::value::with_article(other.type_name()))),
    }
    Ok(())
}

fn write_json_array(out: &mut Capped, xs: &[Value]) -> Result<(), String> {
    put(out, b"[")?;
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            put(out, b",")?;
        }
        crate::deep::nested(|| write_json(out, x))?;
    }
    put(out, b"]")
}

fn write_json_object<'a>(out: &mut Capped, entries: impl Iterator<Item = (&'a str, &'a Value)>) -> Result<(), String> {
    put(out, b"{")?;
    for (i, (k, val)) in entries.enumerate() {
        if i > 0 {
            put(out, b",")?;
        }
        json_leaf(out, k)?;
        put(out, b":")?;
        crate::deep::nested(|| write_json(out, val))?;
    }
    put(out, b"}")
}

/// The route `stringify` took before it wrote JSON directly — kept, test-only, as the
/// oracle that the direct writer produces the same bytes.
#[cfg(test)]
fn to_serde(v: &Value) -> Result<serde_json::Value, String> {
    use serde_json::Value as J;
    Ok(match v {
        Value::Missing => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
        Value::Rational(r) => {
            use num_traits::ToPrimitive;
            if r.is_integer() {
                r.to_i64().map(|i| J::Number(i.into())).unwrap_or(J::Null)
            } else {
                r.to_f64().and_then(serde_json::Number::from_f64).map(J::Number).unwrap_or(J::Null)
            }
        }
        Value::Str(s) => J::String((**s).clone()),
        Value::Dna(s) => J::String((**s).clone()),
        Value::Array(xs) => J::Array(xs.to_values().iter().map(to_serde).collect::<Result<_, _>>()?),
        Value::Tuple(xs) => J::Array(xs.iter().map(to_serde).collect::<Result<_, _>>()?),
        Value::Record(fields) => {
            let mut m = serde_json::Map::new();
            for (k, val) in fields.iter() {
                m.insert(k.as_str().to_string(), to_serde(val)?);
            }
            J::Object(m)
        }
        Value::Dict(map) => {
            let map = map.map();
            let mut m = serde_json::Map::new();
            for (k, val) in map.iter() {
                let key = match k {
                    crate::value::DictKey::Str(s) | crate::value::DictKey::Dna(s) => (**s).clone(),
                    crate::value::DictKey::Int(i) => i.to_string(),
                    crate::value::DictKey::Bool(b) => b.to_string(),
                };
                m.insert(key, to_serde(val)?);
            }
            J::Object(m)
        }
        other => return Err(format!("can't serialize {} to JSON", crate::value::with_article(other.type_name()))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The direct writer must produce the bytes the serde_json route produced, for every
    /// shape: escapes and non-ASCII in strings and keys, every float class, both integer
    /// extremes, unsorted record fields, dict keys of each kind — including keys that
    /// stringify alike (`1` and `"1"`), where the later one must win as a map insert did.
    #[test]
    fn stringify_matches_the_serde_route() {
        use crate::value::{Dict, DictKey};
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let strs = ["", "a", "\"q\"", "back\\slash", "nl\nt\tab", "\u{1}\u{1f}", "é", "日本", "😀", "</script>"];
        let floats = [0.0, -0.0, 1.0, -1.5, 1e300, 1e-300, 0.1, 1e21, 1e-7, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MIN_POSITIVE];
        fn make(next: &mut dyn FnMut() -> u64, depth: u32, strs: &[&str], floats: &[f64]) -> Value {
            let pick = |n: u64, m: usize| (n % m as u64) as usize;
            let k = if depth == 0 { next() % 5 } else { next() % 10 };
            match k {
                0 => Value::Missing,
                1 => Value::Bool(next() % 2 == 0),
                2 => Value::Int(match next() % 4 { 0 => i64::MIN, 1 => i64::MAX, 2 => 0, _ => next() as i64 }),
                3 => Value::Float(floats[pick(next(), floats.len())]),
                4 => Value::Str(Rc::new(strs[pick(next(), strs.len())].to_string())),
                5 | 6 => {
                    let n = next() % 4;
                    Value::array((0..n).map(|_| make(next, depth - 1, strs, floats)).collect())
                }
                7 => {
                    let n = next() % 4;
                    Value::tuple((0..n).map(|_| make(next, depth - 1, strs, floats)).collect())
                }
                8 => {
                    // Field names drawn so that declaration order is rarely sorted.
                    let names = ["zeta", "a", "M", "b2", "é", "_", "a\"q"];
                    let mut fields: Vec<(Symbol, Value)> = Vec::new();
                    for _ in 0..next() % 5 {
                        let name = names[pick(next(), names.len())];
                        if !fields.iter().any(|(k, _)| k.as_str() == name) {
                            fields.push((Symbol::intern(name), make(next, depth - 1, strs, floats)));
                        }
                    }
                    Value::record(fields)
                }
                _ => {
                    let mut m = std::collections::BTreeMap::new();
                    for _ in 0..next() % 6 {
                        let key = match next() % 5 {
                            0 => DictKey::Int(1),
                            1 => DictKey::Str(Rc::new("1".to_string())),
                            2 => DictKey::Bool(true),
                            3 => DictKey::Str(Rc::new("true".to_string())),
                            _ => DictKey::Dna(Rc::new("ACGT".to_string())),
                        };
                        m.insert(key, make(next, depth - 1, strs, floats));
                    }
                    if next() % 3 == 0 {
                        m.insert(DictKey::Str(Rc::new("ACGT".to_string())), Value::Int(7));
                    }
                    Value::Dict(Rc::new(Dict::from_map(m)))
                }
            }
        }
        for _ in 0..20_000 {
            let v = make(&mut next, 4, &strs, &floats);
            let old = to_serde(&v).map(|j| serde_json::to_string(&j).unwrap());
            let new = stringify(&v).map_err(|e| match e {
                Refused::Other(m) => m,
                other => panic!("refused {other:?} for {v}"),
            });
            assert_eq!(new, old, "for {v}");
        }
    }

    #[test]
    fn parse_maps_json_onto_helix_values() {
        let v = parse(r#"{"name":"Ada","age":41,"tags":["x","y"],"ok":true,"note":null}"#).unwrap();
        let Value::Record(fields) = &v else { panic!("expected a record") };
        assert_eq!(fields.len(), 5);
        // Spot-check the field types.
        let get = |k: &str| fields.iter().find(|(n, _)| n.as_str() == k).map(|(_, v)| v).unwrap();
        assert!(matches!(get("name"), Value::Str(s) if s.as_str() == "Ada"));
        assert!(matches!(get("age"), Value::Int(41)));
        assert!(matches!(get("ok"), Value::Bool(true)));
        assert!(matches!(get("note"), Value::Missing));
        assert!(matches!(get("tags"), Value::Array(a) if a.len() == 2));
    }

    #[test]
    fn scalars_and_nulls() {
        assert!(matches!(parse("42").unwrap(), Value::Int(42)));
        assert!(matches!(parse("3.5").unwrap(), Value::Float(_)));
        assert!(matches!(parse("true").unwrap(), Value::Bool(true)));
        assert!(matches!(parse("null").unwrap(), Value::Missing));
        assert!(matches!(parse("\"hi\"").unwrap(), Value::Str(_)));
    }

    #[test]
    fn stringify_is_the_inverse() {
        let s = r#"{"a":[1,2,3],"b":"x","c":null,"d":true}"#;
        let v = parse(s).unwrap();
        let out = stringify(&v).unwrap();
        // Re-parse to compare structurally (key order is not significant).
        assert_eq!(stringify(&parse(&out).unwrap()).unwrap(), out);
        assert!(out.contains("\"a\":[1,2,3]"));
        assert_eq!(stringify(&Value::Missing).unwrap(), "null");
    }

    #[test]
    fn non_data_values_are_rejected() {
        assert!(stringify(&Value::Unit).is_err());
    }

    /// **Every finite Float survives `to_json` -> `parse_json` BIT-IDENTICALLY.**
    ///
    /// This is a property, not a battery, because a battery is exactly what missed it.
    /// Until `float_roundtrip` was enabled in Cargo.toml, serde_json parsed with a fast
    /// best-effort path allowed to land one ULP away, and
    /// `(-0.21453773034276893).to_json().parse_json()` answered `-0.2145377303427689`
    /// — unequal to its own source literal. `0.1`, `0.2`, `0.3`, `pi` and `e` all
    /// round-tripped fine, so any hand-picked list of "hard" values was likely to pass.
    ///
    /// The direction of the bug is worth keeping written down: **serialization was
    /// never wrong** (`to_json` emits the shortest round-trip spelling) and **Helix's
    /// own parser was never wrong** (`to_float` on the same 17-digit string was exact).
    /// Only the JSON read path lost the bit, which is why "the JSON layer is lossy" was
    /// too coarse a diagnosis to act on and "serde_json parses without
    /// `float_roundtrip`" was the actionable one.
    #[test]
    fn every_finite_float_survives_a_json_round_trip() {
        // A deterministic xorshift over BIT PATTERNS, not over values: sampling
        // `random::<f64>()` would only ever visit [0, 1), where the fast path is at its
        // most accurate. Bit patterns reach the exponent range where it is not.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        // The values that are hard for a reason, plus the one that actually failed.
        let corners: Vec<f64> = vec![
            0.0,
            -0.0,
            0.1,
            0.2,
            0.3,
            1.0 / 3.0,
            -0.21453773034276893,
            9007199254740993.0,
            9007199254740992.0,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            f64::from_bits(1),          // smallest subnormal
            f64::from_bits(0x000F_FFFF_FFFF_FFFF), // largest subnormal
            std::f64::consts::PI,
            std::f64::consts::E,
            1.2345678901234567,
            1e-100,
            1e100,
            -1e-300,
        ];

        let mut checked = 0usize;
        let mut cases: Vec<f64> = corners;
        while cases.len() < 20_000 {
            let x = f64::from_bits(next());
            // NaN and the infinities are not JSON numbers — `to_serde` answers `null`
            // for them by design, and that is a separate decision from precision.
            if x.is_finite() {
                cases.push(x);
            }
        }

        for x in cases {
            let json = stringify(&Value::Float(x)).unwrap();
            let back = parse(&json).unwrap();
            let y = match back {
                Value::Float(y) => y,
                // A float whose shortest spelling has no fractional part parses back as
                // an Int (`2.0` -> `2`), which is `from_serde`'s documented Int-first
                // rule and not a precision question. Compare through f64 in that case.
                Value::Int(i) => i as f64,
                other => panic!("{x:?} came back as {other:?} from {json}"),
            };
            assert_eq!(
                x.to_bits(),
                y.to_bits(),
                "round-trip changed the bits: {x:?} -> {json} -> {y:?} \
                 (is `float_roundtrip` still enabled for serde_json in Cargo.toml?)"
            );
            checked += 1;
        }
        assert!(checked >= 20_000, "the property ran on too few values: {checked}");
    }
}

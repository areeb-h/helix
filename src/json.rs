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
            Value::Record(Rc::new(fields))
        }
    })
}

/// Serialize a Helix value to a JSON string.
pub fn stringify(v: &Value) -> Result<String, String> {
    let j = to_serde(v)?;
    serde_json::to_string(&j).map_err(|e| format!("could not serialize to JSON: {e}"))
}

fn to_serde(v: &Value) -> Result<serde_json::Value, String> {
    use serde_json::Value as J;
    Ok(match v {
        Value::Missing => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::Number((*i).into()),
        // JSON has no NaN/Infinity — they serialize as `null` (the JSON convention).
        Value::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
        Value::Str(s) => J::String((**s).clone()),
        Value::Dna(s) => J::String((**s).clone()),
        Value::Array(xs) => {
            J::Array(xs.to_values().iter().map(to_serde).collect::<Result<_, _>>()?)
        }
        Value::Tuple(xs) => {
            J::Array(xs.iter().map(to_serde).collect::<Result<_, _>>()?)
        }
        Value::Record(fields) => {
            let mut m = serde_json::Map::new();
            for (k, val) in fields.iter() {
                m.insert(k.as_str().to_string(), to_serde(val)?);
            }
            J::Object(m)
        }
        other => return Err(format!("can't serialize a {} to JSON", other.type_name())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

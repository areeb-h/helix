//! Dict and Record methods — moved verbatim from the one-file methods module (2026-08-24).

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use std::rc::Rc;

/// Methods on a [`Value::Dict`] (ADR 0020). Lookups (`get`/`contains`) are O(log n);
/// enumeration (`keys`/`values`/`items`) is sorted by key, so output is deterministic.
/// `insert`/`remove` are immutable — they return a new dict (the map is cloned, so a
/// one-shot update is O(n); build in bulk with `pairs.to_dict()` for O(n log n)).
pub(crate) fn dict_method(
    map: &Rc<std::collections::BTreeMap<crate::value::DictKey, Value>>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    use crate::value::DictKey;
    let arity = |n: usize| -> Result<(), HelixError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(HelixError::new(
                format!(
                    "`{}` expects {} argument{}, got {}",
                    name,
                    n,
                    if n == 1 { "" } else { "s" },
                    args.len()
                ),
                line,
                col,
            ))
        }
    };
    let key_of = |v: &Value| DictKey::from_value(v).map_err(|m| HelixError::new(m, line, col));
    match name {
        // `get(k)` → the value, or `missing` when absent (so `d.get(k) ?? default` works).
        "get" => {
            arity(1)?;
            Ok(map.get(&key_of(&args[0])?).cloned().unwrap_or(Value::Missing))
        }
        // `expect(k)` → the value, RAISING on absence — the loud companion to `get`.
        // ADR 0001 keeps `get`/`d[k]` answering `missing` (an absent value is a condition
        // in the data); `expect` is for when absence would be a mistake in the PROGRAM,
        // and it raises at the lookup — before a `missing` is minted and laundered
        // through arithmetic into a number-shaped hole nothing downstream can trace.
        "expect" => {
            arity(1)?;
            let k = key_of(&args[0])?;
            match map.get(&k) {
                Some(v) => Ok(v.clone()),
                None => {
                    let e = HelixError::new(
                        format!(
                            "key `{}` not found in this dict ({} key{})",
                            args[0],
                            map.len(),
                            if map.len() == 1 { "" } else { "s" }
                        ),
                        line,
                        col,
                    );
                    // One-edit did-you-mean over the dict's OWN string keys — the house
                    // policy (a wrong suggestion is worse than silence). Non-string keys
                    // can't typo by spelling, so they never suggest.
                    let near = match &args[0] {
                        Value::Str(want) => map
                            .keys()
                            .filter_map(|c| match c.to_value() {
                                Value::Str(s) => crate::error::typo_distance(want, &s)
                                    .map(|d| (d, (*s).clone())),
                                _ => None,
                            })
                            .min_by_key(|(d, _)| *d)
                            .map(|(_, s)| s),
                        _ => None,
                    };
                    Err(match near {
                        Some(s) => e.hint(format!("did you mean `{s}`?")),
                        None => e.hint(
                            "`.has(k)` checks presence; `.get(k)` answers `missing` \
                             instead of raising, so `.get(k) ?? default` supplies a \
                             fallback.",
                        ),
                    })
                }
            }
        }
        // `has` is the alias that matches a record's `has` — the same key-presence question,
        // one name across both keyed types.
        "contains" | "has" => {
            arity(1)?;
            Ok(Value::Bool(map.contains_key(&key_of(&args[0])?)))
        }
        "count" | "length" => {
            arity(0)?;
            Ok(Value::Int(map.len() as i64))
        }
        "keys" => {
            arity(0)?;
            Ok(Value::array(map.keys().map(|k| k.to_value()).collect()))
        }
        "values" => {
            arity(0)?;
            Ok(Value::array(map.values().cloned().collect()))
        }
        // `(key, value)` tuples, sorted by key — round-trips through `to_dict`.
        "items" => {
            arity(0)?;
            Ok(Value::array(
                map.iter()
                    .map(|(k, v)| Value::Tuple(Rc::new(vec![k.to_value(), v.clone()])))
                    .collect(),
            ))
        }
        "insert" => {
            arity(2)?;
            let k = key_of(&args[0])?;
            let mut new = (**map).clone();
            new.insert(k, args[1].clone());
            Ok(Value::Dict(Rc::new(new)))
        }
        "remove" => {
            arity(1)?;
            let k = key_of(&args[0])?;
            let mut new = (**map).clone();
            new.remove(&k);
            Ok(Value::Dict(Rc::new(new)))
        }
        _ => Err(unknown_method(
            "Dict",
            name,
            &crate::registry::methods_of(crate::registry::DICT_METHODS),
            line,
            col,
        )),
    }
}

/// Methods on a [`Value::Record`] for **dynamic** field access — the escape hatch for
/// consuming unknown-shape data (a parsed JSON API response). `get`/`has`/`keys` look fields
/// up by string name at runtime, so a maybe-absent field is `missing`/`false` rather than a
/// compile error. Static `rec.field` access is unchanged; this is for shapes you don't know
/// until runtime.
pub(crate) fn record_method(
    fields: &Rc<Vec<(crate::symbol::Symbol, Value)>>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let arity = |n: usize| -> Result<(), HelixError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(HelixError::new(
                format!("`{name}` expects {n} argument{}, got {}", if n == 1 { "" } else { "s" }, args.len()),
                line,
                col,
            ))
        }
    };
    let key = |v: &Value| -> Result<String, HelixError> {
        match v {
            Value::Str(s) => Ok((**s).clone()),
            other => Err(type_err(name, "a string field name", other, line, col)),
        }
    };
    match name {
        // `get(k)` → the field's value, or `missing` when absent (so `rec.get(k) ?? default`
        // works). `get(k, default)` → the value, or `default` when absent.
        "get" => {
            if args.len() != 1 && args.len() != 2 {
                return Err(HelixError::new(
                    format!("`get` expects 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                ));
            }
            let k = key(&args[0])?;
            let found = fields.iter().find(|(s, _)| s.as_str() == k).map(|(_, v)| v.clone());
            Ok(found.unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Missing)))
        }
        // `expect(k)` → the field's value, RAISING on absence — the record twin of the
        // dict arm above: the loud lookup for when a missing field means the PROGRAM is
        // wrong, raising before a `missing` is minted (ADR 0001's propagating default
        // stays on `get` and static access).
        "expect" => {
            arity(1)?;
            let k = key(&args[0])?;
            match fields.iter().find(|(s, _)| s.as_str() == k) {
                Some((_, v)) => Ok(v.clone()),
                None => {
                    let e = HelixError::new(
                        format!(
                            "field `{k}` not found in this record ({} field{})",
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" }
                        ),
                        line,
                        col,
                    );
                    let near = fields
                        .iter()
                        .filter_map(|(s, _)| {
                            crate::error::typo_distance(&k, s.as_str())
                                .map(|d| (d, s.as_str().to_string()))
                        })
                        .min_by_key(|(d, _)| *d)
                        .map(|(_, s)| s);
                    Err(match near {
                        Some(s) => e.hint(format!("did you mean `{s}`?")),
                        None => e.hint(
                            "`.has(k)` checks presence; `.get(k)` answers `missing` \
                             instead of raising, so `.get(k, default)` supplies a \
                             fallback.",
                        ),
                    })
                }
            }
        }
        "has" => {
            arity(1)?;
            let k = key(&args[0])?;
            Ok(Value::Bool(fields.iter().any(|(s, _)| s.as_str() == k)))
        }
        "keys" => {
            arity(0)?;
            Ok(Value::array(
                fields.iter().map(|(s, _)| Value::Str(Rc::new(s.as_str().to_string()))).collect(),
            ))
        }
        "values" => {
            arity(0)?;
            Ok(Value::array(fields.iter().map(|(_, v)| v.clone()).collect()))
        }
        "items" => {
            arity(0)?;
            Ok(Value::array(
                fields
                    .iter()
                    .map(|(s, v)| {
                        Value::Tuple(Rc::new(vec![
                            Value::Str(Rc::new(s.as_str().to_string())),
                            v.clone(),
                        ]))
                    })
                    .collect(),
            ))
        }
        // The name is a FIELD of this record, not one of the five dynamic-access methods.
        // The generic "no method" help (`get`/`has`/`keys`/`values`/`items`) is useless
        // here and actively misleading: none of them is what the author wanted. The
        // object-API spelling `r.go(3)` is what everyone writes first, and every working
        // alternative — `(r.go)(3)`, `f = r.go`, `r["go"](3)` — went unmentioned.
        _ => match fields.iter().find(|(s, _)| s.as_str() == name) {
            Some((_, held)) => {
                let e = HelixError::new(
                    format!("`{name}` is a field of this record, not a method"),
                    line,
                    col,
                );
                Err(if held.type_name() == "Function" {
                    e.hint(format!(
                        "it holds a function, so call it through the field: `(rec.{name})(…)` \
                         — or bind it first, `f = rec.{name}`, then `f(…)`."
                    ))
                } else {
                    e.hint(format!(
                        "read it without parentheses: `rec.{name}` (it holds {}).",
                        crate::value::with_article(held.type_name())
                    ))
                })
            }
            None => Err(unknown_method(
                "Record",
                name,
                &crate::registry::methods_of(crate::registry::RECORD_METHODS),
                line,
                col,
            )),
        },
    }
}


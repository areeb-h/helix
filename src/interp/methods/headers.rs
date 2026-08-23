//! Headers methods — case-insensitive reads over wire order — moved verbatim from the one-file methods module (2026-08-24).

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use std::rc::Rc;

/// Methods on a [`Value::Headers`]. Every name lookup is CASE-INSENSITIVE — that is the
/// type's whole reason to exist — and iteration answers wire order with names as they
/// arrived. `get` returns the FIRST match (the one a proxy would act on), `get_all`
/// every match in order, because a repeated name (`Set-Cookie`) is data, not a
/// collision. Header counts are capped upstream, so the linear scans here are bounded.
pub(crate) fn headers_method(
    pairs: &Rc<Vec<(String, String)>>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let want_name = |what: &str| -> Result<&str, HelixError> {
        match args {
            [Value::Str(s)] => Ok(s.as_str()),
            _ => Err(HelixError::new(
                format!("`{name}` takes {what}"),
                line,
                col,
            )
            .hint("e.g. `r.headers.get(\"Content-Type\")` — the lookup ignores case.")),
        }
    };
    match name {
        "get" => {
            let k = want_name("a header name")?;
            Ok(pairs
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| Value::Str(Rc::new(v.clone())))
                .unwrap_or(Value::Missing))
        }
        "get_all" => {
            let k = want_name("a header name")?;
            Ok(Value::array(
                pairs
                    .iter()
                    .filter(|(n, _)| n.eq_ignore_ascii_case(k))
                    .map(|(_, v)| Value::Str(Rc::new(v.clone())))
                    .collect(),
            ))
        }
        "has" | "contains" => {
            let k = want_name("a header name")?;
            Ok(Value::Bool(pairs.iter().any(|(n, _)| n.eq_ignore_ascii_case(k))))
        }
        "keys" => {
            no_args(name, args, line, col)?;
            Ok(Value::array(
                pairs.iter().map(|(n, _)| Value::Str(Rc::new(n.clone()))).collect(),
            ))
        }
        "values" => {
            no_args(name, args, line, col)?;
            Ok(Value::array(
                pairs.iter().map(|(_, v)| Value::Str(Rc::new(v.clone()))).collect(),
            ))
        }
        "items" => {
            no_args(name, args, line, col)?;
            Ok(Value::array(
                pairs
                    .iter()
                    .map(|(n, v)| {
                        Value::Tuple(Rc::new(vec![
                            Value::Str(Rc::new(n.clone())),
                            Value::Str(Rc::new(v.clone())),
                        ]))
                    })
                    .collect(),
            ))
        }
        "count" | "length" => {
            no_args(name, args, line, col)?;
            Ok(Value::Int(pairs.len() as i64))
        }
        // The escape hatch to plain-Dict land: LOWERCASED keys (the canonical form),
        // FIRST occurrence wins, matching what `get` answers. Order and repeats are
        // exactly what this conversion gives up, and taking it is the caller saying so.
        "to_dict" => {
            no_args(name, args, line, col)?;
            let mut map = std::collections::BTreeMap::new();
            for (n, v) in pairs.iter() {
                map.entry(crate::value::DictKey::Str(Rc::new(n.to_ascii_lowercase())))
                    .or_insert_with(|| Value::Str(Rc::new(v.clone())));
            }
            Ok(Value::Dict(Rc::new(map)))
        }
        _ => Err(unknown_method(
            "Headers",
            name,
            &crate::registry::methods_of(crate::registry::HEADERS_METHODS),
            line,
            col,
        )),
    }
}


//! The three strict logical operators the scalar kernel refuses (`and`/`or`
//! short-circuit in the interpreter; a column has no evaluation order to cut
//! short). Semantics are the scalar truth table, oracle-probed and pinned in
//! ADR 0034: Kleene three-valued logic with type errors taking precedence over
//! absorption (`false and 1` is an error, not `false` — exactly as on scalars).

use crate::error::HelixError;
use crate::value::Value;

fn as_bool3(v: &Value, line: usize, col: usize) -> Result<Option<bool>, HelixError> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Missing => Ok(None),
        other => Err(HelixError::new(
            format!("expected a boolean, found a value of type {}", other.type_name()),
            line,
            col,
        )),
    }
}

pub fn and(a: &Value, b: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let (x, y) = (as_bool3(a, line, col)?, as_bool3(b, line, col)?);
    Ok(match (x, y) {
        (Some(false), _) | (_, Some(false)) => Value::Bool(false),
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Missing,
    })
}

pub fn or(a: &Value, b: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let (x, y) = (as_bool3(a, line, col)?, as_bool3(b, line, col)?);
    Ok(match (x, y) {
        (Some(true), _) | (_, Some(true)) => Value::Bool(true),
        (Some(false), Some(false)) => Value::Bool(false),
        _ => Value::Missing,
    })
}

pub fn coalesce(a: &Value, b: &Value) -> Value {
    if matches!(a, Value::Missing) { b.clone() } else { a.clone() }
}

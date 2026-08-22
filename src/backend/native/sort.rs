//! Multi-key stable sort, missing first (ADR 0034 §4). Stability comes from
//! `sort_by` on a row-index permutation — equal keys keep their input order, the
//! guarantee the polars backend needed `maintain_order: true` armor to force.

use std::cmp::Ordering;

use crate::error::HelixError;
use crate::value::Value;

use super::columns::Col;
use super::NativeFrame;

pub fn sort(
    frame: &NativeFrame,
    names: &[String],
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    let key_cols: Vec<&Col> =
        names.iter().map(|n| frame.col(n, line, col)).collect::<Result<_, _>>()?;
    let mut idx: Vec<usize> = (0..frame.len()).collect();
    idx.sort_by(|&a, &b| {
        for c in &key_cols {
            let ord = cell_cmp(&c.get(a), &c.get(b));
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    Ok(frame.take(&idx))
}

/// Ascending cell order: missing first, then the value's own order. Cells in one
/// column share a dtype (the column enforced it), so the cross-type arms exist
/// only for the missing sentinel.
fn cell_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Missing, Value::Missing) => Ordering::Equal,
        (Value::Missing, _) => Ordering::Less,
        (_, Value::Missing) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

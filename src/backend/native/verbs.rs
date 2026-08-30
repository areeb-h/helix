//! The row/column verbs (ADR 0034 §2, §6, §7): filter, select, with_columns,
//! head, vstack, unique_by. Everything reduces to "compute a row-index list,
//! then `Col::take`" — which is also why every one of them is deterministic.

use crate::backend::ColExpr;
use crate::error::HelixError;
use crate::value::Value;

use super::columns::Col;
use super::eval::eval;
use super::key::RowKey;
use super::NativeFrame;

pub fn filter(
    frame: &NativeFrame,
    pred: &ColExpr,
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    // Typed fast path for `col <op> literal` — same results as the boxed
    // evaluator below, which stays the definition of the semantics.
    if let Some(keep) = super::fast::filter_keep(frame, pred, line, col) {
        return Ok(frame.take_sel(std::rc::Rc::new(keep?)));
    }
    let cells = eval(frame, pred, line, col)?.into_rows(frame.len());
    let mut keep: Vec<usize> = Vec::new();
    for (i, v) in cells.iter().enumerate() {
        match v {
            Value::Bool(true) => keep.push(i),
            Value::Bool(false) | Value::Missing => {} // missing keeps the row out
            other => {
                return Err(HelixError::new(
                    format!(
                        "a `where` predicate must be boolean, got a value of type {}",
                        other.type_name()
                    ),
                    line,
                    col,
                )
                .hint(format!("at row {i} of the frame.")))
            }
        }
    }
    Ok(frame.take(keep))
}

pub fn select(
    frame: &NativeFrame,
    names: &[String],
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    let cols: Vec<(String, Col)> = names
        .iter()
        .map(|n| frame.col(n, line, col).map(|c| (n.clone(), c.clone())))
        .collect::<Result<_, _>>()?;
    NativeFrame::new(cols, line, col)
}

pub fn with_columns(
    frame: &NativeFrame,
    cols: &[(String, ColExpr)],
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    let mut out: Vec<(String, Col)> = frame
        .columns(line, col)?
        .iter()
        .map(|(n, c)| ((*n).clone(), (*c).clone()))
        .collect();
    for (name, expr) in cols {
        // Typed arithmetic first; the boxed evaluator stays the semantics.
        let packed = if let Some(r) = super::fast::eval_typed(frame, expr, line, col) {
            r?
        } else {
            let cells = eval(frame, expr, line, col)?.into_rows(frame.len());
            Col::from_values(name, &cells, line, col)?
        };
        // Replace in place, or append (spec §7).
        match out.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = packed,
            None => out.push((name.clone(), packed)),
        }
    }
    NativeFrame::new(out, line, col)
}

pub fn head(frame: &NativeFrame, n: usize) -> NativeFrame {
    let take: Vec<usize> = (0..frame.len().min(n)).collect();
    frame.take(take)
}

pub fn tail(frame: &NativeFrame, n: usize) -> NativeFrame {
    let rows = frame.len();
    // `rows - n` would underflow when asking for more rows than exist; taking them all is
    // the same answer polars gives, and the answer a reader expects.
    let start = rows.saturating_sub(n);
    frame.take((start..rows).collect())
}

pub fn slice(frame: &NativeFrame, offset: usize, len: usize) -> NativeFrame {
    let rows = frame.len();
    let start = offset.min(rows);
    // `saturating_add` because `offset + len` can overflow `usize` on a hostile argument;
    // the clamp to `rows` then makes the window empty rather than wrapping to a huge one.
    let end = start.saturating_add(len).min(rows);
    frame.take((start..end).collect())
}

pub fn vstack(
    top: &NativeFrame,
    bottom: &NativeFrame,
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    let a = top.columns(line, col)?;
    let b = bottom.columns(line, col)?;
    let names = |cs: &[(&String, &Col)]| {
        cs.iter().map(|(n, _)| (*n).clone()).collect::<Vec<_>>()
    };
    if names(&a) != names(&b) {
        return Err(HelixError::new(
            "vstack needs both frames to have the same columns in the same order",
            line,
            col,
        )
        .hint(format!(
            "left: [{}]; right: [{}].",
            names(&a).join(", "),
            names(&b).join(", ")
        )));
    }
    // The eager dtype check (spec §6) — a strict improvement over erroring at
    // some later materialization.
    for ((n, ca), (_, cb)) in a.iter().zip(&b) {
        if !ca.same_dtype(cb) {
            return Err(HelixError::new(
                format!(
                    "vstack column `{n}` is {} on one side and {} on the other",
                    ca.dtype_name(),
                    cb.dtype_name()
                ),
                line,
                col,
            ));
        }
    }
    let cols: Vec<(String, Col)> = a
        .iter()
        .zip(&b)
        .map(|((n, ca), (_, cb))| {
            let mut cells: Vec<Value> = (0..ca.len()).map(|i| ca.get(i)).collect();
            cells.extend((0..cb.len()).map(|i| cb.get(i)));
            Col::from_values(n, &cells, line, col).map(|c| ((*n).clone(), c))
        })
        .collect::<Result<_, _>>()?;
    NativeFrame::new(cols, line, col)
}

pub fn unique_by(
    frame: &NativeFrame,
    subset: &[String],
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    use std::collections::HashMap;
    let key_cols: Vec<&Col> = if subset.is_empty() {
        frame.columns(line, col)?.into_iter().map(|(_, c)| c).collect()
    } else {
        subset.iter().map(|k| frame.col(k, line, col)).collect::<Result<_, _>>()?
    };
    let mut chosen: HashMap<RowKey, usize> = HashMap::new();
    let mut order: Vec<RowKey> = Vec::new();
    for row in 0..frame.len() {
        let key = RowKey::at(&key_cols, row);
        match chosen.get_mut(&key) {
            Some(slot) => {
                // Whole-row distinct keeps the FIRST occurrence; a key subset
                // keeps the LAST (upsert — newest wins). Spec §4.
                if !subset.is_empty() {
                    *slot = row;
                }
            }
            None => {
                chosen.insert(key.clone(), row);
                order.push(key);
            }
        }
    }
    // Output order is the KEPT rows' order (the oracle's rule): for keep-first
    // that equals first-seen order anyway; for keep-last (upsert) the surviving
    // row's position wins, so a re-appended key moves DOWN to its newest row.
    let mut keep: Vec<usize> = order.iter().map(|k| chosen[k]).collect();
    keep.sort_unstable();
    Ok(frame.take(keep))
}

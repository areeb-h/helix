//! Typed fast paths (ADR 0033 Stage 3) — the boxed evaluator in `eval.rs`
//! DEFINES the semantics; these loops only exist where they can reproduce them
//! exactly, and anything outside their shapes falls back. The differential
//! campaign and the cross-engine byte diffs guard the equivalence.
//!
//! Covered here:
//!   * `filter` on `col <op> literal` (and mirrored) for I64-vs-Int and
//!     F64-vs-Float — the exact-compare cases. Mixed Int/Float promotion keeps
//!     the kernel's own subtleties, so it falls back. A NaN delegates the cell
//!     to the kernel so the ERROR is byte-identical.
//!   * `group_agg` with ONE key column (i64 / str / bool) and an i64/f64 value
//!     column — per-group accumulation in row order, bit-matching the generic
//!     path's collect-then-fold.

// The accumulation loops index `group_of[row]` AND read the value column via a
// row-indexed closure — no single iterator carries both, so the range loops stay.
#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::BinOp;
use crate::backend::ColExpr;
use crate::error::HelixError;
use crate::value::Value;

use super::columns::Col;
use super::NativeFrame;

// ---- filter ----

/// `Some(keep)` when the predicate matches a fast shape; `None` → boxed path.
pub fn filter_keep(
    frame: &NativeFrame,
    pred: &ColExpr,
    line: usize,
    col: usize,
) -> Option<Result<Vec<usize>, HelixError>> {
    let ColExpr::Binary(op, a, b) = pred else { return None };
    if !matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne) {
        return None;
    }
    // col OP lit, or lit OP col (flip the operator's direction, not Eq/Ne).
    let (name, lit, flipped) = match (&**a, &**b) {
        (ColExpr::Col(n), ColExpr::Lit(v)) => (n, v, false),
        (ColExpr::Lit(v), ColExpr::Col(n)) => (n, v, true),
        _ => return None,
    };
    let op = if flipped { flip(op) } else { *op };
    let c = match frame.col(name, line, col) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    match (c, lit) {
        (Col::I64 { vals, valid }, Value::Int(k)) => {
            let k = *k;
            let mut keep = Vec::new();
            for (i, (v, ok)) in vals.iter().zip(valid).enumerate() {
                if *ok && int_cmp(op, *v, k) {
                    keep.push(i);
                }
            }
            Some(Ok(keep))
        }
        (Col::F64 { vals, valid }, Value::Float(k)) => {
            if k.is_nan() {
                return None; // the kernel owns the NaN error text
            }
            let k = *k;
            let mut keep = Vec::new();
            for (i, (v, ok)) in vals.iter().zip(valid).enumerate() {
                if !*ok {
                    continue;
                }
                if v.is_nan() {
                    // Reproduce the kernel's exact NaN error (with its hint).
                    let e = crate::interp::ops::eval_binary(
                        &op,
                        Value::Float(*v),
                        Value::Float(k),
                        line,
                        col,
                    )
                    .err();
                    return Some(Err(e.unwrap_or_else(|| {
                        HelixError::new("cannot compare these values (NaN?)", line, col)
                    })
                    .hint(format!("at row {i} of the frame."))));
                }
                if float_cmp(op, *v, k) {
                    keep.push(i);
                }
            }
            Some(Ok(keep))
        }
        _ => None,
    }
}

fn flip(op: &BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Gt => BinOp::Lt,
        BinOp::Le => BinOp::Ge,
        BinOp::Ge => BinOp::Le,
        other => *other,
    }
}

fn int_cmp(op: BinOp, a: i64, b: i64) -> bool {
    match op {
        BinOp::Lt => a < b,
        BinOp::Gt => a > b,
        BinOp::Le => a <= b,
        BinOp::Ge => a >= b,
        BinOp::Eq => a == b,
        BinOp::Ne => a != b,
        _ => unreachable!("filtered by the caller"),
    }
}

fn float_cmp(op: BinOp, a: f64, b: f64) -> bool {
    match op {
        BinOp::Lt => a < b,
        BinOp::Gt => a > b,
        BinOp::Le => a <= b,
        BinOp::Ge => a >= b,
        BinOp::Eq => a == b,
        BinOp::Ne => a != b,
        _ => unreachable!("filtered by the caller"),
    }
}

// ---- group_agg ----

/// A single key column's typed key (missing keys form their own group, same as
/// the generic `RowKey`). Float keys stay on the generic path (bit-pattern
/// grouping there; rare enough not to duplicate).
#[derive(Clone, PartialEq, Eq, Hash)]
enum FastKey {
    Missing,
    Int(i64),
    Bool(bool),
    Str(Rc<String>),
}

/// `Some(frame)` when key/value columns match the fast shapes; `None` → generic.
pub fn group_agg(
    frame: &NativeFrame,
    keys: &[String],
    agg: &str,
    value_col: &str,
    line: usize,
    col: usize,
) -> Option<Result<NativeFrame, HelixError>> {
    if keys.len() != 1 {
        return None;
    }
    let kc = match frame.col(&keys[0], line, col) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    let vc = match frame.col(value_col, line, col) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    if !matches!(kc, Col::I64 { .. } | Col::Bool { .. } | Col::Str { .. }) {
        return None;
    }
    if !matches!(vc, Col::I64 { .. } | Col::F64 { .. }) {
        return None;
    }
    let n = frame.len();

    // Group discovery: first-seen order, one typed key per row (no Vec per row).
    let mut index: HashMap<FastKey, usize> = HashMap::new();
    let mut group_of = Vec::with_capacity(n);
    let mut first_row: Vec<usize> = Vec::new();
    for row in 0..n {
        let key = match kc {
            Col::I64 { vals, valid } => {
                if valid[row] { FastKey::Int(vals[row]) } else { FastKey::Missing }
            }
            Col::Bool { vals, valid } => {
                if valid[row] { FastKey::Bool(vals[row]) } else { FastKey::Missing }
            }
            Col::Str { vals, valid } => {
                if valid[row] { FastKey::Str(vals[row].clone()) } else { FastKey::Missing }
            }
            _ => unreachable!("shape-checked above"),
        };
        let g = match index.get(&key) {
            Some(&g) => g,
            None => {
                let g = first_row.len();
                index.insert(key, g);
                first_row.push(row);
                g
            }
        };
        group_of.push(g);
    }
    let ngroups = first_row.len();

    let agg_col = match agg {
        "count" => {
            let mut counts = vec![0i64; ngroups];
            for &g in &group_of {
                counts[g] += 1;
            }
            Col::I64 { vals: counts, valid: vec![true; ngroups] }
        }
        "sum" | "mean" | "min" | "max" | "std" => {
            // Missing propagation: any missing value poisons its group.
            let mut poisoned = vec![false; ngroups];
            match vc {
                Col::I64 { valid, .. } | Col::F64 { valid, .. } => {
                    for (row, ok) in valid.iter().enumerate() {
                        if !ok {
                            poisoned[group_of[row]] = true;
                        }
                    }
                }
                _ => unreachable!(),
            }
            match (vc, agg) {
                // Int sums stay Int (wrapping, like the generic path).
                (Col::I64 { vals, .. }, "sum") => {
                    let mut sums = vec![0i64; ngroups];
                    for (row, v) in vals.iter().enumerate() {
                        let g = group_of[row];
                        if !poisoned[g] {
                            sums[g] = sums[g].wrapping_add(*v);
                        }
                    }
                    finish_i64(sums, &poisoned)
                }
                (Col::I64 { vals, .. }, "min") | (Col::I64 { vals, .. }, "max") => {
                    let want_min = agg == "min";
                    let mut best = vec![0i64; ngroups];
                    let mut seen = vec![false; ngroups];
                    for (row, v) in vals.iter().enumerate() {
                        let g = group_of[row];
                        if poisoned[g] {
                            continue;
                        }
                        if !seen[g]
                            || (want_min && *v < best[g])
                            || (!want_min && *v > best[g])
                        {
                            best[g] = *v;
                            seen[g] = true;
                        }
                    }
                    finish_i64(best, &poisoned)
                }
                // Everything else runs in f64 — accumulated in ROW ORDER, the
                // generic path's exact fold order, so the bits agree.
                (vc, _) => {
                    let as_f = |row: usize| -> f64 {
                        match vc {
                            Col::I64 { vals, .. } => vals[row] as f64,
                            Col::F64 { vals, .. } => vals[row],
                            _ => unreachable!(),
                        }
                    };
                    let all_int = matches!(vc, Col::I64 { .. });
                    match agg {
                        "sum" => {
                            debug_assert!(!all_int, "int sum handled above");
                            let mut sums = vec![0.0f64; ngroups];
                            for row in 0..n {
                                let g = group_of[row];
                                if !poisoned[g] {
                                    sums[g] += as_f(row);
                                }
                            }
                            finish_f64(sums, &poisoned)
                        }
                        "mean" => {
                            let mut sums = vec![0.0f64; ngroups];
                            let mut counts = vec![0u32; ngroups];
                            for row in 0..n {
                                let g = group_of[row];
                                if !poisoned[g] {
                                    sums[g] += as_f(row);
                                    counts[g] += 1;
                                }
                            }
                            for (s, c) in sums.iter_mut().zip(&counts) {
                                *s /= *c as f64;
                            }
                            finish_f64(sums, &poisoned)
                        }
                        "min" | "max" => {
                            let want_min = agg == "min";
                            let mut best = vec![0.0f64; ngroups];
                            let mut seen = vec![false; ngroups];
                            for row in 0..n {
                                let g = group_of[row];
                                if poisoned[g] {
                                    continue;
                                }
                                let x = as_f(row);
                                if !seen[g]
                                    || (want_min && x < best[g])
                                    || (!want_min && x > best[g])
                                {
                                    best[g] = x;
                                    seen[g] = true;
                                }
                            }
                            finish_f64(best, &poisoned)
                        }
                        "std" => {
                            // Two passes, both in row order — the generic path's
                            // two-pass sample std, bit for bit. A group of one
                            // is missing (unknown spread).
                            let mut sums = vec![0.0f64; ngroups];
                            let mut counts = vec![0u32; ngroups];
                            for row in 0..n {
                                let g = group_of[row];
                                if !poisoned[g] {
                                    sums[g] += as_f(row);
                                    counts[g] += 1;
                                }
                            }
                            let means: Vec<f64> = sums
                                .iter()
                                .zip(&counts)
                                .map(|(s, c)| s / (*c).max(1) as f64)
                                .collect();
                            let mut ss = vec![0.0f64; ngroups];
                            for row in 0..n {
                                let g = group_of[row];
                                if !poisoned[g] {
                                    let d = as_f(row) - means[g];
                                    ss[g] += d * d;
                                }
                            }
                            let mut vals = Vec::with_capacity(ngroups);
                            let mut valid = Vec::with_capacity(ngroups);
                            for g in 0..ngroups {
                                if poisoned[g] || counts[g] < 2 {
                                    vals.push(0.0);
                                    valid.push(false);
                                } else {
                                    vals.push((ss[g] / (counts[g] - 1) as f64).sqrt());
                                    valid.push(true);
                                }
                            }
                            Col::F64 { vals, valid }
                        }
                        _ => unreachable!("agg set checked above"),
                    }
                }
            }
        }
        _ => {
            return Some(Err(HelixError::new(
                format!("`{agg}` is not a grouped aggregation"),
                line,
                col,
            )
            .hint("try mean, sum, min, max, count, or std.")));
        }
    };

    let key_out = kc.take(&first_row);
    let out = vec![(keys[0].clone(), key_out), (value_col.to_string(), agg_col)];
    Some(NativeFrame::new(out, line, col))
}

fn finish_i64(vals: Vec<i64>, poisoned: &[bool]) -> Col {
    let valid: Vec<bool> = poisoned.iter().map(|p| !p).collect();
    Col::I64 { vals, valid }
}

fn finish_f64(vals: Vec<f64>, poisoned: &[bool]) -> Col {
    let valid: Vec<bool> = poisoned.iter().map(|p| !p).collect();
    Col::F64 { vals, valid }
}

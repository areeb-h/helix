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

use crate::ast::BinOp;
use crate::backend::ColExpr;
use crate::error::HelixError;
use crate::value::Value;

use super::columns::Col;
use super::NativeFrame;

// ---- filter ----

/// `Some(keep)` when the predicate matches a fast shape; `None` → boxed path.
/// Chunk size for the parallel mask build (row indices stay derivable from
/// the chunk index, which the NaN error path needs).
const FILTER_CHUNK: usize = 64 * 1024;

pub fn filter_keep(
    frame: &NativeFrame,
    pred: &ColExpr,
    line: usize,
    col: usize,
) -> Option<Result<super::sel::RowSel, HelixError>> {
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
    // A parquet column still on disk: build the mask straight from its pages
    // (the predicate runs per DISTINCT dictionary value). `None` falls
    // through to the decode-then-filter path below.
    if let Some(p) = frame.parquet_pending(name) {
        let paged = match lit {
            Value::Int(k) => {
                let k = *k;
                p.filter_i64(move |v| int_cmp(op, v, k)).transpose()
            }
            Value::Float(k) if !k.is_nan() => {
                let k = *k;
                p.filter_f64(move |v| float_cmp(op, v, k)).transpose()
            }
            _ => None,
        };
        if let Some(r) = paged {
            return Some(match r {
                Ok((mask, n)) => Ok(super::sel::RowSel::from_mask(mask, n)),
                Err(m) => Err(HelixError::new(
                    format!("could not read parquet: {m}"),
                    line,
                    col,
                )),
            });
        }
    }
    let c = match frame.col(name, line, col) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    match (c, lit) {
        (Col::I64 { vals, valid }, Value::Int(k)) => {
            use rayon::prelude::*;
            let k = *k;
            let mut mask = vec![false; vals.len()];
            let n: usize = mask
                .par_chunks_mut(FILTER_CHUNK)
                .zip(vals.par_chunks(FILTER_CHUNK).zip(valid.par_chunks(FILTER_CHUNK)))
                .map(|(mc, (vc, okc))| {
                    let mut cnt = 0usize;
                    for j in 0..vc.len() {
                        if okc[j] && int_cmp(op, vc[j], k) {
                            mc[j] = true;
                            cnt += 1;
                        }
                    }
                    cnt
                })
                .sum();
            Some(Ok(super::sel::RowSel::from_mask(mask, n)))
        }
        (Col::F64 { vals, valid }, Value::Float(k)) => {
            use rayon::prelude::*;
            if k.is_nan() {
                return None; // the kernel owns the NaN error text
            }
            let k = *k;
            let mut mask = vec![false; vals.len()];
            // Each chunk reports (matches, first NaN row in it); the error, if
            // any, fires for the first NaN in ROW order — exactly the row the
            // serial walk would have stopped at.
            let per_chunk: Vec<(usize, Option<usize>)> = mask
                .par_chunks_mut(FILTER_CHUNK)
                .zip(vals.par_chunks(FILTER_CHUNK).zip(valid.par_chunks(FILTER_CHUNK)))
                .enumerate()
                .map(|(ci, (mc, (vc, okc)))| {
                    let base = ci * FILTER_CHUNK;
                    let mut cnt = 0usize;
                    for j in 0..vc.len() {
                        if !okc[j] {
                            continue;
                        }
                        if vc[j].is_nan() {
                            return (cnt, Some(base + j));
                        }
                        if float_cmp(op, vc[j], k) {
                            mc[j] = true;
                            cnt += 1;
                        }
                    }
                    (cnt, None)
                })
                .collect();
            if let Some(i) = per_chunk.iter().find_map(|(_, nan)| *nan) {
                // Reproduce the kernel's exact NaN error (with its hint).
                let e = crate::interp::ops::eval_binary(
                    &op,
                    Value::Float(vals[i]),
                    Value::Float(k),
                    line,
                    col,
                )
                .err();
                let e = e.unwrap_or_else(|| crate::interp::ops::nan_compare_error(line, col));
                return Some(Err(super::eval::at_row(e, i)));
            }
            let n = per_chunk.iter().map(|(c, _)| *c).sum();
            Some(Ok(super::sel::RowSel::from_mask(mask, n)))
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

// ---- with_columns: typed arithmetic ----

/// An owned typed operand: a column's data or a broadcast scalar.
enum TOp {
    I(Vec<i64>, Vec<bool>),
    F(Vec<f64>, Vec<bool>),
    IScalar(i64),
    FScalar(f64),
}

/// Evaluate an arithmetic ColExpr tree over numeric columns without boxing.
/// `None` = a shape outside the covered set (the boxed evaluator, which DEFINES
/// the semantics, takes over). Covered: Col/Lit leaves (i64/f64), Add/Sub/Mul
/// (int wraps, exactly the kernel), Div (always Float; a zero divisor delegates
/// that cell to the kernel so the ERROR is byte-identical, row named).
pub fn eval_typed(
    frame: &NativeFrame,
    expr: &ColExpr,
    line: usize,
    col: usize,
) -> Option<Result<Col, HelixError>> {
    match tev(frame, expr, line, col)? {
        Err(e) => Some(Err(e)),
        Ok(TOp::I(vals, valid)) => Some(Ok(Col::I64 { vals, valid })),
        Ok(TOp::F(vals, valid)) => Some(Ok(Col::F64 { vals, valid })),
        // A bare scalar expression broadcasts — leave that rarity to the boxed path.
        Ok(_) => None,
    }
}

fn tev(
    frame: &NativeFrame,
    expr: &ColExpr,
    line: usize,
    col: usize,
) -> Option<Result<TOp, HelixError>> {
    match expr {
        ColExpr::Lit(Value::Int(k)) => Some(Ok(TOp::IScalar(*k))),
        ColExpr::Lit(Value::Float(k)) => Some(Ok(TOp::FScalar(*k))),
        ColExpr::Col(name) => match frame.col(name, line, col) {
            Err(e) => Some(Err(e)),
            Ok(Col::I64 { vals, valid }) => Some(Ok(TOp::I(vals.clone(), valid.clone()))),
            Ok(Col::F64 { vals, valid }) => Some(Ok(TOp::F(vals.clone(), valid.clone()))),
            Ok(_) => None,
        },
        ColExpr::Binary(op, a, b)
            if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) =>
        {
            let l = match tev(frame, a, line, col)? {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let r = match tev(frame, b, line, col)? {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            Some(apply(*op, l, r, line, col))
        }
        _ => None,
    }
}

fn iop(op: BinOp, x: i64, y: i64) -> i64 {
    match op {
        BinOp::Add => x.wrapping_add(y),
        BinOp::Sub => x.wrapping_sub(y),
        _ => x.wrapping_mul(y),
    }
}

fn fop(op: BinOp, x: f64, y: f64) -> f64 {
    match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        _ => x / y,
    }
}

/// The kernel's own error for this cell — so the typed path's failure bytes match
/// the boxed path exactly (message, advice AND at-row hint).
///
/// Through `at_row`, which APPENDS the row to the kernel's advice. This used to call
/// `.hint(...)` directly, which replaced it — so the typed fast path silently dropped
/// "guard the denominator, e.g. `if d != 0`" and printed only a row number. Two code
/// paths for the same error, and the faster one said less.
fn cell_err(op: BinOp, a: Value, b: Value, row: usize, line: usize, col: usize) -> HelixError {
    match crate::interp::ops::eval_binary(&op, a, b, line, col) {
        Err(e) => super::eval::at_row(e, row),
        Ok(_) => HelixError::new("internal: typed path expected a kernel error", line, col),
    }
}

fn apply(op: BinOp, l: TOp, r: TOp, line: usize, col: usize) -> Result<TOp, HelixError> {
    use TOp::*;
    // Division is ALWAYS float (true division, ADR 0034) and checks its divisor.
    let div = op == BinOp::Div;
    let as_f = |t: TOp| -> TOp {
        match t {
            I(v, m) => F(v.into_iter().map(|x| x as f64).collect(), m),
            IScalar(k) => FScalar(k as f64),
            other => other,
        }
    };
    // Int stays int only for Add/Sub/Mul with both sides int.
    let both_int = matches!((&l, &r), (I(..) | IScalar(_), I(..) | IScalar(_)));
    if both_int && !div {
        return Ok(match (l, r) {
            (I(mut v, m), IScalar(k)) => {
                for x in v.iter_mut() {
                    *x = iop(op, *x, k);
                }
                I(v, m)
            }
            (IScalar(k), I(mut v, m)) => {
                for x in v.iter_mut() {
                    *x = iop(op, k, *x);
                }
                I(v, m)
            }
            (I(mut v, m), I(v2, m2)) => {
                for ((x, y), ok2) in v.iter_mut().zip(v2).zip(&m2) {
                    let _ = ok2;
                    *x = iop(op, *x, y);
                }
                let m: Vec<bool> = m.iter().zip(&m2).map(|(a, b)| *a && *b).collect();
                I(v, m)
            }
            (IScalar(a), IScalar(b)) => IScalar(iop(op, a, b)),
            _ => unreachable!("both_int checked"),
        });
    }
    // Everything else runs in f64.
    let (l, r) = (as_f(l), as_f(r));
    Ok(match (l, r) {
        (F(mut v, m), FScalar(k)) => {
            if div && k == 0.0 {
                // Every present cell divides by zero — the FIRST one errors.
                if let Some(row) = m.iter().position(|ok| *ok) {
                    return Err(cell_err(op, Value::Float(v[row]), Value::Float(k), row, line, col));
                }
            }
            for x in v.iter_mut() {
                *x = fop(op, *x, k);
            }
            F(v, m)
        }
        (FScalar(k), F(mut v, m)) => {
            if div
                && let Some(row) = v.iter().zip(&m).position(|(y, ok)| *ok && *y == 0.0)
            {
                return Err(cell_err(op, Value::Float(k), Value::Float(v[row]), row, line, col));
            }
            for x in v.iter_mut() {
                *x = fop(op, k, *x);
            }
            F(v, m)
        }
        (F(mut v, m), F(v2, m2)) => {
            if div
                && let Some(row) = v2
                    .iter()
                    .zip(m.iter().zip(&m2))
                    .position(|(y, (a, b))| *a && *b && *y == 0.0)
            {
                return Err(cell_err(
                    op,
                    Value::Float(v[row]),
                    Value::Float(v2[row]),
                    row,
                    line,
                    col,
                ));
            }
            for (x, y) in v.iter_mut().zip(&v2) {
                *x = fop(op, *x, *y);
            }
            let m: Vec<bool> = m.iter().zip(&m2).map(|(a, b)| *a && *b).collect();
            F(v, m)
        }
        (FScalar(a), FScalar(b)) => {
            if div && b == 0.0 {
                return Err(cell_err(op, Value::Float(a), Value::Float(b), 0, line, col));
            }
            FScalar(fop(op, a, b))
        }
        _ => unreachable!("promoted above"),
    })
}

// ---- group_agg ----

/// A single key column's typed key (missing keys form their own group, same as
/// the generic `RowKey`). Float keys stay on the generic path (bit-pattern
/// grouping there; rare enough not to duplicate). A string key is its DICT
/// CODE — dictionary entries are unique, so code equality is string equality.
#[derive(Clone, PartialEq, Eq, Hash)]
enum FastKey {
    Missing,
    Int(i64),
    Bool(bool),
    Code(u32),
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
            Col::Str { codes, valid, .. } => {
                if valid[row] { FastKey::Code(codes[row]) } else { FastKey::Missing }
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

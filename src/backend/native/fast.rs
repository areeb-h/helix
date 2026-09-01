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

/// The column shapes [`fast_key`] can key on.
fn keyable(c: &Col) -> bool {
    matches!(c, Col::I64 { .. } | Col::Bool { .. } | Col::Str { .. })
}

/// One row's typed key from a shape-checked column.
///
/// Shared by every fast path that buckets rows, so they cannot drift apart on
/// what "the same key" means -- which matters more than the duplication it saves,
/// because `group`, `unique` and `join` disagreeing about key identity is exactly
/// the kind of divergence the differential campaign exists to catch.
fn fast_key(c: &Col, row: usize) -> FastKey {
    match c {
        Col::I64 { vals, valid } => {
            if valid[row] { FastKey::Int(vals[row]) } else { FastKey::Missing }
        }
        Col::Bool { vals, valid } => {
            if valid[row] { FastKey::Bool(vals[row]) } else { FastKey::Missing }
        }
        Col::Str { codes, valid, .. } => {
            if valid[row] { FastKey::Code(codes[row]) } else { FastKey::Missing }
        }
        _ => unreachable!("shape-checked by the caller"),
    }
}

// ---- unique_by ----

/// `Some(keep)` — ascending row indices — for a ONE-COLUMN key subset;
/// `None` falls back to the generic `RowKey` path.
///
/// WHY THIS IS THE WHOLE FUNCTION. The generic path allocates a `Vec<KeyCell>`
/// per row, clones it again on first sight of a key, and keeps a parallel
/// `order` vector. Here the key is one machine word, so the map alone is enough:
/// a subset key keeps the LAST occurrence (upsert — newest wins, `verbs.rs`),
/// which is exactly what an unconditional `insert` does, so no first-seen order
/// needs tracking at all. The generic path's final `sort_unstable` on the kept
/// indices is reproduced verbatim, so output row order is identical.
///
/// Whole-row `unique()` (an EMPTY subset) keeps the FIRST occurrence and spans
/// every column: a different rule over a different key, so it gets its own path
/// in [`unique_keep_all`] rather than being bent to fit this one.
/// Whether a column can take part in the allocation-free row key.
///
/// Wider than [`keyable`] on purpose: this admits F64, because a whole-row unique
/// over a realistic frame nearly always has a float column in it, and excluding
/// floats would send exactly the frames that matter back to the allocating path.
/// `Null` is admitted because every one of its cells is missing, so it can only
/// ever agree -- it constrains nothing and costs nothing.
fn row_keyable(c: &Col) -> bool {
    matches!(
        c,
        Col::I64 { .. } | Col::F64 { .. } | Col::Bool { .. } | Col::Str { .. } | Col::Null { .. }
    )
}

/// One cell as raw bits, for HASHING ONLY. Collisions are permitted here and
/// resolved by [`rows_eq`]; this exists to be cheap, not to be decisive.
fn cell_bits(c: &Col, row: usize) -> u64 {
    const MISSING: u64 = 0x9e37_79b9_7f4a_7c15;
    match c {
        Col::I64 { vals, valid } => {
            if valid[row] { vals[row] as u64 } else { MISSING }
        }
        // `-0.0` folds to `0.0` before hashing, exactly as `KeyCell::of` does:
        // they are `==` in scalar Helix, so they must land in one bucket.
        Col::F64 { vals, valid } => {
            if valid[row] { (vals[row] + 0.0).to_bits() } else { MISSING }
        }
        Col::Bool { vals, valid } => {
            if valid[row] { vals[row] as u64 } else { MISSING }
        }
        Col::Str { codes, valid, .. } => {
            if valid[row] { codes[row] as u64 } else { MISSING }
        }
        Col::Null { .. } => MISSING,
    }
}

/// Do two rows agree in this column? The DECISIVE test, read straight from the
/// typed column with nothing materialised.
///
/// Dictionary codes compare as codes: entries are unique within a column, so code
/// equality is string equality -- the same license `group_agg` relies on. Floats
/// compare by canonicalised BIT PATTERN, which makes NaN equal to itself, matching
/// `RowKey`'s rule that grouping equality is identity rather than `==`.
fn cells_eq(c: &Col, a: usize, b: usize) -> bool {
    match c {
        Col::I64 { vals, valid } => valid[a] == valid[b] && (!valid[a] || vals[a] == vals[b]),
        Col::F64 { vals, valid } => {
            valid[a] == valid[b]
                && (!valid[a] || (vals[a] + 0.0).to_bits() == (vals[b] + 0.0).to_bits())
        }
        Col::Bool { vals, valid } => valid[a] == valid[b] && (!valid[a] || vals[a] == vals[b]),
        Col::Str { codes, valid, .. } => {
            valid[a] == valid[b] && (!valid[a] || codes[a] == codes[b])
        }
        Col::Null { .. } => true,
    }
}

fn rows_eq(cols: &[&Col], a: usize, b: usize) -> bool {
    cols.iter().all(|c| cells_eq(c, a, b))
}

/// A `Hasher` for keys that ARE ALREADY HASHES.
///
/// `std`'s default is SipHash-1-3: keyed, DoS-resistant, and defined over
/// arbitrary bytes. All three properties are wasted here. `row_hash` has already
/// mixed the row's cells into a well-distributed `u64`, so handing that to a
/// `HashMap` hashed it a SECOND time — the table was doing the expensive half of
/// the work twice and the cheap half not at all.
///
/// Nothing in these tables is a security boundary: they are built from a
/// program's own data, live for the duration of one verb, and are never exposed
/// to an adversary choosing keys.
#[derive(Default)]
struct PreHashed(u64);

impl std::hash::Hasher for PreHashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // Unreachable by construction — only `u64` keys are stored — but folding
        // the bytes keeps it a correct hasher rather than merely an unused one.
        for b in bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn write_u64(&mut self, v: u64) {
        // splitmix64's finalizer. `row_hash` mixes well, but bucket selection
        // reads the LOW bits, and one avalanche removes any dependence on where
        // FNV happens to put its entropy.
        let mut z = v;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.0 = z ^ (z >> 31);
    }
}

type PreHashedMap<V> = HashMap<u64, V, std::hash::BuildHasherDefault<PreHashed>>;

fn row_hash(cols: &[&Col], row: usize) -> u64 {
    // FNV-1a over the cells' bits. No allocation, no `Value`, no per-row Vec --
    // which is the entire point of this path.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in cols {
        h ^= cell_bits(c, row);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `Some(keep)` — ascending row indices — for whole-row `unique()`;
/// `None` falls back to the generic `RowKey` path.
///
/// WHY THIS EXISTS. The generic path builds a `RowKey` per row: a `Vec<KeyCell>`
/// allocated for every row, cloned again on first sight, with an `Rc` bump per
/// string cell. That makes the cost proportional to ROWS rather than to distinct
/// rows, which is backwards for the shape this verb is usually asked about -- a
/// categorical column where a million rows carry a thousand distinct values. It
/// measured 4.8x behind the polars oracle at 100k rows and 9.0x at 400k, growing
/// linearly while the oracle grew sublinearly. Nothing here is cleverer than that
/// path; it just refuses to allocate to ask a question it can answer by looking.
///
/// Keeps the FIRST occurrence, and visits rows in order, so `keep` is already
/// ascending — the generic path's closing `sort_unstable` would be a no-op.
/// Whole-row unique by COMPOSITE direct addressing.
///
/// The single-column idea, carried to the whole row: if every column has a
/// bounded dense domain, then the row's identity IS a mixed-radix integer over
/// those domains, and that integer is a slot. One array probe per row replaces a
/// hash, a table probe and a candidate comparison.
///
/// Keeps the FIRST occurrence — a slot is claimed only when empty — and visits
/// rows in order, so `keep` comes out ascending with no sort to do afterwards.
///
/// REFUSED when any column has no bounded domain (a float is the common case), or
/// when the product of domains exceeds the budget. The product is the real hazard:
/// it is the worst case, not the actual distinct count, and CORRELATED columns
/// make it wildly pessimistic — two perfectly correlated 1000-value columns ask
/// for a million slots to hold a thousand rows. The budget is what keeps that from
/// trading a hash table for an enormous empty array.
/// Whole-row unique by COMPOSITE direct addressing.
///
/// The single-column idea carried to the whole row: if every column has a bounded
/// domain, the row's identity IS a mixed-radix integer over those domains, and
/// that integer is a slot. One array probe per row replaces a hash, a table probe
/// and a candidate comparison.
///
/// Keeps the FIRST occurrence — a slot is claimed only when empty — and visits
/// rows in order, so `keep` comes out ascending with no sort to do.
///
/// REFUSED when any column has no bounded domain, or when the product of domains
/// exceeds the budget. The product is the real hazard: it is the worst case, not
/// the actual distinct count, and CORRELATED columns make it wildly pessimistic —
/// two perfectly correlated 1000-value columns ask for a million slots to hold a
/// thousand rows.
fn unique_dense_row(cols: &[&Col], n: usize) -> Option<Vec<usize>> {
    if n > u32::MAX as usize {
        return None;
    }
    // 4M slots = 16 MB. Past this the array stops being the cheaper structure.
    const MAX_SLOTS: u128 = 1 << 22;

    let mut sizes: Vec<u128> = Vec::with_capacity(cols.len());
    let mut bases: Vec<i64> = Vec::with_capacity(cols.len());
    let mut product: u128 = 1;
    for c in cols {
        let (size, base) = dense_domain(c, n)?;
        product = product.checked_mul(size as u128)?;
        if product > MAX_SLOTS {
            return None;
        }
        sizes.push(size as u128);
        bases.push(base);
    }

    let mut seen = vec![NO_ROW; product as usize];
    let mut keep: Vec<usize> = Vec::new();
    for row in 0..n {
        // Horner over the domains: idx = ((s0 * d1 + s1) * d2 + s2) ...
        let mut idx: u128 = 0;
        for ((c, size), base) in cols.iter().zip(&sizes).zip(&bases) {
            idx = idx * size + dense_slot(c, row, *base) as u128;
        }
        let slot = idx as usize;
        if seen[slot] == NO_ROW {
            seen[slot] = row as u32;
            keep.push(row);
        }
    }
    Some(keep)
}

pub fn unique_keep_all(
    frame: &NativeFrame,
    subset: &[String],
    line: usize,
    col: usize,
) -> Option<Result<Vec<usize>, HelixError>> {
    // Each fast path decides its OWN applicability, so both take the same
    // arguments and the caller just offers them the question in turn.
    if !subset.is_empty() {
        return None;
    }
    let named = match frame.columns(line, col) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    let cols: Vec<&Col> = named.into_iter().map(|(_, c)| c).collect();
    if cols.is_empty() || !cols.iter().all(|c| row_keyable(c)) {
        return None;
    }

    let n = frame.len();
    // Composite direct addressing first, for the same reason the single-column
    // path tries it first: when the domains are bounded there is no key to hash.
    if let Some(keep) = unique_dense_row(&cols, n) {
        return Some(Ok(keep));
    }
    // One entry per DISTINCT row, holding a representative row number inline --
    // not a bucket vector, which would allocate once per distinct row and give
    // most of the win back on a frame that is mostly distinct.
    let mut first: PreHashedMap<usize> =
        PreHashedMap::with_capacity_and_hasher(n / 8 + 16, Default::default());
    // Only ever touched by a genuine 64-bit hash collision between rows that are
    // actually different. Rare, but handled rather than assumed away: a wrong
    // answer here would be a silently dropped row.
    let mut extra: PreHashedMap<Vec<usize>> = PreHashedMap::default();
    let mut keep: Vec<usize> = Vec::new();

    for row in 0..n {
        let h = row_hash(&cols, row);
        match first.get(&h) {
            None => {
                first.insert(h, row);
                keep.push(row);
            }
            Some(&rep) => {
                if rows_eq(&cols, rep, row) {
                    continue;
                }
                let bucket = extra.entry(h).or_default();
                if bucket.iter().any(|&seen| rows_eq(&cols, seen, row)) {
                    continue;
                }
                bucket.push(row);
                keep.push(row);
            }
        }
    }
    Some(Ok(keep))
}

/// An empty slot in a dense direct-address table.
const NO_ROW: u32 = u32::MAX;

/// Dedup a single key column by DIRECT ADDRESSING — no hash table at all.
///
/// The idea: a hash table exists to map an arbitrary key into a dense slot. When
/// the key is ALREADY a dense integer, that map is the identity and the whole
/// table is redundant work. Two columns hand it to us for free:
///
///   * `Col::Str` is dictionary-encoded, and its codes are dense in
///     `[0, dict.len())` BY CONSTRUCTION. The distinct set is already computed —
///     hashing the strings would be recomputing what the column knows.
///   * `Col::I64` needs one scan for its range, and a bounded range is the normal
///     case for the columns people actually deduplicate: ids, categories, keys.
///
/// So dedup becomes one pass of array writes: `last[slot] = row`. Unconditional,
/// because a one-column subset keeps the LAST occurrence (upsert) — the same rule
/// the hash path implements with `insert`. Slot 0 is reserved for missing, so a
/// missing key stays its own key exactly as `RowKey` has it.
///
/// Cost: one linear pass with no hashing, no probing and no rehash growth, plus a
/// sweep of the table. The table is `4 * (domain + 1)` bytes — 4 KB for a
/// thousand-value dictionary, whatever the row count. The row count only ever
/// touches the pass, never the table.
///
/// `None` when no dense domain applies, and the hash path still stands behind it.
/// The dense slot domain of a key column: how many slots it needs, and the base
/// to subtract from an `I64` value. Slot 0 is ALWAYS missing, so a missing cell
/// keeps its own identity exactly as `RowKey` gives it.
///
/// `None` when the column has no bounded domain — a float, or an integer whose
/// range is too sparse to be worth an array.
///
/// ONE definition, because `unique`, whole-row `unique` and `group` must agree on
/// what "the same key" means. Three copies of this arithmetic would be three
/// chances to disagree, and a disagreement here is a silent wrong answer rather
/// than a crash.
fn dense_domain(c: &Col, n: usize) -> Option<(usize, i64)> {
    match c {
        // Dictionary codes are dense in `[0, dict.len())` by construction: the
        // distinct set is already computed, so there is nothing left to hash.
        Col::Str { dict, .. } => Some((dict.len() + 1, 0)),
        Col::Bool { .. } => Some((3, 0)),
        Col::Null { .. } => Some((1, 0)),
        Col::I64 { vals, valid } => {
            let (mut lo, mut hi) = (i64::MAX, i64::MIN);
            let mut any = false;
            for row in 0..n {
                if valid[row] {
                    any = true;
                    lo = lo.min(vals[row]);
                    hi = hi.max(vals[row]);
                }
            }
            if !any {
                return Some((1, 0));
            }
            // A sparse range would trade a hash table for a larger, emptier array.
            // Bounded by a multiple of the rows and a hard ceiling, so a
            // pathological key cannot allocate unboundedly.
            let span = (hi as i128 - lo as i128 + 1) as u128;
            let budget = ((n as u128) * 4).clamp(1024, 1 << 24);
            if span > budget {
                return None;
            }
            Some((span as usize + 1, lo))
        }
        _ => None,
    }
}

/// The slot a row occupies within its column's dense domain (see [`dense_domain`]).
fn dense_slot(c: &Col, row: usize, base: i64) -> usize {
    match c {
        Col::Str { codes, valid, .. } => {
            if valid[row] { codes[row] as usize + 1 } else { 0 }
        }
        Col::Bool { vals, valid } => {
            if valid[row] { vals[row] as usize + 1 } else { 0 }
        }
        Col::Null { .. } => 0,
        Col::I64 { vals, valid } => {
            if valid[row] { (vals[row] as i128 - base as i128) as usize + 1 } else { 0 }
        }
        _ => unreachable!("domain-checked by dense_domain"),
    }
}

/// Dedup a single key column by DIRECT ADDRESSING — no hash table at all.
///
/// A hash table exists to map an arbitrary key onto a dense slot; when the key is
/// already a dense integer that map is the identity, and the table is redundant
/// work. Unconditional writes, because a one-column subset keeps the LAST
/// occurrence (upsert) — the rule the hash path implements with `insert`.
fn unique_dense(kc: &Col, n: usize) -> Option<Vec<usize>> {
    // Rows are stored as `u32` in the table; refuse rather than truncate.
    if n > u32::MAX as usize {
        return None;
    }
    let (domain, base) = dense_domain(kc, n)?;
    let mut last = vec![NO_ROW; domain];
    for row in 0..n {
        last[dense_slot(kc, row, base)] = row as u32;
    }
    // Sweeping the table yields rows in SLOT order, not row order, so the sort is
    // what makes this reproduce the hash path byte for byte.
    let mut keep: Vec<usize> =
        last.into_iter().filter(|&r| r != NO_ROW).map(|r| r as usize).collect();
    keep.sort_unstable();
    Some(keep)
}

pub fn unique_keep(
    frame: &NativeFrame,
    subset: &[String],
    line: usize,
    col: usize,
) -> Option<Result<Vec<usize>, HelixError>> {
    if subset.len() != 1 {
        return None;
    }
    let kc = match frame.col(&subset[0], line, col) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    if !keyable(kc) {
        return None;
    }
    let n = frame.len();
    // Direct addressing first: when the key domain is already dense there is no
    // reason to hash it. Measured at 400k rows over 1000 distinct values, the hash
    // path below spent ~6ms almost entirely in `std`'s SipHash.
    if let Some(keep) = unique_dense(kc, n) {
        return Some(Ok(keep));
    }
    let mut chosen: HashMap<FastKey, usize> = HashMap::with_capacity(n / 8 + 16);
    for row in 0..n {
        // Unconditional: the later row supersedes the earlier one. Missing is its
        // own key here, matching `RowKey`, so all-missing rows collapse to one.
        chosen.insert(fast_key(kc, row), row);
    }
    let mut keep: Vec<usize> = chosen.into_values().collect();
    keep.sort_unstable();
    Some(Ok(keep))
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
    if !keyable(kc) {
        return None;
    }
    if !matches!(vc, Col::I64 { .. } | Col::F64 { .. }) {
        return None;
    }
    let n = frame.len();

    // Group discovery: first-seen order, one typed key per row (no Vec per row).
    const NO_GROUP: u32 = u32::MAX;
    let mut group_of = Vec::with_capacity(n);
    let mut first_row: Vec<usize> = Vec::new();
    match dense_domain(kc, n) {
        // DIRECT ADDRESSING when the key domain is bounded: slot -> group id in a
        // flat array, so finding a row's group is one array read instead of a hash
        // and a probe. Group ids are still handed out in FIRST-SEEN order, which is
        // the output row order the oracle pins — the array changes how a group is
        // FOUND, never which group a row belongs to nor where it lands.
        Some((domain, base)) if n < u32::MAX as usize => {
            let mut slot_group = vec![NO_GROUP; domain];
            for row in 0..n {
                let slot = dense_slot(kc, row, base);
                let g = if slot_group[slot] == NO_GROUP {
                    let g = first_row.len() as u32;
                    slot_group[slot] = g;
                    first_row.push(row);
                    g
                } else {
                    slot_group[slot]
                };
                group_of.push(g as usize);
            }
        }
        _ => {
            let mut index: HashMap<FastKey, usize> = HashMap::new();
            for row in 0..n {
                let key = fast_key(kc, row);
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
        }
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
            // A NaN makes every aggregation in its group answer NaN (ADR 0036 policy
            // 4), and this typed path has no way to say that without a second poison
            // vector threaded through all seven arms. So it DECLINES, and the generic
            // path -- which owns the rule -- answers. That is the fast path's standing
            // discipline: never a different answer, only a faster one or none.
            //
            // Detected in the loop that already walks the column, so it costs one
            // branch per row rather than a second pass. A NaN-bearing column is rare
            // and is already evidence of a computation that failed.
            if let Col::F64 { vals, .. } = vc
                && vals.iter().any(|x| x.is_nan())
            {
                return None;
            }
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

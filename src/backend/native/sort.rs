//! Multi-key stable sort, missing first (ADR 0034 §4). Stability comes from
//! `sort_by` on a row-index permutation — equal keys keep their input order, the
//! guarantee the polars backend needed `maintain_order: true` armor to force.

use std::cmp::Ordering;

use rayon::prelude::*;

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
    // A single key sorts on the typed column directly — no Value per comparison.
    // Missing first, then the same orders `cell_cmp` uses (floats by total_cmp),
    // and `sort_by` is stable, so the bits match the generic path exactly.
    if let [c] = key_cols.as_slice() {
        match c {
            Col::I64 { vals, valid } => {
                // Packed (valid, key, row): sorting owned tuples walks memory
                // sequentially instead of chasing the permutation, and the
                // unique row index makes unstable sort order-deterministic —
                // equal keys keep input order, exactly like the stable path.
                let mut keys: Vec<(bool, i64, u32)> = (0..vals.len())
                    .map(|i| (valid[i], vals[i], i as u32))
                    .collect();
                keys.par_sort_unstable();
                // Missing first: false < true puts invalid rows ahead, and
                // their placeholder key (0) plus the row tiebreak keeps them
                // in input order — same as cell_cmp's Equal under stability.
                let idx: Vec<usize> = keys.iter().map(|(_, _, i)| *i as usize).collect();
                return Ok(frame.take(idx));
            }
            Col::F64 { vals, valid } => {
                // f64 packed by its total_cmp bit trick: flip all bits of
                // negatives, flip the sign bit of non-negatives — u64 order is
                // then exactly total_cmp order.
                let enc = |x: f64| -> u64 {
                    let b = x.to_bits();
                    if b >> 63 == 1 { !b } else { b | (1u64 << 63) }
                };
                let mut keys: Vec<(bool, u64, u32)> = (0..vals.len())
                    .map(|i| (valid[i], enc(vals[i]), i as u32))
                    .collect();
                keys.par_sort_unstable();
                let idx: Vec<usize> = keys.iter().map(|(_, _, i)| *i as usize).collect();
                return Ok(frame.take(idx));
            }
            Col::Str { dict, codes, valid } => {
                // Rank the dictionary once (it is small), then the row sort is
                // an integer pack-sort: rank order == text order, unique ranks
                // per distinct text, row index keeps ties in input order.
                let mut order: Vec<u32> = (0..dict.len() as u32).collect();
                order.sort_by(|&a, &b| dict[a as usize].cmp(&dict[b as usize]));
                let mut rank = vec![0u32; dict.len()];
                for (r, &d) in order.iter().enumerate() {
                    rank[d as usize] = r as u32;
                }
                let mut keys: Vec<(bool, u32, u32)> = (0..codes.len())
                    .map(|i| (valid[i], rank[codes[i] as usize], i as u32))
                    .collect();
                keys.par_sort_unstable();
                let idx: Vec<usize> = keys.iter().map(|(_, _, i)| *i as usize).collect();
                return Ok(frame.take(idx));
            }
            _ => {}
        }
    }
    idx.sort_by(|&a, &b| {
        for c in &key_cols {
            let ord = cell_cmp(&c.get(a), &c.get(b));
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    Ok(frame.take(idx))
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

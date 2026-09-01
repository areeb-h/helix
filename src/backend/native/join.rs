//! Hash join — four kinds, coalesced keys, `_right` suffix on collisions,
//! left-then-right read order (ADR 0034 §5). Deterministic by construction: the
//! output order is a plain loop over the left frame (then, for `right`/`outer`,
//! unmatched right rows in right order) — the property the polars backend needed
//! `MaintainOrderJoin::LeftRight` armor to force.

use std::collections::HashMap;

use crate::error::HelixError;
use crate::value::Value;

use super::columns::Col;
use super::key::RowKey;
use super::NativeFrame;

pub fn join(
    left: &NativeFrame,
    right: &NativeFrame,
    keys: &[String],
    how: &str,
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    if !matches!(how, "inner" | "left" | "right" | "outer" | "full") {
        return Err(HelixError::new(format!("`{how}` is not a join kind"), line, col)
            .hint("try \"inner\", \"left\", \"right\", or \"outer\"."));
    }
    // THE SEAM'S SHARED DIAGNOSTIC, not a private one. `backend/mod.rs` states that
    // engine-agnostic validation lives there "so every backend produces identical
    // Helix error messages" — but `validate_join_keys` had exactly one caller, in
    // `backend/polars.rs`. A bad key therefore read as "no column `k` in the left
    // frame" on one engine and a bare "no column `k`" on the other, losing the only
    // thing a join error needs to say: WHICH side is missing it.
    //
    // Nothing caught this. `dfdiff` compares 129 programs and none of them joins on
    // a bad key, and the `#[cfg_attr(not(dataframes), allow(dead_code))]` on the
    // validator meant a native-only build compiled it as dead code without a word.
    // An `allow` that silences a warning also silences the question the warning was
    // asking.
    let lnames: Vec<String> =
        left.columns(line, col)?.into_iter().map(|(n, _)| n.clone()).collect();
    let rnames: Vec<String> =
        right.columns(line, col)?.into_iter().map(|(n, _)| n.clone()).collect();
    crate::backend::validate_join_keys(&lnames, &rnames, keys, line, col)?;

    let lkeys: Vec<&Col> = keys.iter().map(|k| left.col(k, line, col)).collect::<Result<_, _>>()?;
    let rkeys: Vec<&Col> =
        keys.iter().map(|k| right.col(k, line, col)).collect::<Result<_, _>>()?;
    // Mismatched key dtypes: refuse, never a silent 0-row answer (the polars
    // backend errors here too; the sweep caught native answering an empty
    // frame with exit 0). A Null column (empty frame) constrains nothing.
    for (i, name) in keys.iter().enumerate() {
        let (l, r) = (lkeys[i], rkeys[i]);
        if !l.same_dtype(r)
            && !matches!(l, Col::Null { .. })
            && !matches!(r, Col::Null { .. })
        {
            return Err(HelixError::new(
                format!(
                    "join key `{name}` is {} on the left and {} on the right",
                    l.dtype_name(),
                    r.dtype_name()
                ),
                line,
                col,
            )
            .hint("cast one side first, e.g. `with({id: @id * 1.0})`."));
        }
    }

    // Right-side index: key -> row numbers, in right-frame order. A key with a
    // missing cell never enters the index — missing matches nothing (§5). A
    // single key skips the per-row Vec entirely (a KeyCell is one enum).
    // Pair emission is generic over "how do I probe row r": each typed key
    // shape supplies index-build and probe closures that never box a cell; the
    // generic RowKey machinery remains for multi-key and rare dtypes.
    // TWO PARALLEL INDEX COLUMNS, not a vector of pairs. The pair vector was
    // 32 bytes a row — two `Option<usize>`, neither with a spare niche — and was
    // then split into exactly these two vectors by two more full passes. For a
    // 400k-row join that was ~25 MB of allocation and copying to express 12 MB of
    // indices, all of it before a single column was gathered.
    //
    // Reserved at the left row count: every join kind emits at least one row per
    // matched left row, so that is the result's natural scale.
    let mut left_idx: Vec<Option<usize>> = Vec::with_capacity(left.len());
    let mut right_idx: Vec<Option<usize>> = Vec::with_capacity(left.len());
    let mut right_hit = vec![false; right.len()];
    let keep_unmatched_left = matches!(how, "left" | "outer" | "full");
    {
        let mut emit = |lrow: usize, rrows: Option<&Vec<usize>>| match rrows {
            Some(rrows) => {
                for &rrow in rrows {
                    left_idx.push(Some(lrow));
                    right_idx.push(Some(rrow));
                    right_hit[rrow] = true;
                }
            }
            None => {
                if keep_unmatched_left {
                    left_idx.push(Some(lrow));
                    right_idx.push(None);
                }
            }
        };
        match (keys.len(), lkeys[0], rkeys[0]) {
            (1, Col::I64 { vals: lv, valid: lm }, Col::I64 { vals: rv, valid: rm }) => {
                // DENSE DIRECT ADDRESSING, the same shape the `Str` branch below
                // already uses with dictionary codes. A hash table exists to map an
                // arbitrary key onto a dense slot; an integer key inside a bounded
                // range IS that slot, so the table is redundant work. The build
                // hashed once per RIGHT row and the probe once per LEFT row, which
                // at 400k rows was most of this verb's time — in `std`'s SipHash,
                // a keyed DoS-resistant hash whose properties nothing here needs.
                //
                // The range comes from the right side because that is what gets
                // indexed. A left value outside it simply misses, which is the
                // same answer the map would have given.
                let (mut lo, mut hi) = (i64::MAX, i64::MIN);
                let mut any = false;
                for (v, ok) in rv.iter().zip(rm) {
                    if *ok {
                        any = true;
                        lo = lo.min(*v);
                        hi = hi.max(*v);
                    }
                }
                // A sparse range would swap a hash table for a larger, emptier
                // array. Bounded by both a multiple of the rows indexed and a hard
                // ceiling, so a pathological key can never allocate unboundedly.
                let span: u128 = if any { (hi as i128 - lo as i128 + 1) as u128 } else { 0 };
                let budget = ((right.len() as u128) * 4).clamp(1024, 1 << 24);
                if any && span <= budget {
                    let mut by_slot: Vec<Vec<usize>> = vec![Vec::new(); span as usize];
                    for (row, (v, ok)) in rv.iter().zip(rm).enumerate() {
                        if *ok {
                            by_slot[(*v as i128 - lo as i128) as usize].push(row);
                        }
                    }
                    for (lrow, (v, ok)) in lv.iter().zip(lm).enumerate() {
                        // An EMPTY slot must read as `None`, not as `Some(&[])`:
                        // `emit` treats `Some` as "matched" and would then drop the
                        // unmatched-left row a left/outer join has to keep.
                        let m = if *ok && *v >= lo && *v <= hi {
                            let slot = &by_slot[(*v as i128 - lo as i128) as usize];
                            if slot.is_empty() { None } else { Some(slot) }
                        } else {
                            None
                        };
                        emit(lrow, m);
                    }
                } else {
                    let mut index: HashMap<i64, Vec<usize>> = HashMap::new();
                    for (row, (v, ok)) in rv.iter().zip(rm).enumerate() {
                        if *ok {
                            index.entry(*v).or_default().push(row);
                        }
                    }
                    for (lrow, (v, ok)) in lv.iter().zip(lm).enumerate() {
                        emit(lrow, if *ok { index.get(v) } else { None });
                    }
                }
            }
            (
                1,
                Col::Str { dict: ld, codes: lc, valid: lm },
                Col::Str { dict: rd, codes: rc, valid: rm },
            ) => {
                // Rows indexed by RIGHT code (dense — dictionaries are small);
                // left codes translate to right codes once per dict entry, so
                // per-row probing is two array lookups.
                let mut by_rcode: Vec<Vec<usize>> = vec![Vec::new(); rd.len()];
                for (row, (code, ok)) in rc.iter().zip(rm).enumerate() {
                    if *ok {
                        by_rcode[*code as usize].push(row);
                    }
                }
                let rmap: HashMap<&str, u32> = rd
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (s.as_str(), i as u32))
                    .collect();
                let trans: Vec<Option<u32>> =
                    ld.iter().map(|s| rmap.get(s.as_str()).copied()).collect();
                for (lrow, (code, ok)) in lc.iter().zip(lm).enumerate() {
                    let m = if *ok {
                        trans[*code as usize].map(|rcode| &by_rcode[rcode as usize])
                    } else {
                        None
                    };
                    emit(lrow, m);
                }
            }
            _ => {
                let mut index: HashMap<RowKey, Vec<usize>> = HashMap::new();
                for row in 0..right.len() {
                    let key = RowKey::at(&rkeys, row);
                    if !key.has_missing() {
                        index.entry(key).push_or_insert(row);
                    }
                }
                for lrow in 0..left.len() {
                    let key = RowKey::at(&lkeys, lrow);
                    let m = if key.has_missing() { None } else { index.get(&key) };
                    emit(lrow, m);
                }
            }
        }
    }
    if matches!(how, "right" | "outer" | "full") {
        for (rrow, hit) in right_hit.iter().enumerate() {
            if !hit {
                left_idx.push(None);
                right_idx.push(Some(rrow));
            }
        }
    }

    // Column layout (the oracle's rule, caught by the differential harness):
    // the join's OWN side keeps its column order with key columns coalesced in
    // place; the other side contributes its non-key columns after, a colliding
    // name taking the `_right` suffix. For inner/left/outer that own side is
    // the left frame; for `right` it is (left non-keys first, then) the right
    // frame's order.
    let key_at = |name: &String| keys.iter().position(|k| k == name);
    // Typed assembly: side columns are optional gathers, key columns coalesce
    // typed when both sides share a dtype. The boxed builders exist only as the
    // mixed-dtype fallback (Int key joined to Float key, etc.).
    let coalesced = |k: usize| -> Result<Col, HelixError> {
        if let Some(c) = Col::coalesce_gather(lkeys[k], rkeys[k], &left_idx, &right_idx) {
            return Ok(c);
        }
        let cells: Vec<Value> = left_idx
            .iter()
            .zip(&right_idx)
            .map(|(l, r)| match (l, r) {
                (Some(lr), _) => lkeys[k].get(*lr),
                (None, Some(rr)) => rkeys[k].get(*rr),
                (None, None) => Value::Missing,
            })
            .collect();
        Col::from_values(&keys[k], &cells, line, col)
    };
    let mut out: Vec<(String, Col)> = Vec::with_capacity(left.width() + right.width() - keys.len());
    let push = |name: String, packed: Col, out: &mut Vec<(String, Col)>| {
        let final_name =
            if out.iter().any(|(n, _)| *n == name) { format!("{name}_right") } else { name };
        out.push((final_name, packed));
    };

    let left_cols = left.columns(line, col)?;
    let right_cols = right.columns(line, col)?;
    if how == "right" {
        for (name, c) in &left_cols {
            if key_at(name).is_none() {
                push((*name).clone(), c.take_opt(&left_idx), &mut out);
            }
        }
        for (name, c) in &right_cols {
            match key_at(name) {
                Some(k) => push((*name).clone(), coalesced(k)?, &mut out),
                None => push((*name).clone(), c.take_opt(&right_idx), &mut out),
            }
        }
    } else {
        for (name, c) in &left_cols {
            match key_at(name) {
                Some(k) => push((*name).clone(), coalesced(k)?, &mut out),
                None => push((*name).clone(), c.take_opt(&left_idx), &mut out),
            }
        }
        for (name, c) in &right_cols {
            if key_at(name).is_none() {
                push((*name).clone(), c.take_opt(&right_idx), &mut out);
            }
        }
    }
    NativeFrame::new(out, line, col)
}

/// Small ergonomic helper: `entry(key).push_or_insert(row)`.
trait PushOrInsert {
    fn push_or_insert(self, row: usize);
}

impl PushOrInsert for std::collections::hash_map::Entry<'_, RowKey, Vec<usize>> {
    fn push_or_insert(self, row: usize) {
        self.or_default().push(row);
    }
}

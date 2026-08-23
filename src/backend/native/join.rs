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
    let lkeys: Vec<&Col> = keys.iter().map(|k| left.col(k, line, col)).collect::<Result<_, _>>()?;
    let rkeys: Vec<&Col> =
        keys.iter().map(|k| right.col(k, line, col)).collect::<Result<_, _>>()?;

    // Right-side index: key -> row numbers, in right-frame order. A key with a
    // missing cell never enters the index — missing matches nothing (§5). A
    // single key skips the per-row Vec entirely (a KeyCell is one enum).
    // Pair emission is generic over "how do I probe row r": each typed key
    // shape supplies index-build and probe closures that never box a cell; the
    // generic RowKey machinery remains for multi-key and rare dtypes.
    let mut pairs: Vec<(Option<usize>, Option<usize>)> = Vec::new();
    let mut right_hit = vec![false; right.len()];
    let keep_unmatched_left = matches!(how, "left" | "outer" | "full");
    {
        let mut emit = |lrow: usize, rrows: Option<&Vec<usize>>| match rrows {
            Some(rrows) => {
                for &rrow in rrows {
                    pairs.push((Some(lrow), Some(rrow)));
                    right_hit[rrow] = true;
                }
            }
            None => {
                if keep_unmatched_left {
                    pairs.push((Some(lrow), None));
                }
            }
        };
        match (keys.len(), lkeys[0], rkeys[0]) {
            (1, Col::I64 { vals: lv, valid: lm }, Col::I64 { vals: rv, valid: rm }) => {
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
            (1, Col::Str { vals: lv, valid: lm }, Col::Str { vals: rv, valid: rm }) => {
                let mut index: HashMap<&str, Vec<usize>> = HashMap::new();
                for (row, (v, ok)) in rv.iter().zip(rm).enumerate() {
                    if *ok {
                        index.entry(v.as_str()).or_default().push(row);
                    }
                }
                for (lrow, (v, ok)) in lv.iter().zip(lm).enumerate() {
                    emit(lrow, if *ok { index.get(v.as_str()) } else { None });
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
                pairs.push((None, Some(rrow)));
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
        if let Some(c) = Col::coalesce_gather(lkeys[k], rkeys[k], &pairs) {
            return Ok(c);
        }
        let cells: Vec<Value> = pairs
            .iter()
            .map(|(l, r)| match (l, r) {
                (Some(lr), _) => lkeys[k].get(*lr),
                (None, Some(rr)) => rkeys[k].get(*rr),
                (None, None) => Value::Missing,
            })
            .collect();
        Col::from_values(&keys[k], &cells, line, col)
    };
    let left_idx: Vec<Option<usize>> = pairs.iter().map(|p| p.0).collect();
    let right_idx: Vec<Option<usize>> = pairs.iter().map(|p| p.1).collect();

    let mut out: Vec<(String, Col)> = Vec::with_capacity(left.width() + right.width() - keys.len());
    let push = |name: String, packed: Col, out: &mut Vec<(String, Col)>| {
        let final_name =
            if out.iter().any(|(n, _)| *n == name) { format!("{name}_right") } else { name };
        out.push((final_name, packed));
    };

    if how == "right" {
        for (name, c) in left.columns() {
            if key_at(name).is_none() {
                push(name.clone(), c.take_opt(&left_idx), &mut out);
            }
        }
        for (name, c) in right.columns() {
            match key_at(name) {
                Some(k) => push(name.clone(), coalesced(k)?, &mut out),
                None => push(name.clone(), c.take_opt(&right_idx), &mut out),
            }
        }
    } else {
        for (name, c) in left.columns() {
            match key_at(name) {
                Some(k) => push(name.clone(), coalesced(k)?, &mut out),
                None => push(name.clone(), c.take_opt(&left_idx), &mut out),
            }
        }
        for (name, c) in right.columns() {
            if key_at(name).is_none() {
                push(name.clone(), c.take_opt(&right_idx), &mut out);
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

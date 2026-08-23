//! Grouped aggregation — the armor's doctrine implemented natively (ADR 0034 §3):
//! `count` counts rows INCLUDING missing; every other aggregation PROPAGATES
//! missing (an all-missing group is unknown, not zero); groups come out in
//! first-seen order, deterministically, because the loop below has no other
//! order to offer. Float sums are left-to-right in row order — bit-matching the
//! oracle's sequential kernel; the Neumaier upgrade lands at the Stage-4 flip.

use std::collections::HashMap;

use crate::error::HelixError;
use crate::value::Value;

use super::columns::Col;
use super::key::RowKey;
use super::NativeFrame;

pub fn group_agg(
    frame: &NativeFrame,
    keys: &[String],
    agg: &str,
    value_col: &str,
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    // The typed single-key path serves the common shapes; this generic path
    // DEFINES the semantics it must reproduce (the differential tests hold
    // both to the polars oracle).
    if let Some(r) = super::fast::group_agg(frame, keys, agg, value_col, line, col) {
        return r;
    }
    if !matches!(agg, "count" | "mean" | "sum" | "min" | "max" | "std") {
        return Err(HelixError::new(format!("`{agg}` is not a grouped aggregation"), line, col)
            .hint("try mean, sum, min, max, count, or std."));
    }
    let key_cols: Vec<&Col> =
        keys.iter().map(|k| frame.col(k, line, col)).collect::<Result<_, _>>()?;
    let vals = frame.col(value_col, line, col)?;
    let n = frame.len();

    // First-seen group order: the map remembers WHERE a group's rows collect;
    // `order` remembers WHEN it was first seen.
    let mut index: HashMap<RowKey, usize> = HashMap::new();
    let mut order: Vec<(RowKey, Vec<usize>)> = Vec::new();
    for row in 0..n {
        let key = RowKey::at(&key_cols, row);
        match index.get(&key) {
            Some(&g) => order[g].1.push(row),
            None => {
                index.insert(key.clone(), order.len());
                order.push((key, vec![row]));
            }
        }
    }

    let mut out_keys: Vec<Vec<Value>> = vec![Vec::with_capacity(order.len()); keys.len()];
    let mut out_agg: Vec<Value> = Vec::with_capacity(order.len());
    for (_, rows) in &order {
        for (k, kc) in key_cols.iter().enumerate() {
            out_keys[k].push(kc.get(rows[0]));
        }
        out_agg.push(aggregate(agg, vals, rows, line, col)?);
    }

    let mut cols: Vec<(String, Col)> = Vec::with_capacity(keys.len() + 1);
    for (k, name) in keys.iter().enumerate() {
        cols.push((name.clone(), Col::from_values(name, &out_keys[k], line, col)?));
    }
    cols.push((value_col.to_string(), Col::from_values(value_col, &out_agg, line, col)?));
    NativeFrame::new(cols, line, col)
}

/// One group's aggregate. Missing propagation happens HERE (spec: any missing in
/// the group makes every aggregation but `count` answer missing).
fn aggregate(
    agg: &str,
    vals: &Col,
    rows: &[usize],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    if agg == "count" {
        return Ok(Value::Int(rows.len() as i64));
    }
    let cells: Vec<Value> = rows.iter().map(|&r| vals.get(r)).collect();
    if cells.iter().any(|v| matches!(v, Value::Missing)) {
        return Ok(Value::Missing);
    }
    // All cells present; numeric aggregations promote Int to Float where the
    // operation demands it (mean/std), and keep Int for sum/min/max of Ints —
    // the same shapes the whole-column methods answer.
    let all_int = cells.iter().all(|v| matches!(v, Value::Int(_)));
    let as_f = |v: &Value| match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(x) => Ok(*x),
        other => Err(HelixError::new(
            format!("cannot aggregate a column of type {}", other.type_name()),
            line,
            col,
        )),
    };
    match agg {
        "sum" => {
            if all_int {
                let mut s: i64 = 0;
                for v in &cells {
                    if let Value::Int(i) = v {
                        s = s.wrapping_add(*i);
                    }
                }
                Ok(Value::Int(s))
            } else {
                let mut s = 0.0f64;
                for v in &cells {
                    s += as_f(v)?;
                }
                Ok(Value::Float(s))
            }
        }
        "mean" => {
            let mut s = 0.0f64;
            for v in &cells {
                s += as_f(v)?;
            }
            Ok(Value::Float(s / cells.len() as f64))
        }
        "min" | "max" => {
            let want_min = agg == "min";
            if all_int {
                let mut best = match &cells[0] {
                    Value::Int(i) => *i,
                    _ => unreachable!(),
                };
                for v in &cells[1..] {
                    if let Value::Int(i) = v
                        && ((want_min && *i < best) || (!want_min && *i > best))
                    {
                        best = *i;
                    }
                }
                Ok(Value::Int(best))
            } else {
                let mut best = as_f(&cells[0])?;
                for v in &cells[1..] {
                    let x = as_f(v)?;
                    if (want_min && x < best) || (!want_min && x > best) {
                        best = x;
                    }
                }
                Ok(Value::Float(best))
            }
        }
        "std" => {
            // Sample std (ddof 1), two-pass — deterministic, and a single-element
            // group divides by zero into missing (unknown spread), matching the
            // oracle's null there.
            if cells.len() < 2 {
                return Ok(Value::Missing);
            }
            let n = cells.len() as f64;
            let mut s = 0.0f64;
            for v in &cells {
                s += as_f(v)?;
            }
            let m = s / n;
            let mut ss = 0.0f64;
            for v in &cells {
                let d = as_f(v)? - m;
                ss += d * d;
            }
            Ok(Value::Float((ss / (n - 1.0)).sqrt()))
        }
        _ => unreachable!("agg validated by the caller"),
    }
}

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
            .hint("try mean, sum, min, max, count, std, median, first, or nunique — or `agg({...})` for several at once."));
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
    // A NaN propagates as NaN, exactly as it does for a whole column or an array
    // (ADR 0036 policy 4). `min`/`max` used to SKIP it -- `x < best` is false for a
    // NaN, so it simply never became the best -- which is the pandas `skipna` default
    // that ADR 0025:132 wrote down as a red line and that both backends then shipped
    // in the frame world. `count` returns above this, so it still counts every row.
    // `missing` is checked first: absence is the weaker claim.
    if cells.iter().any(|v| matches!(v, Value::Float(f) if f.is_nan())) {
        return Ok(Value::Float(f64::NAN));
    }
    // Strings order in scalar Helix ("a" < "b"), so a String column answers
    // lexical min/max — the polars backend already did; native refused (sweep).
    // First-cell check up front so numeric groups pay one discriminant test,
    // not a full pass, before their own kernel.
    if matches!(agg, "min" | "max")
        && matches!(cells.first(), Some(Value::Str(_)))
        && cells.iter().all(|v| matches!(v, Value::Str(_)))
    {
        let mut best = 0usize;
        for i in 1..cells.len() {
            if let (Value::Str(a), Value::Str(b)) = (&cells[i], &cells[best]) {
                let better = if agg == "min" { a < b } else { a > b };
                if better {
                    best = i;
                }
            }
        }
        return Ok(cells[best].clone());
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

/// Several aggregates in ONE pass (field build, 1.37): the groups are discovered once, each
/// spec's column expression is evaluated once over the frame (the `with` evaluator — typed
/// fast path first, the boxed kernel as the semantics), and every group folds every spec.
/// The frame is the keys followed by one column per spec, in the order written.
pub fn group_agg_many(
    frame: &NativeFrame,
    keys: &[String],
    aggs: &[crate::backend::AggSpec],
    line: usize,
    col: usize,
) -> Result<NativeFrame, HelixError> {
    use crate::backend::AggKind;
    let key_cols: Vec<&Col> =
        keys.iter().map(|k| frame.col(k, line, col)).collect::<Result<_, _>>()?;
    let n = frame.len();
    // Each spec's values: a materialized column per expression, or none for `count`.
    let mut spec_cols: Vec<Option<Col>> = Vec::with_capacity(aggs.len());
    for spec in aggs {
        spec_cols.push(match &spec.expr {
            None => None,
            Some(expr) => Some(if let Some(r) = super::fast::eval_typed(frame, expr, line, col) {
                r?
            } else {
                let cells = super::eval::eval(frame, expr, line, col)?.into_rows(n);
                Col::from_values(&spec.name, &cells, line, col)?
            }),
        });
    }
    // First-seen group order, exactly as `group_agg`.
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
    let mut out_aggs: Vec<Vec<Value>> = vec![Vec::with_capacity(order.len()); aggs.len()];
    for (_, rows) in &order {
        for (k, kc) in key_cols.iter().enumerate() {
            out_keys[k].push(kc.get(rows[0]));
        }
        for (i, spec) in aggs.iter().enumerate() {
            out_aggs[i].push(match (spec.kind, &spec_cols[i]) {
                (AggKind::Count, _) => Value::Int(rows.len() as i64),
                (kind, Some(vals)) => aggregate_kind(kind, vals, rows, line, col)?,
                // Unreachable by construction (`count` is the one kind without a column,
                // handled above); answering `missing` keeps this total rather than panicking.
                (_, None) => Value::Missing,
            });
        }
    }
    let mut cols: Vec<(String, Col)> = Vec::with_capacity(keys.len() + aggs.len());
    for (k, name) in keys.iter().enumerate() {
        cols.push((name.clone(), Col::from_values(name, &out_keys[k], line, col)?));
    }
    for (i, spec) in aggs.iter().enumerate() {
        cols.push((spec.name.clone(), Col::from_values(&spec.name, &out_aggs[i], line, col)?));
    }
    NativeFrame::new(cols, line, col)
}

/// One group's aggregate by kind: the six the single-column verbs have always answered go
/// through [`aggregate`]; `median`, `first` and `nunique` are defined here, under the same
/// doctrine (a `missing` in the group makes the answer missing — except `first`, whose
/// answer is the first row's value and is knowable; a NaN propagates for the numeric ones).
fn aggregate_kind(
    kind: crate::backend::AggKind,
    vals: &Col,
    rows: &[usize],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    use crate::backend::AggKind;
    match kind {
        AggKind::First => Ok(rows.first().map(|&r| vals.get(r)).unwrap_or(Value::Missing)),
        AggKind::Median => {
            let cells: Vec<Value> = rows.iter().map(|&r| vals.get(r)).collect();
            if cells.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            if cells.iter().any(|v| matches!(v, Value::Float(f) if f.is_nan())) {
                return Ok(Value::Float(f64::NAN));
            }
            let mut xs: Vec<f64> = Vec::with_capacity(cells.len());
            for v in &cells {
                xs.push(match v {
                    Value::Int(i) => *i as f64,
                    Value::Float(x) => *x,
                    other => {
                        return Err(HelixError::new(
                            format!("cannot take the median of a column of type {}", other.type_name()),
                            line,
                            col,
                        ))
                    }
                });
            }
            if xs.is_empty() {
                return Ok(Value::Missing);
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let m = xs.len() / 2;
            Ok(Value::Float(if xs.len() % 2 == 1 { xs[m] } else { (xs[m - 1] + xs[m]) / 2.0 }))
        }
        AggKind::Nunique => {
            let cells: Vec<Value> = rows.iter().map(|&r| vals.get(r)).collect();
            if cells.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            // Distinct by VALUE equality: an Int, a Bool, a String, or a Float by its bits
            // with -0.0 folded into 0.0 (`0.0 == -0.0` holds) and every NaN one value. A
            // Dict key would refuse a Float, and a group of floats is the common case.
            #[derive(PartialEq, Eq, PartialOrd, Ord)]
            enum Distinct {
                Bool(bool),
                Int(i64),
                Float(u64),
                Str(String),
            }
            let mut seen: std::collections::BTreeSet<Distinct> = std::collections::BTreeSet::new();
            for v in &cells {
                let k = match v {
                    Value::Bool(b) => Distinct::Bool(*b),
                    Value::Int(i) => Distinct::Int(*i),
                    Value::Float(x) => Distinct::Float(if x.is_nan() {
                        f64::NAN.to_bits()
                    } else if *x == 0.0 {
                        0.0f64.to_bits()
                    } else {
                        x.to_bits()
                    }),
                    Value::Str(s) => Distinct::Str((**s).clone()),
                    other => {
                        return Err(HelixError::new(
                            format!("cannot count distinct values of type {}", other.type_name()),
                            line,
                            col,
                        ))
                    }
                };
                seen.insert(k);
            }
            Ok(Value::Int(seen.len() as i64))
        }
        other => aggregate(other.label(), vals, rows, line, col),
    }
}

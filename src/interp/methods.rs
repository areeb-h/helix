//! Value-method dispatch (`call_method`) and the per-type method implementations
//! for arrays, strings, and DNA, plus the shared numeric helpers (Neumaier
//! compensated summation, population standard deviation). These are free
//! functions shared by both the tree-walker and the bytecode VM — the parent
//! module re-exports them, so `crate::interp::call_method` still resolves.

use super::*;
use std::rc::Rc;

use crate::error::{suggest, HelixError};
use crate::value::Value;


pub(crate) fn call_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // `is_missing` is universal: true only for the `missing` value itself.
    if name == "is_missing" {
        if !args.is_empty() {
            return Err(HelixError::new("`is_missing` takes no arguments", line, col));
        }
        return Ok(Value::Bool(matches!(recv, Value::Missing)));
    }
    match recv {
        Value::Array(items) => match array_numeric_fast(items, name, &args, line, col)? {
            // A typed array's numeric reduction reads the packed buffer directly.
            Some(v) => Ok(v),
            // Everything else materializes to `Value`s and runs the general path.
            None => array_method(&items.to_values(), name, &args, line, col),
        },
        Value::Str(s) => string_method(s, name, &args, line, col),
        Value::Dna(s) => dna_method(s, name, &args, line, col),
        Value::Tensor(t) => crate::tensor::method(t, name, &args, line, col),
        Value::PyObject(h) => crate::python::method(h, name, &args, line, col),
        other => Err(HelixError::new(
            format!("a {} has no method `{}`", other.type_name(), name),
            line,
            col,
        )),
    }
}

fn numeric_vec(items: &[Value], who: &str, line: usize, col: usize) -> Result<Vec<f64>, HelixError> {
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.iter().enumerate() {
        match v.as_f64() {
            Some(x) => out.push(x),
            None => {
                return Err(HelixError::new(
                    format!(
                        "`{}` needs an array of numbers, but element {} is a {}",
                        who,
                        i,
                        v.type_name()
                    ),
                    line,
                    col,
                ))
            }
        }
    }
    Ok(out)
}

/// True if any element is `missing` *or* a `NaN` float — every numeric aggregation
/// propagates both as `missing` (ADR-0001). `NaN` is "not a number" and, being
/// unordered, would otherwise silently corrupt sort-based stats (a stray `NaN`
/// lands at an arbitrary position, giving a wrong median/quantile). `inf` is left
/// alone: it orders correctly and yields a well-defined (if extreme) result.
fn missing_or_nan(items: &[Value]) -> bool {
    items
        .iter()
        .any(|v| matches!(v, Value::Missing) || matches!(v, Value::Float(f) if f.is_nan()))
}

/// Order two numeric `Value`s, comparing two `Int`s **exactly** rather than via
/// their `f64` widening. Widening collapses distinct `i64`s above 2^53 to one
/// value, which made the boxed `min`/`max`/`sort` path pick the wrong element and
/// disagree with the exact packed-`Int` path; an `i64`-direct compare keeps them in
/// lock-step. Callers guarantee both values are numeric (`Int`/`Float`).
fn numeric_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        _ => a
            .as_f64()
            .unwrap_or(f64::NAN)
            .partial_cmp(&b.as_f64().unwrap_or(f64::NAN))
            .unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// Numeric-reduction fast path for **typed** arrays (`Ints`/`Floats`): read the
/// packed buffer directly, never materializing a `Vec<Value>`. Returns `Ok(None)`
/// for a `Values` array, a non-reduction method, an argument-bearing call, or a
/// `Float` array containing `NaN` — so the caller's general, missing/NaN-aware path
/// runs and the result matches the untyped array exactly. Typed arrays are
/// missing-free by construction, so no missing check is needed here.
fn array_numeric_fast(
    ad: &crate::value::ArrayData,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Option<Value>, HelixError> {
    use crate::value::ArrayData;
    if !matches!(
        name,
        "count" | "sum" | "mean" | "std" | "var" | "median" | "min" | "max"
    ) || !args.is_empty()
    {
        return Ok(None);
    }
    match ad {
        ArrayData::Values(_) => Ok(None),
        ArrayData::Ints(xs) => array_int_reduce(xs, name, line, col).map(Some),
        ArrayData::Floats(xs) => {
            // A `NaN` flips the answer to `missing` under ADR-0001; defer so the
            // general path matches the untyped result exactly.
            if xs.iter().any(|x| x.is_nan()) {
                Ok(None)
            } else {
                array_float_reduce(xs, name, line, col).map(Some)
            }
        }
    }
}

fn array_int_reduce(xs: &[i64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
        "count" => Ok(Value::Int(xs.len() as i64)),
        "sum" => {
            // i128 accumulate; stay exact `Int` if it fits, else compensated `Float`.
            let wide: i128 = xs.iter().map(|&n| n as i128).sum();
            Ok(match i64::try_from(wide) {
                Ok(n) => Value::Int(n),
                Err(_) => {
                    let fs: Vec<f64> = xs.iter().map(|&n| n as f64).collect();
                    Value::Float(neumaier_sum(&fs))
                }
            })
        }
        "min" | "max" => {
            if xs.is_empty() {
                empty_guard(&Vec::<f64>::new(), name, line, col)?;
            }
            let best = if name == "min" {
                *xs.iter().min().unwrap()
            } else {
                *xs.iter().max().unwrap()
            };
            Ok(Value::Int(best))
        }
        // mean/std/var/median: widen to f64 (still half a `Vec<Value>`).
        _ => {
            let fs: Vec<f64> = xs.iter().map(|&n| n as f64).collect();
            float_stat(&fs, name, line, col)
        }
    }
}

fn array_float_reduce(xs: &[f64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
        "count" => Ok(Value::Int(xs.len() as i64)),
        "sum" => Ok(Value::Float(neumaier_sum(xs))),
        "min" | "max" => {
            if xs.is_empty() {
                empty_guard(&Vec::<f64>::new(), name, line, col)?;
            }
            let mut best = xs[0];
            for &x in &xs[1..] {
                if (name == "min" && x < best) || (name == "max" && x > best) {
                    best = x;
                }
            }
            Ok(Value::Float(best))
        }
        _ => float_stat(xs, name, line, col),
    }
}

/// Shared `f64` reductions (`mean`/`std`/`var`/`median`) — identical kernels to the
/// general `array_method` path, so a typed array's result matches the untyped one.
fn float_stat(xs: &[f64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    empty_guard(xs, name, line, col)?;
    Ok(match name {
        "mean" => Value::Float(neumaier_sum(xs) / xs.len() as f64),
        "std" => Value::Float(population_std(xs)),
        "var" => Value::Float(crate::stats::variance(xs)),
        "median" => Value::Float(crate::stats::median(xs)),
        _ => unreachable!("float_stat only handles mean/std/var/median"),
    })
}

fn array_method(
    items: &[Value],
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let no_args = |n: &str| {
        if args.is_empty() {
            Ok(())
        } else {
            Err(HelixError::new(
                format!("`{}` takes no arguments, got {}", n, args.len()),
                line,
                col,
            ))
        }
    };

    match name {
        "count" => {
            no_args(name)?;
            // Counts every slot, including `missing` holes.
            Ok(Value::Int(items.len() as i64))
        }
        "mean" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "mean", line, col)?;
            empty_guard(&xs, "mean", line, col)?;
            Ok(Value::Float(neumaier_sum(&xs) / xs.len() as f64))
        }
        "std" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "std", line, col)?;
            empty_guard(&xs, "std", line, col)?;
            Ok(Value::Float(population_std(&xs)))
        }
        "median" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "median", line, col)?;
            empty_guard(&xs, "median", line, col)?;
            Ok(Value::Float(crate::stats::median(&xs)))
        }
        "var" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "var", line, col)?;
            empty_guard(&xs, "var", line, col)?;
            Ok(Value::Float(crate::stats::variance(&xs)))
        }
        "quantile" => {
            // One argument: the probability `p` in [0, 1] (e.g. `xs.quantile(0.95)`).
            if args.len() != 1 {
                return Err(HelixError::new(
                    format!("`quantile` takes one probability in [0, 1], got {}", args.len()),
                    line,
                    col,
                )
                .hint("e.g. `xs.quantile(0.95)` for the 95th percentile."));
            }
            let p = match args[0].as_f64() {
                Some(p) => p,
                None => return Err(type_err("quantile", "a number in [0, 1]", &args[0], line, col)),
            };
            if !(0.0..=1.0).contains(&p) {
                return Err(HelixError::new(
                    format!("`quantile` needs a probability in [0, 1], got {}", p),
                    line,
                    col,
                )
                .hint("0 is the minimum, 0.5 the median, 1 the maximum."));
            }
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "quantile", line, col)?;
            empty_guard(&xs, "quantile", line, col)?;
            Ok(Value::Float(crate::stats::quantile(&xs, p)))
        }
        "summary" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let mut xs = numeric_vec(items, "summary", line, col)?;
            empty_guard(&xs, "summary", line, col)?;
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // A descriptive overview (the `describe()` analogue): count, central
            // tendency, spread, and the three order-statistic extremes/center.
            let fields = vec![
                (Symbol::intern("count"), Value::Int(xs.len() as i64)),
                (Symbol::intern("mean"), Value::Float(crate::stats::mean(&xs))),
                (Symbol::intern("std"), Value::Float(crate::stats::std(&xs))),
                (Symbol::intern("min"), Value::Float(xs[0])),
                (Symbol::intern("median"), Value::Float(crate::stats::quantile_sorted(&xs, 0.5))),
                (Symbol::intern("max"), Value::Float(xs[xs.len() - 1])),
            ];
            Ok(Value::Record(Rc::new(fields)))
        }
        "sum" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            // Keep Int if every element is an Int; otherwise compensated float sum.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                // Accumulate in i128 so a total that exceeds i64 neither panics
                // (debug) nor silently wraps (release): stay exact `Int` when it
                // fits, else promote to a compensated `Float` — mirroring `**`'s
                // Int→Float overflow promotion, so a large sum is never wrong.
                let wide: i128 = items
                    .iter()
                    .map(|v| if let Value::Int(i) = v { *i as i128 } else { 0 })
                    .sum();
                match i64::try_from(wide) {
                    Ok(n) => Ok(Value::Int(n)),
                    Err(_) => {
                        let xs: Vec<f64> = items
                            .iter()
                            .map(|v| if let Value::Int(i) = v { *i as f64 } else { 0.0 })
                            .collect();
                        Ok(Value::Float(neumaier_sum(&xs)))
                    }
                }
            } else {
                let xs = numeric_vec(items, "sum", line, col)?;
                Ok(Value::Float(neumaier_sum(&xs)))
            }
        }
        "min" | "max" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            // `numeric_vec` validates (all-numeric) and powers `empty_guard`, but the
            // selection compares the original `Value`s EXACTLY via `numeric_cmp` — not
            // their f64 widening, which would collapse two i64 above 2^53 to the same
            // value and pick the wrong element (and disagree with the packed Int path).
            let xs = numeric_vec(items, name, line, col)?;
            empty_guard(&xs, name, line, col)?;
            let mut best_idx = 0;
            for i in 1..items.len() {
                let ord = numeric_cmp(&items[i], &items[best_idx]);
                let better = if name == "min" {
                    ord == std::cmp::Ordering::Less
                } else {
                    ord == std::cmp::Ordering::Greater
                };
                if better {
                    best_idx = i;
                }
            }
            Ok(items[best_idx].clone())
        }
        "normalize" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "normalize", line, col)?;
            empty_guard(&xs, "normalize", line, col)?;
            let mean = neumaier_sum(&xs) / xs.len() as f64;
            let sd = population_std(&xs);
            if sd == 0.0 {
                return Err(HelixError::new(
                    "cannot normalize: all values are identical (standard deviation is 0)",
                    line,
                    col,
                )
                .hint("normalize rescales by spread; a constant column has no spread."));
            }
            let out: Vec<Value> = xs.iter().map(|x| Value::Float((x - mean) / sd)).collect();
            Ok(Value::array(out))
        }
        "drop_missing" => {
            no_args(name)?;
            // Common case: nothing to drop → share the input array (an `Rc` bump,
            // zero allocation) instead of copying every element into a new `Vec`.
            if !items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::array(items.to_vec()));
            }
            let out: Vec<Value> = items
                .iter()
                .filter(|v| !matches!(v, Value::Missing))
                .cloned()
                .collect();
            Ok(Value::array(out))
        }
        "sort" => {
            no_args(name)?;
            let mut sorted: Vec<Value> = items.to_vec();
            // numeric sort if all numeric, else lexical if all strings
            if items.iter().all(|v| v.as_f64().is_some()) {
                // Exact compare (see `numeric_cmp`) so two i64 above 2^53 keep their
                // distinct order instead of collapsing through f64.
                sorted.sort_by(numeric_cmp);
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else if items.iter().all(|v| matches!(v, Value::Dna(_))) {
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Dna(x), Value::Dna(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                return Err(HelixError::new(
                    "`sort` needs an array of all numbers, all strings, or all DNA",
                    line,
                    col,
                ));
            }
            Ok(Value::array(sorted))
        }
        "join" => {
            arity("join", args, 1, line, col)?;
            let sep = str_arg(args, 0, "join", line, col)?;
            // Each element is rendered with its normal display (a string element is
            // its raw text, not a quoted form), then joined: `[1,2,3].join("-")`.
            let joined = items.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(sep);
            Ok(Value::Str(Rc::new(joined)))
        }
        "reverse" => {
            no_args(name)?;
            let mut v: Vec<Value> = items.to_vec();
            v.reverse();
            Ok(Value::array(v))
        }
        "first" | "last" => {
            no_args(name)?;
            if items.is_empty() {
                return Err(HelixError::new(
                    format!("cannot take `{}` of an empty array", name),
                    line,
                    col,
                ));
            }
            let idx = if name == "first" { 0 } else { items.len() - 1 };
            Ok(items[idx].clone())
        }
        "take" => {
            arity("take", args, 1, line, col)?;
            let n = as_int(&args[0], "take", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().take(n).cloned().collect();
            Ok(Value::array(out))
        }
        "drop" => {
            arity("drop", args, 1, line, col)?;
            let n = as_int(&args[0], "drop", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().skip(n).cloned().collect();
            Ok(Value::array(out))
        }
        "zip" => {
            arity("zip", args, 1, line, col)?;
            let other = match &args[0] {
                Value::Array(a) => a.to_values().into_owned(),
                v => {
                    return Err(HelixError::new(
                        format!("`zip` needs an array, but got a {}", v.type_name()),
                        line,
                        col,
                    )
                    .hint("e.g. `xs.zip(ys)` pairs elements positionally."))
                }
            };
            let n = items.len().min(other.len());
            let out: Vec<Value> = (0..n)
                .map(|i| Value::Tuple(Rc::new(vec![items[i].clone(), other[i].clone()])))
                .collect();
            Ok(Value::array(out))
        }
        "enumerate" => {
            no_args(name)?;
            let out: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(i, v)| Value::Tuple(Rc::new(vec![Value::Int(i as i64), v.clone()])))
                .collect();
            Ok(Value::array(out))
        }
        "top" => {
            arity("top", args, 1, line, col)?;
            let n = as_int(&args[0], "top", line, col)?.max(0) as usize;
            let out: Vec<Value> = value_histogram(items)
                .into_iter()
                .take(n)
                .map(|(v, c)| Value::Tuple(Rc::new(vec![v, Value::Int(c)])))
                .collect();
            Ok(Value::array(out))
        }
        "frequencies" => {
            // The full value-count histogram as `(value, count)` pairs (count desc,
            // value asc) — `top` without the limit. For k-mer spectra etc.
            no_args(name)?;
            let out: Vec<Value> = value_histogram(items)
                .into_iter()
                .map(|(v, c)| Value::Tuple(Rc::new(vec![v, Value::Int(c)])))
                .collect();
            Ok(Value::array(out))
        }
        "unique" => {
            // Distinct values in first-seen order. O(n) for string/DNA arrays.
            no_args(name)?;
            let mut out: Vec<Value> = Vec::new();
            if items.iter().all(|v| matches!(v, Value::Str(_) | Value::Dna(_))) {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for v in items.iter() {
                    if seen.insert(v.to_string()) {
                        out.push(v.clone());
                    }
                }
            } else {
                for v in items.iter() {
                    if !out.iter().any(|u| values_equal(u, v)) {
                        out.push(v.clone());
                    }
                }
            }
            Ok(Value::array(out))
        }
        _ => Err(unknown_method(
            "Array",
            name,
            &crate::registry::methods_of(crate::registry::ARRAY_METHODS),
            line,
            col,
        )),
    }
}

/// Value-count histogram, sorted by count desc then value asc — the shared core
/// of `top`/`frequencies`. String/DNA arrays (k-mer spectra) take a fast ~O(n)
/// hash path; everything else falls back to the value-equality scan (which honors
/// cross-type numeric equality, e.g. `1 == 1.0`), preserving exact semantics.
/// Insertion order is preserved before the sort, matching the old `top`.
fn value_histogram(items: &[Value]) -> Vec<(Value, i64)> {
    let mut counts: Vec<(Value, i64)> = Vec::new();
    if items.iter().all(|v| matches!(v, Value::Str(_) | Value::Dna(_))) {
        let mut idx: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(items.len());
        for v in items.iter() {
            match idx.get(&v.to_string()) {
                Some(&i) => counts[i].1 += 1,
                None => {
                    idx.insert(v.to_string(), counts.len());
                    counts.push((v.clone(), 1));
                }
            }
        }
    } else {
        for v in items.iter() {
            if let Some(e) = counts.iter_mut().find(|(k, _)| values_equal(k, v)) {
                e.1 += 1;
            } else {
                counts.push((v.clone(), 1));
            }
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_string().cmp(&b.0.to_string())));
    counts
}

fn empty_guard(xs: &[f64], who: &str, line: usize, col: usize) -> Result<(), HelixError> {
    if xs.is_empty() {
        Err(HelixError::new(
            format!("cannot compute `{}` of an empty array", who),
            line,
            col,
        ))
    } else {
        Ok(())
    }
}

/// Neumaier's improved Kahan compensated summation — bounds the rounding error of
/// a float sum, recovering terms that naive left-to-right summation would lose to
/// catastrophic cancellation. Every float aggregation routes through it.
pub(crate) fn neumaier_sum(xs: &[f64]) -> f64 {
    let mut sum = 0.0;
    let mut c = 0.0; // running compensation for lost low-order bits
    for &x in xs {
        let t = sum + x;
        if sum.abs() >= x.abs() {
            c += (sum - t) + x;
        } else {
            c += (x - t) + sum;
        }
        sum = t;
    }
    sum + c
}

fn population_std(xs: &[f64]) -> f64 {
    let mean = neumaier_sum(xs) / xs.len() as f64;
    let sq: Vec<f64> = xs.iter().map(|x| (x - mean).powi(2)).collect();
    let var = neumaier_sum(&sq) / xs.len() as f64;
    var.sqrt()
}

fn string_method(
    s: &Rc<String>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // Arity check; methods that take arguments call it with their own count.
    let arity = |n: usize| -> Result<(), HelixError> {
        if args.len() != n {
            return Err(HelixError::new(
                format!(
                    "`{}` expects {} argument{}, got {}",
                    name,
                    n,
                    if n == 1 { "" } else { "s" },
                    args.len()
                ),
                line,
                col,
            ));
        }
        Ok(())
    };
    match name {
        "upper" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.to_uppercase())))
        }
        "lower" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.to_lowercase())))
        }
        "count" => {
            arity(0)?;
            Ok(Value::Int(s.chars().count() as i64))
        }
        "reverse" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.chars().rev().collect())))
        }
        "trim" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.trim().to_string())))
        }
        "split" => {
            arity(1)?;
            let sep = str_arg(args, 0, name, line, col)?;
            if sep.is_empty() {
                return Err(HelixError::new("`split` separator cannot be empty", line, col)
                    .hint("split on a non-empty string, e.g. `s.split(\",\")`."));
            }
            let parts: Vec<Value> =
                s.split(sep).map(|p| Value::Str(Rc::new(p.to_string()))).collect();
            Ok(Value::array(parts))
        }
        "replace" => {
            arity(2)?;
            let from = str_arg(args, 0, name, line, col)?;
            let to = str_arg(args, 1, name, line, col)?;
            Ok(Value::Str(Rc::new(s.replace(from, to))))
        }
        "contains" => {
            arity(1)?;
            Ok(Value::Bool(s.contains(str_arg(args, 0, name, line, col)?)))
        }
        "starts_with" => {
            arity(1)?;
            Ok(Value::Bool(s.starts_with(str_arg(args, 0, name, line, col)?)))
        }
        "ends_with" => {
            arity(1)?;
            Ok(Value::Bool(s.ends_with(str_arg(args, 0, name, line, col)?)))
        }
        _ => Err(unknown_method(
            "String",
            name,
            &crate::registry::methods_of(crate::registry::STRING_METHODS),
            line,
            col,
        )),
    }
}

/// Pull argument `i` as a `&str`, with a clean type error otherwise.
fn str_arg<'a>(
    args: &'a [Value],
    i: usize,
    who: &str,
    line: usize,
    col: usize,
) -> Result<&'a str, HelixError> {
    match &args[i] {
        Value::Str(a) => Ok(a.as_str()),
        other => Err(type_err(who, "a string", other, line, col)),
    }
}

fn dna_method(
    s: &Rc<String>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "length" => {
            if !args.is_empty() {
                return Err(HelixError::new("`length` takes no arguments", line, col));
            }
            Ok(Value::Int(s.len() as i64))
        }
        "gc_content" => {
            if !args.is_empty() {
                return Err(HelixError::new("`gc_content` takes no arguments", line, col));
            }
            if s.is_empty() {
                return Err(HelixError::new(
                    "cannot compute `gc_content` of an empty sequence",
                    line,
                    col,
                ));
            }
            // GC fraction over *called* bases: `N` (unknown) is excluded from the
            // denominator, so `gc_content("GCN") == 1.0`, not 2/3.
            let gc = s.chars().filter(|c| *c == 'G' || *c == 'C').count();
            let called = s.chars().filter(|c| *c != 'N').count();
            Ok(Value::Float(if called == 0 { 0.0 } else { gc as f64 / called as f64 }))
        }
        "complement" => {
            if !args.is_empty() {
                return Err(HelixError::new("`complement` takes no arguments", line, col));
            }
            Ok(Value::Dna(Rc::new(complement(s))))
        }
        "reverse_complement" => {
            if !args.is_empty() {
                return Err(HelixError::new(
                    "`reverse_complement` takes no arguments",
                    line,
                    col,
                ));
            }
            let rc: String = complement(s).chars().rev().collect();
            Ok(Value::Dna(Rc::new(rc)))
        }
        "find" => {
            arity("find", args, 1, line, col)?;
            let needle = match &args[0] {
                Value::Str(p) => (**p).clone(),
                Value::Dna(p) => (**p).clone(),
                v => {
                    return Err(HelixError::new(
                        format!("`find` needs a string or DNA pattern, but got a {}", v.type_name()),
                        line,
                        col,
                    ))
                }
            };
            // ACGT is ASCII, so the byte offset is the base offset.
            match s.find(&needle) {
                Some(idx) => Ok(Value::Int(idx as i64)),
                None => Ok(Value::Missing),
            }
        }
        "kmers" => {
            // The countable k-mer *spectrum*: only windows of unambiguous ACGT —
            // any window containing `N`/IUPAC is skipped (the Jellyfish/KMC/KmerGo
            // convention), so every emitted k-mer round-trips through `dna()` and is
            // canonicalizable. A sequence shorter than `k` (or empty) yields `[]`.
            let k = kmer_k("kmers", args, line, col)?;
            let chars: Vec<char> = s.chars().collect();
            let mut out = Vec::new();
            if k <= chars.len() {
                for w in chars.windows(k) {
                    if w.iter().all(|c| is_acgt(*c)) {
                        out.push(Value::Str(Rc::new(w.iter().collect())));
                    }
                }
            }
            Ok(Value::array(out))
        }
        "windows" => {
            // Every length-`k` substring, faithfully (ambiguity included) — the
            // sequence is reconstructable from its windows. Shorter than `k` → `[]`.
            let k = kmer_k("windows", args, line, col)?;
            let chars: Vec<char> = s.chars().collect();
            let mut out = Vec::new();
            if k <= chars.len() {
                out.reserve(chars.len() - k + 1);
                for w in chars.windows(k) {
                    out.push(Value::Str(Rc::new(w.iter().collect())));
                }
            }
            Ok(Value::array(out))
        }
        _ => Err(unknown_method(
            "Dna",
            name,
            &crate::registry::methods_of(crate::registry::DNA_METHODS),
            line,
            col,
        )),
    }
}

/// The 4 unambiguous DNA bases (the `kmers` spectrum alphabet).
fn is_acgt(c: char) -> bool {
    matches!(c, 'A' | 'C' | 'G' | 'T')
}

/// Parse the single positive-length argument shared by `kmers`/`windows`.
fn kmer_k(name: &str, args: &[Value], line: usize, col: usize) -> Result<usize, HelixError> {
    arity(name, args, 1, line, col)?;
    let k = as_int(&args[0], name, line, col)?;
    if k <= 0 {
        return Err(HelixError::new(
            format!("`{}` needs a positive length, got {}", name, k),
            line,
            col,
        ));
    }
    Ok(k as usize)
}

/// A valid (uppercase) IUPAC nucleotide code: the 4 bases, the 10 two/three-fold
/// ambiguity codes, and `N` (any base). This is the alphabet `dna()` accepts and
/// `read_fasta`/`read_fastq` already produce, so the two paths agree.
pub(crate) fn is_iupac_dna(c: char) -> bool {
    matches!(
        c,
        'A' | 'C' | 'G' | 'T' | 'R' | 'Y' | 'S' | 'W' | 'K' | 'M' | 'B' | 'D' | 'H' | 'V' | 'N'
    )
}

/// IUPAC complement of one (uppercase) base. Ambiguity codes complement to the
/// code for the complementary base set (`R`=A/G → `Y`=C/T, etc.); `S`/`W`/`N` are
/// self-complementary. Unknown chars pass through unchanged (defensive).
fn iupac_complement(c: char) -> char {
    match c {
        'A' => 'T',
        'T' => 'A',
        'C' => 'G',
        'G' => 'C',
        'R' => 'Y',
        'Y' => 'R',
        'K' => 'M',
        'M' => 'K',
        'B' => 'V',
        'V' => 'B',
        'D' => 'H',
        'H' => 'D',
        'S' => 'S',
        'W' => 'W',
        'N' => 'N',
        other => other,
    }
}

fn complement(s: &str) -> String {
    s.chars().map(iupac_complement).collect()
}

fn unknown_method(
    type_name: &str,
    name: &str,
    candidates: &[&str],
    line: usize,
    col: usize,
) -> HelixError {
    let mut err = HelixError::new(
        format!("a {} has no method `{}`", type_name, name),
        line,
        col,
    );
    if let Some(s) = suggest(name, candidates) {
        err = err.hint(format!("did you mean `{}`?", s));
    } else {
        err = err.hint(format!(
            "available {} methods: {}",
            type_name,
            candidates.join(", ")
        ));
    }
    err
}

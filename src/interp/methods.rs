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
        Value::Array(items) => array_method(items, name, &args, line, col),
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

/// True if any element is `missing` — every numeric aggregation propagates it
/// (ADR-0001), returning `missing` rather than a number.
fn has_missing(items: &[Value]) -> bool {
    items.iter().any(|v| matches!(v, Value::Missing))
}

fn array_method(
    items: &Rc<Vec<Value>>,
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
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "mean", line, col)?;
            empty_guard(&xs, "mean", line, col)?;
            Ok(Value::Float(neumaier_sum(&xs) / xs.len() as f64))
        }
        "std" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "std", line, col)?;
            empty_guard(&xs, "std", line, col)?;
            Ok(Value::Float(population_std(&xs)))
        }
        "median" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "median", line, col)?;
            empty_guard(&xs, "median", line, col)?;
            Ok(Value::Float(crate::stats::median(&xs)))
        }
        "var" => {
            no_args(name)?;
            if has_missing(items) {
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
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "quantile", line, col)?;
            empty_guard(&xs, "quantile", line, col)?;
            Ok(Value::Float(crate::stats::quantile(&xs, p)))
        }
        "summary" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let mut xs = numeric_vec(items, "summary", line, col)?;
            empty_guard(&xs, "summary", line, col)?;
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // A descriptive overview (the `describe()` analogue): count, central
            // tendency, spread, and the three order-statistic extremes/center.
            let fields = vec![
                ("count".to_string(), Value::Int(xs.len() as i64)),
                ("mean".to_string(), Value::Float(crate::stats::mean(&xs))),
                ("std".to_string(), Value::Float(crate::stats::std(&xs))),
                ("min".to_string(), Value::Float(xs[0])),
                ("median".to_string(), Value::Float(crate::stats::quantile_sorted(&xs, 0.5))),
                ("max".to_string(), Value::Float(xs[xs.len() - 1])),
            ];
            Ok(Value::Record(Rc::new(fields)))
        }
        "sum" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            // Keep Int if every element is an Int; otherwise compensated float sum.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let s: i64 = items
                    .iter()
                    .map(|v| if let Value::Int(i) = v { *i } else { 0 })
                    .sum();
                Ok(Value::Int(s))
            } else {
                let xs = numeric_vec(items, "sum", line, col)?;
                Ok(Value::Float(neumaier_sum(&xs)))
            }
        }
        "min" | "max" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, name, line, col)?;
            empty_guard(&xs, name, line, col)?;
            let mut best_idx = 0;
            for (i, &x) in xs.iter().enumerate() {
                let better = if name == "min" { x < xs[best_idx] } else { x > xs[best_idx] };
                if better {
                    best_idx = i;
                }
            }
            Ok(items[best_idx].clone())
        }
        "normalize" => {
            no_args(name)?;
            if has_missing(items) {
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
            Ok(Value::Array(Rc::new(out)))
        }
        "drop_missing" => {
            no_args(name)?;
            let out: Vec<Value> = items
                .iter()
                .filter(|v| !matches!(v, Value::Missing))
                .cloned()
                .collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "sort" => {
            no_args(name)?;
            let mut sorted: Vec<Value> = (**items).clone();
            // numeric sort if all numeric, else lexical if all strings
            if items.iter().all(|v| v.as_f64().is_some()) {
                sorted.sort_by(|a, b| {
                    a.as_f64()
                        .unwrap()
                        .partial_cmp(&b.as_f64().unwrap())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                return Err(HelixError::new(
                    "`sort` needs an array of all numbers or all strings",
                    line,
                    col,
                ));
            }
            Ok(Value::Array(Rc::new(sorted)))
        }
        "reverse" => {
            no_args(name)?;
            let mut v: Vec<Value> = (**items).clone();
            v.reverse();
            Ok(Value::Array(Rc::new(v)))
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
            Ok(Value::Array(Rc::new(out)))
        }
        "drop" => {
            arity("drop", args, 1, line, col)?;
            let n = as_int(&args[0], "drop", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().skip(n).cloned().collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "zip" => {
            arity("zip", args, 1, line, col)?;
            let other = match &args[0] {
                Value::Array(a) => a.clone(),
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
            Ok(Value::Array(Rc::new(out)))
        }
        "enumerate" => {
            no_args(name)?;
            let out: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(i, v)| Value::Tuple(Rc::new(vec![Value::Int(i as i64), v.clone()])))
                .collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "top" => {
            arity("top", args, 1, line, col)?;
            let n = as_int(&args[0], "top", line, col)?.max(0) as usize;
            // Frequency count by value equality, ordered by count desc then value asc.
            let mut counts: Vec<(Value, i64)> = Vec::new();
            for v in items.iter() {
                if let Some(e) = counts.iter_mut().find(|(k, _)| values_equal(k, v)) {
                    e.1 += 1;
                } else {
                    counts.push((v.clone(), 1));
                }
            }
            counts.sort_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| a.0.to_string().cmp(&b.0.to_string()))
            });
            let out: Vec<Value> = counts
                .into_iter()
                .take(n)
                .map(|(v, c)| Value::Tuple(Rc::new(vec![v, Value::Int(c)])))
                .collect();
            Ok(Value::Array(Rc::new(out)))
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
    if !args.is_empty() {
        return Err(HelixError::new(
            format!("`{}` takes no arguments, got {}", name, args.len()),
            line,
            col,
        ));
    }
    match name {
        "upper" => Ok(Value::Str(Rc::new(s.to_uppercase()))),
        "lower" => Ok(Value::Str(Rc::new(s.to_lowercase()))),
        "count" => Ok(Value::Int(s.chars().count() as i64)),
        "reverse" => Ok(Value::Str(Rc::new(s.chars().rev().collect()))),
        _ => Err(unknown_method(
            "String",
            name,
            &crate::registry::methods_of(crate::registry::STRING_METHODS),
            line,
            col,
        )),
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
            let gc = s.chars().filter(|c| *c == 'G' || *c == 'C').count();
            Ok(Value::Float(gc as f64 / s.len() as f64))
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
            arity("kmers", args, 1, line, col)?;
            let k = as_int(&args[0], "kmers", line, col)?;
            if k <= 0 {
                return Err(HelixError::new(
                    format!("`kmers` needs a positive length, got {}", k),
                    line,
                    col,
                ));
            }
            let k = k as usize;
            let chars: Vec<char> = s.chars().collect();
            if k > chars.len() {
                return Err(HelixError::new(
                    format!(
                        "k-mer length {} is longer than the sequence (length {})",
                        k,
                        chars.len()
                    ),
                    line,
                    col,
                ));
            }
            let mut out = Vec::with_capacity(chars.len() - k + 1);
            for w in chars.windows(k) {
                out.push(Value::Str(Rc::new(w.iter().collect())));
            }
            Ok(Value::Array(Rc::new(out)))
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

fn complement(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A' => 'T',
            'T' => 'A',
            'C' => 'G',
            'G' => 'C',
            other => other,
        })
        .collect()
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

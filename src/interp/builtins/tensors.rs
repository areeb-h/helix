//! Builtins: tensor constructors and array/tensor bridges — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

pub(super) fn call(name: &str, args: Vec<Value>, line: usize, col: usize) -> Called {
    if !matches!(name, "to_array" | "to_tensor" | "tensor" | "zeros" | "ones" | "eye" | "argmax" | "argmin" | "linspace") {
        return Called::Not(args);
    }
    Called::Done(dispatch(name, args, line, col))
}

fn dispatch(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
                "to_array" => {
                    arity(name, &args, 1, line, col)?;
                    match args.into_iter().next().unwrap() {
                        // A Tensor flattens NATIVELY (row-major) — this was Python-gated,
                        // which put a feature wall exactly between the two halves of a
                        // network: the BLAS tensor path computes at ~177 GFLOPS and the
                        // autodiff tape consumes `[Float]`, and on a stock binary nothing
                        // could cross (the nn field report's top finding). A tracked
                        // tensor's payload crosses the same way, via its VALUE — the tape
                        // is not extended, exactly like `value_of`.
                        Value::Tensor(t) => Ok(Value::float_array(t.iter().copied().collect())),
                        Value::Node(n) => match crate::autodiff::node_value(&n) {
                            Value::Tensor(t) => {
                                Ok(Value::float_array(t.iter().copied().collect()))
                            }
                            other => Ok(Value::array(vec![other])),
                        },
                        // Explicit, on-demand materialization of a Python iterable into a
                        // native Helix Array (the visible escape hatch from
                        // opaque-by-default).
                        other => crate::python::to_array(other, line, col),
                    }
                }
                "to_tensor" => {
                    arity(name, &args, 1, line, col)?;
                    // Bring a Python NumPy f64 array into Helix as a native Tensor.
                    crate::python::to_tensor(args.into_iter().next().unwrap(), line, col)
                }
                // JSON, charts, writers, and format export are now methods (see
                // `interp::methods` / `interp::export_method`): `str.parse_json()`,
                // `value.to_json()`, `xs.bar_chart()`, `data.to_html()`, `df.write_csv(p)`.
                // Reproducible RNG — seeded + pure (same seed → same draws).
                "tensor" => {
                    arity(name, &args, 1, line, col)?;
                    // The scalar→tensor bridge: an argument carrying a tracked value
                    // builds a tracked tensor, so a trainable layer's weights can be
                    // ordinary variables and its forward pass an ordinary `matmul`.
                    // Everything else takes the plain build unchanged — the predicate
                    // is the array's own walk, and a packed buffer answers `false`
                    // without one, so a program that is not differentiating pays
                    // nothing for this.
                    if crate::autodiff::contains_tracked(&args[0]) {
                        return crate::autodiff::tensor_node(&args[0], line, col);
                    }
                    Ok(Value::Tensor(Rc::new(tensor::from_value(&args[0], line, col)?)))
                }
                "zeros" | "ones" => {
                    arity(name, &args, 1, line, col)?;
                    let shape = tensor_shape_arg(&args[0], line, col)?;
                    // Guard the element count (checked, so the product can't overflow
                    // and ask ndarray for an absurd allocation that aborts).
                    const MAX_ELEMS: usize = 1_000_000_000; // ~8 GB of f64
                    let count = shape.iter().try_fold(1usize, |acc, &d| acc.checked_mul(d));
                    if !matches!(count, Some(c) if c <= MAX_ELEMS) {
                        return Err(HelixError::new(
                            format!("tensor shape {:?} is too large to allocate", shape),
                            line,
                            col,
                        )
                        .hint("the total element count must stay under 1 billion."));
                    }
                    let t = if name == "zeros" {
                        tensor::zeros(&shape)
                    } else {
                        tensor::ones(&shape)
                    };
                    Ok(Value::Tensor(Rc::new(t)))
                }
                "eye" => {
                    arity(name, &args, 1, line, col)?;
                    let n = as_int(&args[0], "eye", line, col)?;
                    if n < 0 {
                        return Err(HelixError::new("`eye` needs a non-negative size", line, col));
                    }
                    // Guard the n*n element count (an `eye(40000)` is ~12.8 GB and would
                    // OOM-abort), matching the `zeros`/`ones` cap.
                    let n = n as usize;
                    if !matches!(n.checked_mul(n), Some(c) if c <= 1_000_000_000) {
                        return Err(HelixError::new(
                            format!("`eye({n})` is too large to allocate"),
                            line,
                            col,
                        )
                        .hint("the total element count (n*n) must stay under 1 billion."));
                    }
                    Ok(Value::Tensor(Rc::new(tensor::eye(n))))
                }
                // ---- reverse-mode autodiff (src/autodiff.rs) ----
                "argmax" | "argmin" => {
                    arity(name, &args, 1, line, col)?;
                    let want_max = name == "argmax";
                    let vals: Vec<f64> = match &args[0] {
                        Value::Array(items) => {
                            let vs = items.to_values();
                            // Three-valued propagation (ADR 0001): a `missing` or `NaN`
                            // element makes the arg-extreme undefined, so return `missing`
                            // — matching sum/mean/min/max/median — instead of raising a
                            // type error on `missing` or silently skipping a `NaN`.
                            if vs
                                .iter()
                                .any(|v| matches!(v, Value::Missing) || matches!(v, Value::Float(f) if f.is_nan()))
                            {
                                return Ok(Value::Missing);
                            }
                            let mut out = Vec::with_capacity(vs.len());
                            for v in vs.iter() {
                                out.push(
                                    v.as_f64()
                                        .ok_or_else(|| type_err(name, "an array of numbers", v, line, col))?,
                                );
                            }
                            out
                        }
                        Value::Tensor(t) => {
                            if t.iter().any(|f| f.is_nan()) {
                                return Ok(Value::Missing);
                            }
                            t.iter().copied().collect()
                        }
                        other => {
                            return Err(type_err(name, "an array or tensor of numbers", other, line, col))
                        }
                    };
                    if vals.is_empty() {
                        return Err(HelixError::new(
                            format!("`{name}` of an empty collection"),
                            line,
                            col,
                        ));
                    }
                    let mut best = 0usize;
                    for (i, &x) in vals.iter().enumerate() {
                        if (want_max && x > vals[best]) || (!want_max && x < vals[best]) {
                            best = i;
                        }
                    }
                    Ok(Value::Int(best as i64))
                }
                "linspace" => {
                    // `linspace(start, stop, n)` → n evenly-spaced floats, endpoints
                    // inclusive (the float analogue of `range`, for sampling a domain).
                    arity(name, &args, 3, line, col)?;
                    let f = |v: &Value| -> Result<f64, HelixError> {
                        v.as_f64().ok_or_else(|| type_err("linspace", "a number", v, line, col))
                    };
                    let (start, stop) = (f(&args[0])?, f(&args[1])?);
                    let n = as_int(&args[2], "linspace", line, col)?;
                    if n < 0 {
                        return Err(HelixError::new("`linspace` needs a non-negative count", line, col));
                    }
                    if n as usize > 100_000_000 {
                        return Err(HelixError::new("`linspace` count is too large (limit 100M)", line, col));
                    }
                    let n = n as usize;
                    let out: Vec<f64> = match n {
                        0 => Vec::new(),
                        1 => vec![start],
                        _ => (0..n)
                            .map(|i| start + (i as f64) * (stop - start) / ((n - 1) as f64))
                            .collect(),
                    };
                    Ok(Value::float_array(out))
                }
                // Model-evaluation metrics over two equal-length numeric arrays. `missing`
                // in either propagates (ADR 0001).
        _ => Err(HelixError::new(
            format!("internal: `{name}` routed to the wrong builtin module"),
            line,
            col,
        )),
    }
}

//! The built-in function dispatch (`print`, math, `range`, `dna`, `tensor`,
//! `read_csv`/`read_parquet`/`read_fasta`, `write_parquet`, …) — an `impl Interp`
//! method, split out from the core evaluator. The numeric/shape helper free
//! functions it uses (`broadcast_unary`, `apply_float_fn`, `int_range`, …) stay in
//! the parent module and are reached via `use super::*`.

use super::*;

impl super::Interp {
    pub(crate) fn call_builtin(
        &mut self,
        name: &str,
        args: Vec<Value>,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match name {
            "print" => {
                let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect();
                println!("{}", parts.join(" "));
                Ok(Value::Unit)
            }
            "dna" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => make_dna(s, line, col),
                    other => Err(type_err("dna", "a string", other, line, col)),
                }
            }
            "to_array" => {
                arity(name, &args, 1, line, col)?;
                // Explicit, on-demand materialization of a Python iterable into a
                // native Helix Array (the visible escape hatch from opaque-by-default).
                crate::python::to_array(args.into_iter().next().unwrap(), line, col)
            }
            "to_dataframe" => {
                arity(name, &args, 1, line, col)?;
                // Bring a Python polars/pandas/pyarrow frame into Helix as a native
                // DataFrame, zero-copy via Arrow.
                crate::python::to_dataframe(args.into_iter().next().unwrap(), line, col)
            }
            "to_tensor" => {
                arity(name, &args, 1, line, col)?;
                // Bring a Python NumPy f64 array into Helix as a native Tensor.
                crate::python::to_tensor(args.into_iter().next().unwrap(), line, col)
            }
            "parse_json" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        crate::json::parse(s).map_err(|e| HelixError::new(e, line, col))
                    }
                    other => Err(type_err("parse_json", "a JSON string", other, line, col)),
                }
            }
            "to_json" => {
                arity(name, &args, 1, line, col)?;
                crate::json::stringify(&args[0])
                    .map(|s| Value::Str(Rc::new(s)))
                    .map_err(|e| HelixError::new(e, line, col))
            }
            "http_get" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(url) => {
                        #[cfg(feature = "http")]
                        {
                            let (status, body) =
                                crate::http::get(url).map_err(|e| HelixError::new(e, line, col))?;
                            // `{status, body}` — body is usually fed to `parse_json`.
                            Ok(Value::Record(Rc::new(vec![
                                ("status".to_string(), Value::Int(status)),
                                ("body".to_string(), Value::Str(Rc::new(body))),
                            ])))
                        }
                        #[cfg(not(feature = "http"))]
                        {
                            let _ = url;
                            Err(HelixError::new(
                                "this build has no HTTP support",
                                line,
                                col,
                            )
                            .hint("build without `--no-default-features`, or with `--features http`."))
                        }
                    }
                    other => Err(type_err("http_get", "a URL string", other, line, col)),
                }
            }
            "read_csv" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let lf = dataframe::read_csv(s, line, col)?;
                        Ok(Value::DataFrame(Rc::new(lf)))
                    }
                    other => Err(type_err("read_csv", "a string path", other, line, col)),
                }
            }
            "read_vcf" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let lf = crate::vcf::read_vcf(s, line, col)?;
                        Ok(Value::DataFrame(Rc::new(lf)))
                    }
                    other => Err(type_err("read_vcf", "a string path", other, line, col)),
                }
            }
            "range" => match args.len() {
                1 => {
                    let n = as_int(&args[0], "range", line, col)?;
                    int_range(0, n, line, col)
                }
                2 => {
                    let a = as_int(&args[0], "range", line, col)?;
                    let b = as_int(&args[1], "range", line, col)?;
                    int_range(a, b, line, col)
                }
                _ => Err(HelixError::new(
                    format!("`range` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("use `range(n)` or `range(start, stop)`.")),
            },
            "read_parquet" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let lf = dataframe::read_parquet(s, line, col)?;
                        Ok(Value::DataFrame(Rc::new(lf)))
                    }
                    other => Err(type_err("read_parquet", "a string path", other, line, col)),
                }
            }
            "read_fasta" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => crate::bio::read_fasta(s, line, col),
                    other => Err(type_err("read_fasta", "a string path", other, line, col)),
                }
            }
            "write_parquet" => {
                arity(name, &args, 2, line, col)?;
                match (&args[0], &args[1]) {
                    (Value::DataFrame(lf), Value::Str(p)) => {
                        dataframe::write_parquet(lf, p, line, col)?;
                        Ok(Value::Unit)
                    }
                    (Value::DataFrame(_), other) => {
                        Err(type_err("write_parquet", "a string path", other, line, col))
                    }
                    (other, _) => Err(type_err("write_parquet", "a DataFrame", other, line, col)),
                }
            }
            // ---- tensor constructors ----
            "tensor" => {
                arity(name, &args, 1, line, col)?;
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
                Ok(Value::Tensor(Rc::new(tensor::eye(n as usize))))
            }
            // ---- math standard library (broadcasts over arrays, propagates missing) ----
            "sqrt" | "cbrt" | "exp" | "ln" | "log10" | "log2" | "sin" | "cos" | "tan" | "asin"
            | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "degrees" | "radians" => {
                arity(name, &args, 1, line, col)?;
                let f: fn(f64) -> f64 = match name {
                    "sqrt" => f64::sqrt,
                    "cbrt" => f64::cbrt,
                    "exp" => f64::exp,
                    "ln" => f64::ln,
                    "log10" => f64::log10,
                    "log2" => f64::log2,
                    "sin" => f64::sin,
                    "cos" => f64::cos,
                    "tan" => f64::tan,
                    "asin" => f64::asin,
                    "acos" => f64::acos,
                    "atan" => f64::atan,
                    "sinh" => f64::sinh,
                    "cosh" => f64::cosh,
                    "tanh" => f64::tanh,
                    "degrees" => f64::to_degrees,
                    "radians" => f64::to_radians,
                    _ => unreachable!(),
                };
                apply_float_fn(name, f, &args[0], line, col)
            }
            "floor" | "ceil" | "round" | "trunc" => {
                arity(name, &args, 1, line, col)?;
                let f: fn(f64) -> f64 = match name {
                    "floor" => f64::floor,
                    "ceil" => f64::ceil,
                    "round" => f64::round,
                    "trunc" => f64::trunc,
                    _ => unreachable!(),
                };
                apply_round_fn(name, f, &args[0], line, col)
            }
            "abs" => {
                arity(name, &args, 1, line, col)?;
                broadcast_unary(&args[0], &|s| match s {
                    Value::Int(i) => Ok(Value::Int(i.abs())),
                    Value::Float(x) => Ok(Value::Float(x.abs())),
                    other => Err(type_err("abs", "a number or array of numbers", other, line, col)),
                })
            }
            "sign" => {
                arity(name, &args, 1, line, col)?;
                broadcast_unary(&args[0], &|s| match s {
                    Value::Int(i) => Ok(Value::Int(i.signum())),
                    Value::Float(x) => Ok(Value::Int(if *x > 0.0 {
                        1
                    } else if *x < 0.0 {
                        -1
                    } else {
                        0
                    })),
                    other => Err(type_err("sign", "a number or array of numbers", other, line, col)),
                })
            }
            "log" => {
                arity(name, &args, 2, line, col)?;
                match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((x, base)) => Ok(Value::Float(x.log(base))),
                }
            }
            "atan2" => {
                arity(name, &args, 2, line, col)?;
                match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((y, x)) => Ok(Value::Float(y.atan2(x))),
                }
            }
            "hypot" => {
                arity(name, &args, 2, line, col)?;
                match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((a, b)) => Ok(Value::Float(a.hypot(b))),
                }
            }
            "min" | "max" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let a = args[0]
                    .as_f64()
                    .ok_or_else(|| type_err(name, "a number", &args[0], line, col))?;
                let b = args[1]
                    .as_f64()
                    .ok_or_else(|| type_err(name, "a number", &args[1], line, col))?;
                let pick_first = if name == "min" { a <= b } else { a >= b };
                Ok(if pick_first { args[0].clone() } else { args[1].clone() })
            }
            _ => {
                let mut err =
                    HelixError::new(format!("`{}` is not a known function", name), line, col);
                if let Some(s) = suggest(name, BUILTIN_FNS) {
                    err = err.hint(format!("did you mean `{}`?", s));
                }
                Err(err)
            }
        }
    }
}

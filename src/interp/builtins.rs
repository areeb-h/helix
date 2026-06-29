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
                // Rich rendering on a terminal (tables, color, elision, grouped
                // numbers); byte-identical to the plain `display_value` join when
                // piped/redirected. A DataFrame argument still materializes here, so
                // a failed query is a real error (non-zero exit), never a swallowed
                // placeholder printed as if the program succeeded.
                println!("{}", crate::render::render_print(&args, line, col)?);
                Ok(Value::Unit)
            }
            // `emit(x)` — write one value as a single PLAIN line and FLUSH immediately, so a
            // downstream consumer sees it now rather than at exit. `print` is block-buffered
            // when piped (and rich-formats for a terminal); `emit` is the streaming sink —
            // one value per line, machine-readable (pair with `x.to_json()` for NDJSON).
            "emit" => {
                arity(name, &args, 1, line, col)?;
                use std::io::Write;
                let s = crate::value::display_value(&args[0], line, col)?;
                let mut out = std::io::stdout().lock();
                // Errors writing to a closed pipe are the consumer's business, not a Helix
                // runtime error (a piped reader exiting first is normal); ignore them.
                let _ = writeln!(out, "{s}");
                let _ = out.flush();
                Ok(Value::Unit)
            }
            "assert" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`assert` takes 1 or 2 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                match &args[0] {
                    Value::Bool(true) => Ok(Value::Unit),
                    Value::Bool(false) | Value::Missing => {
                        // A `missing` condition is not provably true → a failed assertion.
                        let msg = match args.get(1) {
                            Some(Value::Str(s)) => format!("assertion failed: {s}"),
                            Some(other) => format!(
                                "assertion failed: {}",
                                crate::value::display_value(other, line, col)?
                            ),
                            None => "assertion failed".to_string(),
                        };
                        Err(HelixError::new(msg, line, col))
                    }
                    other => Err(type_err("assert", "a boolean condition", other, line, col)),
                }
            }
            "assert_eq" => {
                arity(name, &args, 2, line, col)?;
                if values_equal(&args[0], &args[1]) {
                    Ok(Value::Unit)
                } else {
                    let a = crate::value::display_value(&args[0], line, col)?;
                    let b = crate::value::display_value(&args[1], line, col)?;
                    Err(HelixError::new(format!("assertion failed: {a} != {b}"), line, col))
                }
            }
            "assert_close" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(HelixError::new(
                        format!("`assert_close` takes 2 or 3 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let num = |v: &Value| -> Result<f64, HelixError> {
                    match v {
                        Value::Int(i) => Ok(*i as f64),
                        Value::Float(f) => Ok(*f),
                        other => Err(type_err("assert_close", "a number", other, line, col)),
                    }
                };
                let (a, b) = (num(&args[0])?, num(&args[1])?);
                // Default tolerance suits the f64 round-off of typical scientific compute.
                let tol = match args.get(2) {
                    Some(v) => num(v)?,
                    None => 1e-9,
                };
                if (a - b).abs() <= tol {
                    Ok(Value::Unit)
                } else {
                    Err(HelixError::new(
                        format!("assertion failed: {a} is not within {tol} of {b}"),
                        line,
                        col,
                    ))
                }
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
            // JSON, charts, writers, and format export are now methods (see
            // `interp::methods` / `interp::export_method`): `str.parse_json()`,
            // `value.to_json()`, `xs.bar_chart()`, `data.to_html()`, `df.write_csv(p)`.
            // Reproducible RNG — seeded + pure (same seed → same draws).
            "random" => crate::rng::random(&args, line, col),
            "randn" => crate::rng::randn(&args, line, col),
            "random_int" => crate::rng::random_int(&args, line, col),
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
                                (Symbol::intern("status"), Value::Int(status)),
                                (Symbol::intern("body"), Value::Str(Rc::new(body))),
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
                    Value::Str(s) => Ok(Value::dataframe(dataframe::read_csv(s, line, col)?)),
                    other => Err(type_err("read_csv", "a string path", other, line, col)),
                }
            }
            // Generic text/JSON readers — the non-tabular counterpart to read_csv,
            // for reloading config, library definitions, and saved JSON across runs.
            "read_text" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::Str(Rc::new(read_text_file(s, line, col)?))),
                    other => Err(type_err("read_text", "a string path", other, line, col)),
                }
            }
            "read_json" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let text = read_text_file(s, line, col)?;
                        crate::json::parse(&text)
                            .map_err(|m| HelixError::new(format!("invalid JSON in `{s}`: {m}"), line, col))
                    }
                    other => Err(type_err("read_json", "a string path", other, line, col)),
                }
            }
            "file_exists" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::Bool(std::path::Path::new(s.as_str()).exists())),
                    other => Err(type_err("file_exists", "a string path", other, line, col)),
                }
            }
            // List a directory's immediate entries as full (dir-joined) path strings,
            // sorted for a reproducible order — so the result feeds straight into
            // `read_csv`/`read_text` and a program can ingest "whatever files exist".
            "read_dir" => {
                arity(name, &args, 1, line, col)?;
                let dir = match &args[0] {
                    Value::Str(s) => s,
                    other => return Err(type_err("read_dir", "a directory path string", other, line, col)),
                };
                let rd = std::fs::read_dir(dir.as_str()).map_err(|e| {
                    HelixError::new(format!("could not read directory `{dir}`: {e}"), line, col)
                        .hint("check the path exists and is a directory.")
                })?;
                let mut paths = Vec::new();
                for entry in rd {
                    let entry = entry.map_err(|e| {
                        HelixError::new(format!("error reading directory `{dir}`: {e}"), line, col)
                    })?;
                    paths.push(entry.path().to_string_lossy().into_owned());
                }
                paths.sort();
                Ok(Value::array(paths.into_iter().map(|p| Value::Str(Rc::new(p))).collect()))
            }
            // Storage-lifecycle ops. `remove_file` is idempotent (false if the file
            // wasn't there); `mkdir` makes the directory and any parents, returning
            // whether it was newly created. Both error only on a real I/O failure.
            "remove_file" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let p = std::path::Path::new(s.as_str());
                        if p.exists() {
                            std::fs::remove_file(p).map_err(|e| {
                                HelixError::new(format!("could not remove `{s}`: {e}"), line, col)
                            })?;
                            Ok(Value::Bool(true))
                        } else {
                            Ok(Value::Bool(false))
                        }
                    }
                    other => Err(type_err("remove_file", "a string path", other, line, col)),
                }
            }
            "mkdir" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let existed = std::path::Path::new(s.as_str()).exists();
                        std::fs::create_dir_all(s.as_str()).map_err(|e| {
                            HelixError::new(format!("could not create directory `{s}`: {e}"), line, col)
                        })?;
                        Ok(Value::Bool(!existed))
                    }
                    other => Err(type_err("mkdir", "a string path", other, line, col)),
                }
            }
            // Content hash (hex SHA-256) for content-addressing / reproducibility ids.
            "sha256" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        use sha2::{Digest, Sha256};
                        let mut hasher = Sha256::new();
                        hasher.update(s.as_bytes());
                        Ok(Value::Str(Rc::new(format!("{:x}", hasher.finalize()))))
                    }
                    other => Err(type_err("sha256", "a string", other, line, col)),
                }
            }
            "read_vcf" => {
                // `read_vcf(path)` scans; `read_vcf(path, "chr:start-end")` does an
                // indexed region query against the file's `.tbi`.
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`read_vcf` takes 1 or 2 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let path = match &args[0] {
                    Value::Str(s) => s,
                    other => {
                        return Err(type_err("read_vcf", "a string path", other, line, col));
                    }
                };
                let df = match args.get(1) {
                    Some(Value::Str(region)) => {
                        crate::vcf::read_vcf_region(path, region, line, col)?
                    }
                    Some(other) => {
                        return Err(type_err("read_vcf", "a string region", other, line, col));
                    }
                    None => crate::vcf::read_vcf(path, line, col)?,
                };
                Ok(Value::dataframe(df))
            }
            "read_bcf" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(crate::vcf::read_bcf(s, line, col)?)),
                    other => Err(type_err("read_bcf", "a string path", other, line, col)),
                }
            }
            "read_sam" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(crate::sam::read_sam(s, line, col)?)),
                    other => Err(type_err("read_sam", "a string path", other, line, col)),
                }
            }
            "read_bam" => {
                // `read_bam(path)` scans; `read_bam(path, "chr:start-end")` does an
                // indexed region query against the file's `.bai`.
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`read_bam` takes 1 or 2 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let path = match &args[0] {
                    Value::Str(s) => s,
                    other => {
                        return Err(type_err("read_bam", "a string path", other, line, col));
                    }
                };
                let df = match args.get(1) {
                    Some(Value::Str(region)) => {
                        crate::sam::read_bam_region(path, region, line, col)?
                    }
                    Some(other) => {
                        return Err(type_err("read_bam", "a string region", other, line, col));
                    }
                    None => crate::sam::read_bam(path, line, col)?,
                };
                Ok(Value::dataframe(df))
            }
            "read_gff" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(crate::gff::read_gff(s, line, col)?)),
                    other => Err(type_err("read_gff", "a string path", other, line, col)),
                }
            }
            "read_bed" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(crate::bed::read_bed(s, line, col)?)),
                    other => Err(type_err("read_bed", "a string path", other, line, col)),
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
                3 => {
                    let a = as_int(&args[0], "range", line, col)?;
                    let b = as_int(&args[1], "range", line, col)?;
                    let step = as_int(&args[2], "range", line, col)?;
                    int_range_step(a, b, step, line, col)
                }
                _ => Err(HelixError::new(
                    format!("`range` takes 1 to 3 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("use `range(n)`, `range(start, stop)`, or `range(start, stop, step)`.")),
            },
            "read_parquet" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(dataframe::read_parquet(s, line, col)?)),
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
            "read_fastq" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => crate::bio::read_fastq(s, line, col),
                    other => Err(type_err("read_fastq", "a string path", other, line, col)),
                }
            }
            // ---- tensor constructors ----
            "tensor" => {
                arity(name, &args, 1, line, col)?;
                Ok(Value::Tensor(Rc::new(tensor::from_value(&args[0], line, col)?)))
            }
            "dataframe" => {
                // Build a DataFrame from in-memory columns: `dataframe({age: [30, 41],
                // name: ["a", "b"]})`. Each record field is a column (its array); the
                // backend seam (`build_frame`) checks equal lengths / duplicate names.
                arity(name, &args, 1, line, col)?;
                let fields = match &args[0] {
                    Value::Record(f) => f,
                    other => {
                        return Err(type_err("dataframe", "a record of columns", other, line, col)
                            .hint("e.g. `dataframe({age: [30, 41], name: [\"a\", \"b\"]})`."))
                    }
                };
                if fields.is_empty() {
                    return Err(HelixError::new("`dataframe` needs at least one column", line, col)
                        .hint("e.g. `dataframe({age: [30, 41], name: [\"a\", \"b\"]})`."));
                }
                let mut columns = Vec::with_capacity(fields.len());
                for (sym, val) in fields.iter() {
                    let cname = sym.as_str();
                    columns.push((cname.to_string(), array_to_coldata(cname, val, line, col)?));
                }
                Ok(Value::dataframe(crate::backend::build_frame(columns, line, col)?))
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
            "variable" => {
                arity(name, &args, 1, line, col)?;
                crate::autodiff::variable(&args[0], line, col)
            }
            "value_of" => {
                arity(name, &args, 1, line, col)?;
                Ok(crate::autodiff::value_of(&args[0]))
            }
            "gradient" => {
                arity(name, &args, 2, line, col)?;
                crate::autodiff::gradient(&args[0], &args[1], line, col)
            }
            // ---- math standard library (broadcasts over arrays, propagates missing) ----
            "sqrt" | "cbrt" | "exp" | "ln" | "log10" | "log2" | "sin" | "cos" | "tan" | "asin"
            | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "degrees" | "radians" | "erf"
            | "normal_cdf" | "normal_pdf" | "relu" | "sigmoid" => {
                arity(name, &args, 1, line, col)?;
                // A tracked (autodiff) argument builds a graph node instead.
                if matches!(&args[0], Value::Node(_)) {
                    return crate::autodiff::unary_builtin(name, &args[0], line, col);
                }
                let f: fn(f64) -> f64 = match name {
                    "erf" => crate::stats::erf,
                    "normal_cdf" => crate::stats::normal_cdf,
                    "normal_pdf" => crate::stats::normal_pdf,
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
                    "relu" => |x: f64| x.max(0.0),
                    "sigmoid" => |x: f64| 1.0 / (1.0 + (-x).exp()),
                    _ => unreachable!(),
                };
                // Pass the operand by value so a unique buffer can be reused in place.
                apply_float_fn(name, f, args.into_iter().next().unwrap(), line, col)
            }
            // Index of the largest / smallest element (first on ties) — over an array
            // or a tensor (flattened). The classification companion to `softmax`.
            "argmax" | "argmin" => {
                arity(name, &args, 1, line, col)?;
                let want_max = name == "argmax";
                let vals: Vec<f64> = match &args[0] {
                    Value::Array(items) => {
                        let vs = items.to_values();
                        let mut out = Vec::with_capacity(vs.len());
                        for v in vs.iter() {
                            out.push(
                                v.as_f64()
                                    .ok_or_else(|| type_err(name, "an array of numbers", v, line, col))?,
                            );
                        }
                        out
                    }
                    Value::Tensor(t) => t.iter().copied().collect(),
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
            "floor" | "ceil" | "trunc" => {
                arity(name, &args, 1, line, col)?;
                let f: fn(f64) -> f64 = match name {
                    "floor" => f64::floor,
                    "ceil" => f64::ceil,
                    "trunc" => f64::trunc,
                    _ => unreachable!(),
                };
                apply_round_fn(name, f, &args[0], line, col)
            }
            "round" => {
                // `round(x)` → nearest integer (Int); `round(x, d)` → round to `d`
                // decimal places (Float). Both broadcast over arrays and pass `missing`.
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`round` takes a number and an optional digit count, got {} arguments", args.len()),
                        line,
                        col,
                    ));
                }
                if args.len() == 1 {
                    return apply_round_fn("round", f64::round, &args[0], line, col);
                }
                let d = as_int(&args[1], "round", line, col)?;
                let scale = 10f64.powi(d as i32);
                broadcast_unary(&args[0], &|s| match s {
                    Value::Int(i) => Ok(Value::Float(*i as f64)),
                    Value::Float(x) => Ok(Value::Float((x * scale).round() / scale)),
                    other => Err(type_err("round", "a number or array of numbers", other, line, col)),
                })
            }
            "abs" => {
                arity(name, &args, 1, line, col)?;
                // `wrapping_abs` matches the wrapping-on-overflow convention used by the
                // arithmetic ops; a packed array maps over its buffer (no per-element box).
                super::apply_abs(args.into_iter().next().unwrap(), line, col)
            }
            "sign" => {
                arity(name, &args, 1, line, col)?;
                super::apply_sign(&args[0], line, col)
            }
            // IEEE float predicates → Bool (Bool array / 0.0-or-1.0 tensor mask when
            // broadcast). An `Int`/`Rational` is always finite, never NaN/inf. These let a
            // program guard a `NaN`/`inf` (e.g. from an `exp` overflow) BEFORE a comparison
            // — which raises on a non-orderable `NaN` rather than silently mis-ordering.
            "is_nan" => {
                arity(name, &args, 1, line, col)?;
                float_predicate(&args[0], name, f64::is_nan, false, line, col)
            }
            "is_finite" => {
                arity(name, &args, 1, line, col)?;
                float_predicate(&args[0], name, f64::is_finite, true, line, col)
            }
            "is_infinite" => {
                arity(name, &args, 1, line, col)?;
                float_predicate(&args[0], name, f64::is_infinite, false, line, col)
            }
            // Monotonic seconds since the first call (process start), for `t0 =
            // clock_monotonic()` … `clock_monotonic() - t0` timing. Impure + monotonic
            // (never decreases); the absolute value is meaningless, only differences are.
            "clock_monotonic" => {
                arity(name, &args, 0, line, col)?;
                use std::sync::OnceLock;
                use std::time::Instant;
                static START: OnceLock<Instant> = OnceLock::new();
                Ok(Value::Float(START.get_or_init(Instant::now).elapsed().as_secs_f64()))
            }
            "log" => match args.len() {
                // single-arg log(x) = natural log (numpy parity); broadcasts + missing
                1 => apply_float_fn("log", f64::ln, args.into_iter().next().unwrap(), line, col),
                2 => match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((x, base)) => Ok(Value::Float(x.log(base))),
                },
                _ => Err(HelixError::new(
                    format!("`log` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("`log(x)` is the natural log; `log(x, base)` is log to a base.")),
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
            "gcd" => {
                arity(name, &args, 2, line, col)?;
                match (&args[0], &args[1]) {
                    (Value::Missing, _) | (_, Value::Missing) => Ok(Value::Missing),
                    _ => {
                        let a = as_int(&args[0], "gcd", line, col)?;
                        let b = as_int(&args[1], "gcd", line, col)?;
                        Ok(Value::Int(gcd_i64(a, b)))
                    }
                }
            }
            "rational" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let n = as_int(&args[0], "rational", line, col)?;
                let d = as_int(&args[1], "rational", line, col)?;
                if d == 0 {
                    return Err(HelixError::new("`rational` denominator cannot be zero", line, col));
                }
                Ok(Value::Rational(Rc::new(num_rational::BigRational::new(n.into(), d.into()))))
            }
            "numerator" | "denominator" => {
                arity(name, &args, 1, line, col)?;
                use num_traits::ToPrimitive;
                match &args[0] {
                    Value::Rational(r) => {
                        let big = if name == "numerator" { r.numer() } else { r.denom() };
                        big.to_i64().map(Value::Int).ok_or_else(|| {
                            HelixError::new(format!("`{name}` is too large for an integer"), line, col)
                        })
                    }
                    Value::Int(i) => Ok(Value::Int(if name == "numerator" { *i } else { 1 })),
                    Value::Missing => Ok(Value::Missing),
                    other => Err(type_err(name, "a rational or integer", other, line, col)),
                }
            }
            "to_float" => {
                arity(name, &args, 1, line, col)?;
                use num_traits::ToPrimitive;
                match &args[0] {
                    Value::Int(i) => Ok(Value::Float(*i as f64)),
                    Value::Float(f) => Ok(Value::Float(*f)),
                    Value::Rational(r) => Ok(Value::Float(r.to_f64().unwrap_or(f64::NAN))),
                    // A numeric *string* is parsed (the honest spelling of what people
                    // used to reach for `parse_json` to do).
                    Value::Str(s) => parse_str_float(s, line, col),
                    Value::Missing => Ok(Value::Missing),
                    other => Err(type_err("to_float", "a number or numeric string", other, line, col)),
                }
            }
            "dict" => {
                // An empty keyed map (ADR 0020); build a populated one with
                // `[(k, v), …].to_dict()`. Grows via the immutable `.insert(k, v)`.
                if !args.is_empty() {
                    return Err(HelixError::new(
                        "`dict()` takes no arguments",
                        line,
                        col,
                    )
                    .hint("build a populated dict with `[(k, v), …].to_dict()` or `xs.frequencies().to_dict()`."));
                }
                Ok(Value::Dict(Rc::new(std::collections::BTreeMap::new())))
            }
            "to_int" => {
                arity(name, &args, 1, line, col)?;
                use num_traits::ToPrimitive;
                match &args[0] {
                    Value::Int(i) => Ok(Value::Int(*i)),
                    // Truncate toward zero — matches the usual numeric→int narrowing.
                    Value::Float(f) => Ok(Value::Int(f.trunc() as i64)),
                    Value::Rational(r) => Ok(Value::Int(r.to_f64().unwrap_or(f64::NAN).trunc() as i64)),
                    Value::Str(s) => parse_str_int(s, line, col),
                    Value::Missing => Ok(Value::Missing),
                    other => Err(type_err("to_int", "a number or integer string", other, line, col)),
                }
            }
            "lll" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`lll` takes a basis and an optional delta, got {}", args.len()),
                        line,
                        col,
                    )
                    .hint("e.g. `lll(basis)` or `lll(basis, 0.99)`."));
                }
                let rows = match &args[0] {
                    Value::Array(outer) => outer,
                    other => return Err(type_err("lll", "an array of basis vectors", other, line, col)),
                };
                let mut basis: Vec<Vec<f64>> = Vec::with_capacity(rows.len());
                for row in rows.to_values().iter() {
                    match row {
                        Value::Array(inner) => {
                            let mut v = Vec::with_capacity(inner.len());
                            for x in inner.to_values().iter() {
                                match x.as_f64() {
                                    Some(f) => v.push(f),
                                    None => {
                                        return Err(type_err("lll", "numeric basis entries", x, line, col))
                                    }
                                }
                            }
                            basis.push(v);
                        }
                        other => {
                            return Err(type_err("lll", "each basis vector to be an array", other, line, col))
                        }
                    }
                }
                let delta = match args.get(1) {
                    Some(d) => d
                        .as_f64()
                        .ok_or_else(|| type_err("lll", "a numeric delta", &args[1], line, col))?,
                    None => 0.75,
                };
                let reduced =
                    crate::lattice::lll(basis, delta).map_err(|e| HelixError::new(e, line, col))?;
                let out: Vec<Value> = reduced.into_iter().map(Value::float_array).collect();
                Ok(Value::array(out))
            }
            "lll_exact" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`lll_exact` takes a basis and an optional delta, got {}", args.len()),
                        line,
                        col,
                    )
                    .hint("e.g. `lll_exact(basis)` or `lll_exact(basis, 0.99)`."));
                }
                let rows = match &args[0] {
                    Value::Array(outer) => outer,
                    other => {
                        return Err(type_err("lll_exact", "an array of basis vectors", other, line, col))
                    }
                };
                // Coerce every entry to an exact integer (Int, or integer-valued
                // Float/Rational); a fractional entry is a clean error — exact LLL is
                // defined on an integer lattice.
                let mut basis: Vec<Vec<num_bigint::BigInt>> = Vec::with_capacity(rows.len());
                for row in rows.to_values().iter() {
                    match row {
                        Value::Array(inner) => {
                            let mut v = Vec::with_capacity(inner.len());
                            for x in inner.to_values().iter() {
                                v.push(num_bigint::BigInt::from(as_int(x, "lll_exact", line, col)?));
                            }
                            basis.push(v);
                        }
                        other => {
                            return Err(type_err(
                                "lll_exact",
                                "each basis vector to be an array",
                                other,
                                line,
                                col,
                            ))
                        }
                    }
                }
                let delta = match args.get(1) {
                    Some(d) => d
                        .as_f64()
                        .ok_or_else(|| type_err("lll_exact", "a numeric delta", &args[1], line, col))?,
                    None => 0.75,
                };
                let reduced = crate::lattice::lll_exact(basis, delta)
                    .map_err(|e| HelixError::new(e, line, col))?;
                use num_traits::ToPrimitive;
                let mut out: Vec<Value> = Vec::with_capacity(reduced.len());
                for row in reduced {
                    let mut iv = Vec::with_capacity(row.len());
                    for x in row {
                        let i = x.to_i64().ok_or_else(|| {
                            HelixError::new(
                                "`lll_exact` reduced-basis entry overflows a 64-bit integer",
                                line,
                                col,
                            )
                        })?;
                        iv.push(Value::Int(i));
                    }
                    out.push(Value::array(iv));
                }
                Ok(Value::array(out))
            }
            "align" => {
                if args.len() < 2 || args.len() > 4 {
                    return Err(HelixError::new(
                        format!(
                            "`align` takes (a, b), (a, b, mode), or (a, b, mode, scoring), got {} arguments",
                            args.len()
                        ),
                        line,
                        col,
                    )
                    .hint("e.g. `align(a, b)`, `align(a, b, \"local\")`, or `align(a, b, \"global\", {match: 2, mismatch: -3, gap_open: -5, gap_extend: -1})`."));
                }
                let seq = |v: &Value| -> Result<Vec<Value>, HelixError> {
                    match v {
                        Value::Array(arr) => Ok(arr.to_values().into_owned()),
                        other => Err(type_err("align", "an array", other, line, col)),
                    }
                };
                let a = seq(&args[0])?;
                let b = seq(&args[1])?;
                // The Gotoh DP fills six O(n·m) tables (~27 bytes/cell at i64 scores);
                // cap the cell count so a huge pair fails cleanly instead of trying to
                // allocate gigabytes.
                const MAX_ALIGN_CELLS: u128 = 50_000_000;
                if (a.len() as u128 + 1) * (b.len() as u128 + 1) > MAX_ALIGN_CELLS {
                    return Err(HelixError::new(
                        "`align` sequences are too large (the alignment matrix would exceed 50M cells)",
                        line,
                        col,
                    ));
                }
                // Optional trailing args, in either order: a mode string and/or a
                // scoring record. Scoring defaults to match +1 / mismatch −1 / no
                // gap-open / gap-extend −1; any field may be overridden.
                let mut mode = crate::align::Mode::Global;
                let mut sc =
                    crate::align::Scoring { match_: 1, mismatch: -1, gap_open: 0, gap_extend: -1 };
                let (mut mode_set, mut scoring_set) = (false, false);
                for extra in &args[2..] {
                    match extra {
                        Value::Str(s) => {
                            if mode_set {
                                return Err(HelixError::new("`align` was given two modes", line, col));
                            }
                            mode = match s.as_str() {
                                "global" => crate::align::Mode::Global,
                                "local" => crate::align::Mode::Local,
                                "semiglobal" => crate::align::Mode::Semiglobal,
                                other => {
                                    return Err(HelixError::new(
                                        format!("unknown align mode `{other}`"),
                                        line,
                                        col,
                                    )
                                    .hint("use \"global\", \"local\", or \"semiglobal\"."))
                                }
                            };
                            mode_set = true;
                        }
                        Value::Record(fields) => {
                            if scoring_set {
                                return Err(HelixError::new(
                                    "`align` was given two scoring records",
                                    line,
                                    col,
                                ));
                            }
                            for (k, v) in fields.iter() {
                                let iv = as_int(v, "align scoring", line, col)?;
                                if !(-1_000_000..=1_000_000).contains(&iv) {
                                    return Err(HelixError::new(
                                        "`align` scoring values must be within ±1000000",
                                        line,
                                        col,
                                    ));
                                }
                                match k.as_str() {
                                    "match" => sc.match_ = iv,
                                    "mismatch" => sc.mismatch = iv,
                                    "gap_open" => sc.gap_open = iv,
                                    "gap_extend" => sc.gap_extend = iv,
                                    other => {
                                        return Err(HelixError::new(
                                            format!("unknown align scoring field `{other}`"),
                                            line,
                                            col,
                                        )
                                        .hint("scoring fields: match, mismatch, gap_open, gap_extend."))
                                    }
                                }
                            }
                            scoring_set = true;
                        }
                        other => {
                            return Err(type_err(
                                "align",
                                "a mode string or a scoring record",
                                other,
                                line,
                                col,
                            ))
                        }
                    }
                }
                let (score, steps) =
                    crate::align::align_path(a.len(), b.len(), mode, sc, |i, j| values_equal(&a[i], &b[j]));
                let mut a_al = Vec::with_capacity(steps.len());
                let mut b_al = Vec::with_capacity(steps.len());
                let mut matches = 0i64;
                for st in &steps {
                    match st {
                        crate::align::Step::Both(ia, ib) => {
                            if values_equal(&a[*ia], &b[*ib]) {
                                matches += 1;
                            }
                            a_al.push(a[*ia].clone());
                            b_al.push(b[*ib].clone());
                        }
                        crate::align::Step::OnlyA(ia) => {
                            a_al.push(a[*ia].clone());
                            b_al.push(Value::Missing);
                        }
                        crate::align::Step::OnlyB(ib) => {
                            a_al.push(Value::Missing);
                            b_al.push(b[*ib].clone());
                        }
                    }
                }
                Ok(Value::Record(Rc::new(vec![
                    (Symbol::intern("score"), Value::Int(score)),
                    (Symbol::intern("matches"), Value::Int(matches)),
                    (Symbol::intern("length"), Value::Int(steps.len() as i64)),
                    (Symbol::intern("a_aligned"), Value::array(a_al)),
                    (Symbol::intern("b_aligned"), Value::array(b_al)),
                ])))
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
            "correlation" => {
                arity(name, &args, 2, line, col)?;
                // `missing` in either series propagates (ADR-0001); a non-array, a
                // length mismatch, or a non-numeric element is a clean error.
                let xs = num_array(name, &args[0], line, col)?;
                let ys = num_array(name, &args[1], line, col)?;
                let (xs, ys) = match (xs, ys) {
                    (Some(xs), Some(ys)) => (xs, ys),
                    _ => return Ok(Value::Missing),
                };
                if xs.len() != ys.len() {
                    return Err(HelixError::new(
                        format!(
                            "`correlation` needs two equal-length arrays, got {} and {}",
                            xs.len(),
                            ys.len()
                        ),
                        line,
                        col,
                    ));
                }
                if xs.is_empty() {
                    return Err(HelixError::new(
                        "cannot compute `correlation` of empty arrays",
                        line,
                        col,
                    ));
                }
                match crate::stats::pearson(&xs, &ys) {
                    Some(r) => Ok(Value::Float(r)),
                    None => Err(HelixError::new(
                        "correlation is undefined: one of the series has zero variance",
                        line,
                        col,
                    )
                    .hint("a constant series has no spread to correlate.")),
                }
            }
            "t_test" => {
                arity(name, &args, 2, line, col)?;
                // Welch's two-sample t-test → {statistic, df, p_value}. `missing` in
                // either sample propagates; each needs at least two values.
                let xs = num_array(name, &args[0], line, col)?;
                let ys = num_array(name, &args[1], line, col)?;
                let (xs, ys) = match (xs, ys) {
                    (Some(xs), Some(ys)) => (xs, ys),
                    _ => return Ok(Value::Missing),
                };
                match crate::stats::welch_t_test(&xs, &ys) {
                    Some((t, df, p)) => {
                        let fields = vec![
                            (Symbol::intern("statistic"), Value::Float(t)),
                            (Symbol::intern("df"), Value::Float(df)),
                            (Symbol::intern("p_value"), Value::Float(p)),
                        ];
                        Ok(Value::Record(Rc::new(fields)))
                    }
                    None => Err(HelixError::new(
                        "t-test is undefined: each sample needs at least two values with spread",
                        line,
                        col,
                    )
                    .hint("two constant samples have no variance to compare.")),
                }
            }
            "linear_regression" => {
                arity(name, &args, 2, line, col)?;
                // OLS fit of `y ~ x` → {slope, intercept, r_squared, slope_std_error,
                // slope_p_value}. `missing` in either series propagates.
                let xs = num_array(name, &args[0], line, col)?;
                let ys = num_array(name, &args[1], line, col)?;
                let (xs, ys) = match (xs, ys) {
                    (Some(xs), Some(ys)) => (xs, ys),
                    _ => return Ok(Value::Missing),
                };
                if xs.len() != ys.len() {
                    return Err(HelixError::new(
                        format!(
                            "`linear_regression` needs two equal-length arrays, got {} and {}",
                            xs.len(),
                            ys.len()
                        ),
                        line,
                        col,
                    ));
                }
                let floats = |xs: Vec<f64>| Value::float_array(xs);
                match crate::stats::linear_regression(&xs, &ys) {
                    Some(f) => {
                        let fields = vec![
                            (Symbol::intern("slope"), Value::Float(f.slope)),
                            (Symbol::intern("intercept"), Value::Float(f.intercept)),
                            (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                            (Symbol::intern("slope_std_error"), Value::Float(f.slope_std_error)),
                            (Symbol::intern("slope_p_value"), Value::Float(f.slope_p_value)),
                            (Symbol::intern("rss"), Value::Float(f.rss)),
                            (Symbol::intern("predictions"), floats(f.predictions)),
                            (Symbol::intern("residuals"), floats(f.residuals)),
                        ];
                        Ok(Value::Record(Rc::new(fields)))
                    }
                    None => Err(HelixError::new(
                        "linear regression is undefined: need at least three points and variance in both x and y",
                        line,
                        col,
                    )
                    .hint("a constant predictor or response has no line to fit.")),
                }
            }
            "multiple_regression" => {
                // OLS fit of `y` on several predictor columns. args: (predictors, y)
                // with an optional 3rd boolean `intercept` (default true). `missing`
                // anywhere propagates. coefficients/std_errors/p_values are
                // parameter-indexed arrays (with an intercept, index 0 is it).
                if args.len() < 2 || args.len() > 3 {
                    return Err(HelixError::new(
                        format!("`multiple_regression` takes (predictors, y[, intercept]), got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let with_intercept = match args.get(2) {
                    None => true,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => {
                        return Err(type_err("multiple_regression", "a boolean `intercept` flag", other, line, col));
                    }
                };
                let preds = num_arrays(name, &args[0], line, col)?;
                let y = num_array(name, &args[1], line, col)?;
                let (preds, y) = match (preds, y) {
                    (Some(preds), Some(y)) => (preds, y),
                    _ => return Ok(Value::Missing),
                };
                let floats = |xs: Vec<f64>| Value::float_array(xs);
                match crate::stats::multiple_regression(&preds, &y, with_intercept) {
                    Some(f) => {
                        let fields = vec![
                            (Symbol::intern("coefficients"), floats(f.coefficients)),
                            (Symbol::intern("std_errors"), floats(f.std_errors)),
                            (Symbol::intern("p_values"), floats(f.p_values)),
                            (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                            (Symbol::intern("adj_r_squared"), Value::Float(f.adj_r_squared)),
                            (Symbol::intern("rss"), Value::Float(f.rss)),
                            (Symbol::intern("predictions"), floats(f.predictions)),
                            (Symbol::intern("residuals"), floats(f.residuals)),
                        ];
                        Ok(Value::Record(Rc::new(fields)))
                    }
                    None => Err(HelixError::new(
                        "multiple regression is undefined: need more observations than predictors, equal-length non-collinear predictors, and variance in y",
                        line,
                        col,
                    )
                    .hint("e.g. `multiple_regression([x1, x2], y)` with enough rows.")),
                }
            }
            "least_squares" => {
                // OLS without inference (no std_errors/p_values) — the fast fit for
                // model selection. args: (predictors, y[, intercept]).
                if args.len() < 2 || args.len() > 3 {
                    return Err(HelixError::new(
                        format!("`least_squares` takes (predictors, y[, intercept]), got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let with_intercept = match args.get(2) {
                    None => true,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => {
                        return Err(type_err("least_squares", "a boolean `intercept` flag", other, line, col));
                    }
                };
                let preds = num_arrays(name, &args[0], line, col)?;
                let y = num_array(name, &args[1], line, col)?;
                let (preds, y) = match (preds, y) {
                    (Some(preds), Some(y)) => (preds, y),
                    _ => return Ok(Value::Missing),
                };
                match crate::stats::least_squares(&preds, &y, with_intercept) {
                    Some(f) => Ok(Value::Record(Rc::new(vec![
                        (Symbol::intern("coefficients"), Value::float_array(f.coefficients)),
                        (Symbol::intern("rss"), Value::Float(f.rss)),
                        (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                        (Symbol::intern("predictions"), Value::float_array(f.predictions)),
                        (Symbol::intern("residuals"), Value::float_array(f.residuals)),
                    ]))),
                    None => Err(HelixError::new(
                        "least squares is undefined: need at least as many rows as parameters, equal-length non-collinear predictors",
                        line,
                        col,
                    )
                    .hint("e.g. `least_squares([x1, x2], y)`.")),
                }
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
            "mse" | "rmse" | "mae" | "r2_score" => {
                arity(name, &args, 2, line, col)?;
                let a = num_array(name, &args[0], line, col)?;
                let b = num_array(name, &args[1], line, col)?;
                let (a, b) = match (a, b) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return Ok(Value::Missing),
                };
                if a.len() != b.len() {
                    return Err(HelixError::new(
                        format!("`{name}` needs two equal-length arrays, got {} and {}", a.len(), b.len()),
                        line,
                        col,
                    ));
                }
                if a.is_empty() {
                    return Err(HelixError::new(format!("cannot compute `{name}` of empty arrays"), line, col));
                }
                let nf = a.len() as f64;
                match name {
                    "mse" => Ok(Value::Float(a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / nf)),
                    "rmse" => Ok(Value::Float((a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / nf).sqrt())),
                    "mae" => Ok(Value::Float(a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f64>() / nf)),
                    // r2_score(y_true, y_pred) = 1 - SS_res/SS_tot — y_true first, to
                    // match scikit-learn and the sibling metrics (mse/mae take y_true,
                    // y_pred). SS_tot is the variance of the *true* values.
                    _ => {
                        let (actual, pred) = (&a, &b);
                        let mean = actual.iter().sum::<f64>() / nf;
                        let ss_tot: f64 = actual.iter().map(|v| (v - mean).powi(2)).sum();
                        if ss_tot == 0.0 {
                            return Err(HelixError::new(
                                "`r2_score` is undefined: the actual values have zero variance",
                                line,
                                col,
                            ));
                        }
                        let ss_res: f64 = actual.iter().zip(pred).map(|(y, p)| (y - p).powi(2)).sum();
                        Ok(Value::Float(1.0 - ss_res / ss_tot))
                    }
                }
            }
            // Information criteria for model selection (Gaussian-likelihood form):
            // aic(rss, n, k) = n*ln(rss/n) + 2k ; bic(rss, n, k) = n*ln(rss/n) + k*ln(n).
            "aic" | "bic" => {
                arity(name, &args, 3, line, col)?;
                let rss = args[0].as_f64().ok_or_else(|| type_err(name, "a number (rss)", &args[0], line, col))?;
                let nn = as_int(&args[1], name, line, col)?;
                let kk = as_int(&args[2], name, line, col)?;
                if nn <= 0 {
                    return Err(HelixError::new(format!("`{name}` needs n > 0"), line, col));
                }
                if rss < 0.0 {
                    return Err(HelixError::new(format!("`{name}` needs rss >= 0"), line, col));
                }
                let (nf, kf) = (nn as f64, kk as f64);
                let log_like = nf * (rss / nf).ln(); // ∝ -2·logL up to a constant
                Ok(Value::Float(if name == "aic" {
                    log_like + 2.0 * kf
                } else {
                    log_like + kf * nf.ln()
                }))
            }
            // Classification metrics — free functions taking (y_true, y_pred), mirroring
            // the regression metrics above. `accuracy` is multiclass-safe; the rest are
            // binary against a positive class (3rd arg `pos_label`, default `1`).
            "accuracy" => {
                arity(name, &args, 2, line, col)?;
                let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                let correct = a.iter().zip(&b).filter(|(x, y)| values_equal(x, y)).count();
                Ok(Value::Float(correct as f64 / a.len() as f64))
            }
            "precision" | "recall" | "f1_score" => {
                if args.len() != 2 && args.len() != 3 {
                    return Err(HelixError::new(
                        format!("`{name}` takes (y_true, y_pred) or (y_true, y_pred, pos_label), got {} arguments", args.len()),
                        line,
                        col,
                    ));
                }
                let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                let pos = if args.len() == 3 { args[2].clone() } else { Value::Int(1) };
                let (tp, fp, fa_neg, _) = binary_counts(&a, &b, &pos);
                // sklearn convention: an undefined ratio (no predicted/actual positives)
                // is reported as 0.0 rather than raising.
                let precision = if tp + fp == 0 { 0.0 } else { tp as f64 / (tp + fp) as f64 };
                let recall = if tp + fa_neg == 0 { 0.0 } else { tp as f64 / (tp + fa_neg) as f64 };
                Ok(Value::Float(match name {
                    "precision" => precision,
                    "recall" => recall,
                    _ if precision + recall == 0.0 => 0.0,
                    _ => 2.0 * precision * recall / (precision + recall),
                }))
            }
            "confusion_matrix" => {
                if args.len() != 2 && args.len() != 3 {
                    return Err(HelixError::new(
                        format!("`confusion_matrix` takes (y_true, y_pred) or (y_true, y_pred, pos_label), got {} arguments", args.len()),
                        line,
                        col,
                    ));
                }
                let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                let pos = if args.len() == 3 { args[2].clone() } else { Value::Int(1) };
                let (tp, fp, fa_neg, tn) = binary_counts(&a, &b, &pos);
                Ok(Value::Record(Rc::new(vec![
                    (Symbol::intern("tp"), Value::Int(tp)),
                    (Symbol::intern("fp"), Value::Int(fp)),
                    (Symbol::intern("fn"), Value::Int(fa_neg)),
                    (Symbol::intern("tn"), Value::Int(tn)),
                ])))
            }
            _ => {
                let mut err =
                    HelixError::new(format!("`{}` is not a known function", name), line, col);
                let cands: Vec<&str> = crate::registry::names().collect();
                if let Some(s) = suggest(name, &cands) {
                    err = err.hint(format!("did you mean `{}`?", s));
                }
                Err(err)
            }
        }
    }
}

/// Convert a Helix array (one column of a `dataframe({…})` call) into a
/// backend-agnostic [`ColData`]. The column type is inferred from the first
/// non-`missing` element; `missing` becomes a null (selecting the nullable
/// variant). Ints + Floats in one column promote to Float; a column may not mix
/// otherwise-incompatible types.
fn array_to_coldata(
    name: &str,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<crate::backend::ColData, HelixError> {
    use crate::backend::ColData;
    let arr = match v {
        Value::Array(a) => a,
        other => {
            return Err(HelixError::new(
                format!("column `{}` must be an array, but got a {}", name, other.type_name()),
                line,
                col,
            )
            .hint("each DataFrame column is an array, e.g. `dataframe({age: [30, 41]})`."))
        }
    };
    let vals = arr.to_values();
    let mixed = |got: &Value| {
        HelixError::new(
            format!("column `{}` mixes types (found a {})", name, got.type_name()),
            line,
            col,
        )
        .hint("each DataFrame column must be all one type.")
    };
    let has_missing = vals.iter().any(|x| matches!(x, Value::Missing));
    let kind = match vals.iter().find(|x| !matches!(x, Value::Missing)) {
        Some(k) => k,
        None => {
            return Err(HelixError::new(
                format!("cannot infer the type of column `{}` (empty or all `missing`)", name),
                line,
                col,
            ))
        }
    };
    match kind {
        Value::Int(_) | Value::Float(_) => {
            let any_float = vals.iter().any(|x| matches!(x, Value::Float(_)));
            if any_float {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(Some(*i as f64)),
                        Value::Float(f) => out.push(Some(*f)),
                        Value::Missing => out.push(None),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::Float(out))
            } else if has_missing {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(Some(*i)),
                        Value::Missing => out.push(None),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::IntOpt(out))
            } else {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(*i),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::Int(out))
            }
        }
        Value::Str(_) | Value::Dna(_) => {
            let pull = |x: &Value| match x {
                Value::Str(s) => Some((**s).clone()),
                Value::Dna(s) => Some((**s).clone()),
                _ => None,
            };
            if has_missing {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Missing => out.push(None),
                        _ => match pull(x) {
                            Some(s) => out.push(Some(s)),
                            None => return Err(mixed(x)),
                        },
                    }
                }
                Ok(ColData::StrOpt(out))
            } else {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match pull(x) {
                        Some(s) => out.push(s),
                        None => return Err(mixed(x)),
                    }
                }
                Ok(ColData::Str(out))
            }
        }
        Value::Bool(_) => {
            if has_missing {
                return Err(HelixError::new(
                    format!("boolean column `{}` cannot contain `missing`", name),
                    line,
                    col,
                ));
            }
            let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
            for x in vals.iter() {
                match x {
                    Value::Bool(b) => out.push(*b),
                    o => return Err(mixed(o)),
                }
            }
            Ok(ColData::Bool(out))
        }
        other => Err(HelixError::new(
            format!("column `{}` has an unsupported element type ({})", name, other.type_name()),
            line,
            col,
        )
        .hint("DataFrame columns must be numbers, strings, DNA, or booleans.")),
    }
}

/// Extract a slice of numeric columns from an array-of-arrays argument (the predictor
/// matrix of `multiple_regression`). Returns `Ok(None)` if any element anywhere is
/// `missing`; errors if the outer value is not an array, or any inner value is not a
/// numeric array.
fn num_arrays(
    who: &str,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<Option<Vec<Vec<f64>>>, HelixError> {
    let outer = match v {
        Value::Array(items) => items,
        other => return Err(type_err(who, "an array of predictor arrays", other, line, col)),
    };
    let mut cols = Vec::with_capacity(outer.len());
    for el in outer.to_values().iter() {
        match num_array(who, el, line, col)? {
            Some(c) => cols.push(c),
            None => return Ok(None),
        }
    }
    Ok(Some(cols))
}

/// Greatest common divisor of two i64 (Euclid). Sign-agnostic; gcd(n,0)=|n|.
/// Computed in i128 so `abs(i64::MIN)` doesn't overflow.
fn gcd_i64(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = ((a as i128).unsigned_abs(), (b as i128).unsigned_abs());
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a as i64
}

/// Read a whole file to a `String`, capping at `MAX_STRING_LEN` so a huge file is a
/// clean error rather than an allocator abort. Shared by `read_text` and `read_json`.
fn read_text_file(path: &str, line: usize, col: usize) -> Result<String, HelixError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        HelixError::new(format!("could not read `{path}`: {e}"), line, col)
            .hint("check the path exists and is readable.")
    })?;
    if meta.len() as usize > crate::interp::MAX_STRING_LEN {
        return Err(HelixError::new(
            format!("`{path}` is larger than the {}-byte text limit", crate::interp::MAX_STRING_LEN),
            line,
            col,
        )
        .hint("read very large tabular files with read_csv/read_parquet instead."));
    }
    std::fs::read_to_string(path)
        .map_err(|e| HelixError::new(format!("could not read `{path}`: {e}"), line, col))
}

/// Extract two equal-length, non-empty label arrays as raw `Value`s for the
/// classification metrics. Labels are compared by value equality — never coerced to
/// f64 — so integer (0/1), boolean, and string class labels all work uniformly.
fn label_pair(
    who: &str,
    yt: &Value,
    yp: &Value,
    line: usize,
    col: usize,
) -> Result<(Vec<Value>, Vec<Value>), HelixError> {
    let take = |v: &Value| -> Result<Vec<Value>, HelixError> {
        match v {
            Value::Array(items) => Ok(items.to_values().into_owned()),
            other => Err(type_err(who, "an array of labels", other, line, col)),
        }
    };
    let (a, b) = (take(yt)?, take(yp)?);
    if a.len() != b.len() {
        return Err(HelixError::new(
            format!("`{who}` needs two equal-length arrays, got {} and {}", a.len(), b.len()),
            line,
            col,
        ));
    }
    if a.is_empty() {
        return Err(HelixError::new(format!("cannot compute `{who}` of empty arrays"), line, col));
    }
    Ok((a, b))
}

/// Tally (tp, fp, fn, tn) of `pred` against `truth` for one positive class `pos`.
fn binary_counts(truth: &[Value], pred: &[Value], pos: &Value) -> (i64, i64, i64, i64) {
    let (mut tp, mut fp, mut fa_neg, mut tn) = (0i64, 0i64, 0i64, 0i64);
    for (yt, yp) in truth.iter().zip(pred) {
        match (values_equal(yt, pos), values_equal(yp, pos)) {
            (true, true) => tp += 1,
            (false, true) => fp += 1,
            (true, false) => fa_neg += 1,
            (false, false) => tn += 1,
        }
    }
    (tp, fp, fa_neg, tn)
}

/// Extract a numeric `Vec<f64>` from an array argument. Returns `Ok(None)` when any
/// element is `missing` *or* a `NaN` float (so the caller can propagate `missing`,
/// per ADR-0001 — a `NaN` would otherwise silently corrupt the bivariate result),
/// and errors if the value is not an array or holds a non-numeric element.
fn num_array(who: &str, v: &Value, line: usize, col: usize) -> Result<Option<Vec<f64>>, HelixError> {
    let items = match v {
        Value::Array(items) => items,
        other => return Err(type_err(who, "an array of numbers", other, line, col)),
    };
    let mut out = Vec::with_capacity(items.len());
    for el in items.to_values().iter() {
        match el {
            Value::Missing => return Ok(None),
            Value::Float(f) if f.is_nan() => return Ok(None),
            _ => match el.as_f64() {
                Some(x) => out.push(x),
                None => return Err(type_err(who, "an array of numbers", el, line, col)),
            },
        }
    }
    Ok(Some(out))
}

/// Parse a string to a float for `to_float(s)` and `String.to_float()`. Leading/trailing
/// whitespace is ignored; a non-numeric string is a clear error (not a silent NaN).
pub(crate) fn parse_str_float(s: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let t = s.trim();
    t.parse::<f64>().map(Value::Float).map_err(|_| {
        HelixError::new(format!("could not parse {t:?} as a number"), line, col)
            .hint("`to_float` expects a numeric string like \"3.14\" or \"-2\".")
    })
}

/// Parse a string to an integer for `to_int(s)` and `String.to_int()`. Strict: a decimal
/// string like "3.5" is rejected (use `to_float`), so an integer field never rounds silently.
pub(crate) fn parse_str_int(s: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let t = s.trim();
    t.parse::<i64>().map(Value::Int).map_err(|_| {
        HelixError::new(format!("could not parse {t:?} as an integer"), line, col)
            .hint("`to_int` expects an integer string like \"42\" or \"-7\"; for decimals use `to_float`.")
    })
}

/// An IEEE float predicate (`is_nan`/`is_finite`/`is_infinite`): `Float` → `Bool` via `f`;
/// an `Int`/`Rational` is exact so it yields `on_exact` (always finite, never NaN/inf);
/// `missing` propagates; an array maps elementwise to a `Bool` array; a tensor (f64-only,
/// so no `Bool` element type) yields a `1.0`/`0.0` mask tensor usable in arithmetic.
fn float_predicate(
    v: &Value,
    name: &str,
    f: fn(f64) -> bool,
    on_exact: bool,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match v {
        Value::Array(items) => {
            let out: Result<Vec<Value>, HelixError> = items
                .to_values()
                .iter()
                .map(|e| float_predicate(e, name, f, on_exact, line, col))
                .collect();
            Ok(Value::array(out?))
        }
        Value::Tensor(t) => {
            let data: Vec<f64> = t.iter().map(|&x| if f(x) { 1.0 } else { 0.0 }).collect();
            let out = ndarray::ArrayD::from_shape_vec(t.raw_dim(), data)
                .expect("same length as source tensor");
            Ok(Value::Tensor(Rc::new(out)))
        }
        Value::Missing => Ok(Value::Missing),
        Value::Float(x) => Ok(Value::Bool(f(*x))),
        Value::Int(_) | Value::Rational(_) => Ok(Value::Bool(on_exact)),
        other => Err(type_err(name, "a number or array of numbers", other, line, col)),
    }
}

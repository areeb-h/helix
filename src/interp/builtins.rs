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
            "json.parse" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        crate::json::parse(s).map_err(|e| HelixError::new(e, line, col))
                    }
                    other => Err(type_err("json.parse", "a JSON string", other, line, col)),
                }
            }
            "json.stringify" => {
                arity(name, &args, 1, line, col)?;
                crate::json::stringify(&args[0])
                    .map(|s| Value::Str(Rc::new(s)))
                    .map_err(|e| HelixError::new(e, line, col))
            }
            // Terminal charts → a rendered string (theme/color from `render::auto`).
            "chart.bar" => crate::chart::bar(&args, line, col),
            "chart.hist" => crate::chart::hist(&args, line, col),
            "chart.line" => crate::chart::line(&args, line, col),
            "chart.scatter" => crate::chart::scatter(&args, line, col),
            "chart.sparkline" => crate::chart::sparkline(&args, line, col),
            // Writers (effectful) → write a file, return Unit.
            "io.write" => crate::writers::write_text(&args, line, col),
            "io.append" => crate::writers::append_text(&args, line, col),
            "io.write_csv" => crate::writers::write_csv(&args, line, col),
            "io.write_tsv" => crate::writers::write_tsv(&args, line, col),
            "io.write_json" => crate::writers::write_json(&args, line, col),
            "bio.write_fasta" => crate::writers::write_fasta(&args, line, col),
            "bio.write_fastq" => crate::writers::write_fastq(&args, line, col),
            // Exporters (pure) → serialize to a string in another format.
            "export.markdown" => crate::writers::to_markdown(&args, line, col),
            "export.html" => crate::writers::to_html(&args, line, col),
            "export.svg_bar" => crate::writers::svg_bar(&args, line, col),
            "export.svg_line" => crate::writers::svg_line(&args, line, col),
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
            "read_vcf" => {
                // `read_vcf(path)` scans; `read_vcf(path, "chr:start-end")` does an
                // indexed region query against the file's `.tbi`.
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`bio.read_vcf` takes 1 or 2 arguments, got {}", args.len()),
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
                        format!("`bio.read_bam` takes 1 or 2 arguments, got {}", args.len()),
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
            "io.write_parquet" => {
                arity(name, &args, 2, line, col)?;
                match (&args[0], &args[1]) {
                    (Value::DataFrame(lf), Value::Str(p)) => {
                        lf.write_parquet(p, line, col)?;
                        Ok(Value::Unit)
                    }
                    (Value::DataFrame(_), other) => {
                        Err(type_err("io.write_parquet", "a string path", other, line, col))
                    }
                    (other, _) => Err(type_err("io.write_parquet", "a DataFrame", other, line, col)),
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
            // ---- math standard library (broadcasts over arrays, propagates missing) ----
            "sqrt" | "cbrt" | "exp" | "ln" | "log10" | "log2" | "sin" | "cos" | "tan" | "asin"
            | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "degrees" | "radians" | "erf"
            | "normal_cdf" | "normal_pdf" => {
                arity(name, &args, 1, line, col)?;
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
                    // `wrapping_abs` matches the wrapping-on-overflow convention used by
                    // the arithmetic ops; `i64::MIN.abs()` would otherwise panic in debug.
                    Value::Int(i) => Ok(Value::Int(i.wrapping_abs())),
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
                match crate::stats::linear_regression(&xs, &ys) {
                    Some(f) => {
                        let fields = vec![
                            (Symbol::intern("slope"), Value::Float(f.slope)),
                            (Symbol::intern("intercept"), Value::Float(f.intercept)),
                            (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                            (Symbol::intern("slope_std_error"), Value::Float(f.slope_std_error)),
                            (Symbol::intern("slope_p_value"), Value::Float(f.slope_p_value)),
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
                arity(name, &args, 2, line, col)?;
                // OLS fit of `y` on several predictor columns. The first argument is an
                // array of predictor arrays; the second is the response. `missing`
                // anywhere propagates. The result's coefficients/std_errors/p_values are
                // parameter-indexed arrays (index 0 is the intercept).
                let preds = num_arrays(name, &args[0], line, col)?;
                let y = num_array(name, &args[1], line, col)?;
                let (preds, y) = match (preds, y) {
                    (Some(preds), Some(y)) => (preds, y),
                    _ => return Ok(Value::Missing),
                };
                let floats = |xs: Vec<f64>| {
                    Value::array(xs.into_iter().map(Value::Float).collect())
                };
                match crate::stats::multiple_regression(&preds, &y) {
                    Some(f) => {
                        let fields = vec![
                            (Symbol::intern("coefficients"), floats(f.coefficients)),
                            (Symbol::intern("std_errors"), floats(f.std_errors)),
                            (Symbol::intern("p_values"), floats(f.p_values)),
                            (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                            (Symbol::intern("adj_r_squared"), Value::Float(f.adj_r_squared)),
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
            // --- descriptive statistics helpers (one numeric array; missing propagates) ---
            "stats.standard_error"
            | "stats.coefficient_of_variation"
            | "stats.iqr"
            | "stats.spread"
            | "stats.zscores" => {
                arity(name, &args, 1, line, col)?;
                let xs = match num_array(name, &args[0], line, col)? {
                    Some(xs) => xs,
                    None => return Ok(Value::Missing),
                };
                if xs.is_empty() {
                    return Err(HelixError::new(
                        format!("cannot compute `{}` of an empty array", name),
                        line,
                        col,
                    ));
                }
                match name {
                    "stats.standard_error" => {
                        Ok(Value::Float(crate::stats::std(&xs) / (xs.len() as f64).sqrt()))
                    }
                    "stats.coefficient_of_variation" => {
                        // CV is a ratio to the mean, so it's undefined when the mean
                        // is zero — a clean error, not a silent `inf`/`NaN` (matching
                        // the zero-spread guard the z-scores path already has).
                        let m = crate::stats::mean(&xs);
                        if m == 0.0 {
                            return Err(HelixError::new(
                                "coefficient of variation is undefined: the mean is zero",
                                line,
                                col,
                            ));
                        }
                        Ok(Value::Float(crate::stats::std(&xs) / m))
                    }
                    "stats.iqr" => Ok(Value::Float(
                        crate::stats::quantile(&xs, 0.75) - crate::stats::quantile(&xs, 0.25),
                    )),
                    "stats.spread" => {
                        let (mut lo, mut hi) = (xs[0], xs[0]);
                        for &x in &xs {
                            lo = lo.min(x);
                            hi = hi.max(x);
                        }
                        Ok(Value::Float(hi - lo))
                    }
                    // z-scores: each value's distance from the mean in standard deviations.
                    _ => {
                        let (m, sd) = (crate::stats::mean(&xs), crate::stats::std(&xs));
                        if sd == 0.0 {
                            return Err(HelixError::new(
                                "cannot compute z-scores: the values have zero spread",
                                line,
                                col,
                            )
                            .hint("a constant series has no standard deviation to scale by."));
                        }
                        let out: Vec<Value> =
                            xs.iter().map(|x| Value::Float((x - m) / sd)).collect();
                        Ok(Value::array(out))
                    }
                }
            }
            // --- sequence helpers over DNA values (missing propagates) ---
            "bio.at_content" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Missing => Ok(Value::Missing),
                    Value::Dna(s) => Ok(Value::Float(1.0 - dna_gc_content(s, name, line, col)?)),
                    other => Err(type_err(name, "a DNA sequence", other, line, col)),
                }
            }
            "bio.mean_gc" | "bio.total_length" => {
                arity(name, &args, 1, line, col)?;
                let items = match &args[0] {
                    Value::Array(items) => items,
                    Value::Missing => return Ok(Value::Missing),
                    other => return Err(type_err(name, "an array of DNA sequences", other, line, col)),
                };
                let vals = items.to_values();
                if vals.iter().any(|v| matches!(v, Value::Missing)) {
                    return Ok(Value::Missing);
                }
                let seqs: Vec<&Rc<String>> = vals
                    .iter()
                    .map(|v| match v {
                        Value::Dna(s) => Ok(s),
                        other => Err(type_err(name, "an array of DNA sequences", other, line, col)),
                    })
                    .collect::<Result<_, _>>()?;
                if name == "bio.total_length" {
                    Ok(Value::Int(seqs.iter().map(|s| s.len() as i64).sum()))
                } else {
                    if seqs.is_empty() {
                        return Err(HelixError::new(
                            "cannot compute `bio.mean_gc` of no sequences",
                            line,
                            col,
                        ));
                    }
                    let total: f64 = seqs
                        .iter()
                        .map(|s| dna_gc_content(s, name, line, col))
                        .sum::<Result<f64, _>>()?;
                    Ok(Value::Float(total / seqs.len() as f64))
                }
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

/// The GC fraction of a DNA sequence (`who` names the calling builtin for errors).
/// Errors on an empty sequence, which has no composition to measure.
fn dna_gc_content(s: &str, who: &str, line: usize, col: usize) -> Result<f64, HelixError> {
    if s.is_empty() {
        return Err(HelixError::new(
            format!("cannot compute `{}` of an empty sequence", who),
            line,
            col,
        ));
    }
    // GC fraction over *called* bases: `N` (unknown) is excluded from the
    // denominator, so `gc_content("GCN") == 1.0`, not 2/3.
    let gc = s.chars().filter(|c| *c == 'G' || *c == 'C').count();
    let called = s.chars().filter(|c| *c != 'N').count();
    Ok(if called == 0 { 0.0 } else { gc as f64 / called as f64 })
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

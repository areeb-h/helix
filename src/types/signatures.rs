//! Static signature tables: the return-type rules for builtins and the per-type
//! method families (`array_method_type`, `string_method_type`, `tensor_method_type`,
//! `df_method_type`, `groupby_method_type`, …). Pure functions mapping a name (and
//! arg types) to a result `Type`, mirroring the runtime's actual behaviour. The
//! `Checker` consults these during inference.

use super::*;

// `it` by default, or the single lambda param. Body is the (sole) arg expr.
pub(super) fn comprehension_params(args: &[Expr]) -> (Vec<String>, &Expr) {
    match args.first() {
        Some(Expr::Lambda { params, body }) => (params.clone(), body),
        Some(e) => (vec!["it".to_string()], e),
        None => (vec!["it".to_string()], &Expr::Missing),
    }
}

pub(super) fn require_boolish(t: &Type, name: &str, line: usize, col: usize) -> Result<(), HelixError> {
    if matches!(t, Type::Bool | Type::Missing | Type::Unknown) {
        Ok(())
    } else {
        Err(HelixError::new(
            format!(
                "`{}` expects a yes/no test, but the expression produces a value of type {}",
                name, t
            ),
            line,
            col,
        )
        .hint("write a comparison, e.g. `xs.filter(it > 50)`."))
    }
}

// ---------- signature tables ----------

pub(super) fn builtin_type(name: &str, args: &[Type], line: usize, col: usize) -> Result<Type, HelixError> {
    let any = |ts: &[Type], f: fn(&Type) -> bool| ts.iter().any(f);
    // math: container/Unknown ⇒ Unknown; Missing ⇒ Missing (the false-positive guard)
    if MATH_UNARY_FLOAT.contains(&name)
        || matches!(name, "floor" | "ceil" | "round" | "trunc" | "abs" | "sign")
    {
        if args.len() != 1 {
            return Err(arity_err(name, 1, args.len(), line, col));
        }
        let a = &args[0];
        if matches!(a, Type::Array(_) | Type::Tensor | Type::Unknown) {
            return Ok(Type::Unknown);
        }
        if matches!(a, Type::Missing) {
            return Ok(Type::Missing);
        }
        if !is_numeric(a) {
            return Err(type_err(name, "a number or array of numbers", a, line, col));
        }
        return Ok(match name {
            "floor" | "ceil" | "round" | "trunc" | "sign" => Type::Int,
            "abs" => a.clone(),
            _ => Type::Float,
        });
    }
    match name {
        "print" => Ok(Type::Unit),
        "assert" => {
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`assert` takes a condition and an optional message, got {} arguments", args.len()),
                    line,
                    col,
                ));
            }
            if !compatible(&args[0], &Type::Bool) {
                return Err(type_err("assert", "a boolean condition", &args[0], line, col));
            }
            if let Some(msg) = args.get(1)
                && !compatible(msg, &Type::String)
            {
                return Err(type_err("assert", "a string message", msg, line, col));
            }
            Ok(Type::Unit)
        }
        // `assert_eq` accepts any two comparable values; `assert_close` needs numbers
        // (plus an optional tolerance). Equality/closeness is checked at runtime.
        "assert_eq" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            Ok(Type::Unit)
        }
        "assert_close" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(HelixError::new(
                    format!("`assert_close` takes two numbers and an optional tolerance, got {} arguments", args.len()),
                    line,
                    col,
                ));
            }
            for a in args {
                if !is_numeric(a) && !matches!(a, Type::Unknown) {
                    return Err(type_err("assert_close", "a number", a, line, col));
                }
            }
            Ok(Type::Unit)
        }
        "dna" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err("dna", "a string", &args[0], line, col));
            }
            Ok(Type::Dna)
        }
        "range" => {
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`range` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                ));
            }
            for a in args {
                if !compatible(a, &Type::Int) {
                    return Err(type_err("range", "an integer", a, line, col));
                }
            }
            Ok(Type::Array(Box::new(Type::Int)))
        }
        "io.read_csv" | "io.read_parquet" | "bio.read_bcf" | "bio.read_sam" | "bio.read_gff"
        | "bio.read_bed" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err(name, "a string path", &args[0], line, col));
            }
            Ok(Type::DataFrame)
        }
        // `read_vcf`/`read_bam` scan with one argument; the optional region second
        // argument runs an indexed query, so these readers accept one or two strings.
        "bio.read_vcf" | "bio.read_bam" => {
            if args.is_empty() || args.len() > 2 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err(name, "a string path", &args[0], line, col));
            }
            if let Some(region) = args.get(1)
                && !compatible(region, &Type::String)
            {
                return Err(type_err(name, "a string region", region, line, col));
            }
            Ok(Type::DataFrame)
        }
        "bio.read_fasta" | "bio.read_fastq" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err(name, "a string path", &args[0], line, col));
            }
            // An array of sequence records; element kept `Unknown` so
            // field/sequence-method access stays permissive.
            Ok(Type::Array(Box::new(Type::Unknown)))
        }
        "io.write_parquet" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::DataFrame) {
                return Err(type_err("io.write_parquet", "a DataFrame", &args[0], line, col));
            }
            if !compatible(&args[1], &Type::String) {
                return Err(type_err("io.write_parquet", "a string path", &args[1], line, col));
            }
            Ok(Type::Unit)
        }
        "tensor" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            let a = &args[0];
            if is_numeric(a)
                || matches!(a, Type::Array(_) | Type::Unknown | Type::Missing)
            {
                Ok(Type::Tensor)
            } else {
                Err(type_err("tensor", "a number or array", a, line, col))
            }
        }
        "zeros" | "ones" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &array_of_unknown()) {
                return Err(type_err(name, "an array like `[2, 3]`", &args[0], line, col));
            }
            Ok(Type::Tensor)
        }
        "eye" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::Int) {
                return Err(type_err("eye", "an integer", &args[0], line, col));
            }
            Ok(Type::Tensor)
        }
        // two-arg math
        "log" | "atan2" | "hypot" | "min" | "max" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !is_numeric(a) {
                    return Err(type_err(name, "a number", a, line, col));
                }
            }
            Ok(if matches!(name, "min" | "max") {
                Type::Num
            } else {
                Type::Float
            })
        }
        "stats.correlation" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !matches!(a, Type::Array(_)) {
                    return Err(type_err(name, "an array of numbers", a, line, col));
                }
            }
            Ok(Type::Float)
        }
        "stats.t_test" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !matches!(a, Type::Array(_)) {
                    return Err(type_err(name, "an array of numbers", a, line, col));
                }
            }
            Ok(Type::Record(vec![
                ("statistic".to_string(), Type::Float),
                ("df".to_string(), Type::Float),
                ("p_value".to_string(), Type::Float),
            ]))
        }
        "stats.linear_regression" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !matches!(a, Type::Array(_)) {
                    return Err(type_err(name, "an array of numbers", a, line, col));
                }
            }
            Ok(Type::Record(vec![
                ("slope".to_string(), Type::Float),
                ("intercept".to_string(), Type::Float),
                ("r_squared".to_string(), Type::Float),
                ("slope_std_error".to_string(), Type::Float),
                ("slope_p_value".to_string(), Type::Float),
            ]))
        }
        "stats.multiple_regression" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            // First arg: an array of predictor arrays. Second: the response array.
            for a in args {
                if !matches!(a, Type::Array(_)) {
                    return Err(type_err(name, "an array", a, line, col));
                }
            }
            let nums = || Type::Array(Box::new(Type::Float));
            Ok(Type::Record(vec![
                ("coefficients".to_string(), nums()),
                ("std_errors".to_string(), nums()),
                ("p_values".to_string(), nums()),
                ("r_squared".to_string(), Type::Float),
                ("adj_r_squared".to_string(), Type::Float),
            ]))
        }
        // Descriptive-statistics helpers: one array of numbers in, a scalar (or, for
        // `zscores`, an array) out. `bio.mean_gc`/`bio.total_length` take an array too.
        "stats.standard_error"
        | "stats.coefficient_of_variation"
        | "stats.iqr"
        | "stats.spread"
        | "stats.zscores"
        | "bio.mean_gc"
        | "bio.total_length" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            match &args[0] {
                Type::Unknown => Ok(Type::Unknown),
                Type::Missing => Ok(Type::Missing),
                Type::Array(_) => Ok(match name {
                    "stats.zscores" => Type::Array(Box::new(Type::Float)),
                    "bio.total_length" => Type::Int,
                    _ => Type::Float,
                }),
                other => Err(type_err(name, "an array", other, line, col)),
            }
        }
        // `bio.at_content` takes a single DNA sequence.
        "bio.at_content" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            match &args[0] {
                Type::Unknown => Ok(Type::Unknown),
                Type::Missing => Ok(Type::Missing),
                Type::Dna => Ok(Type::Float),
                other => Err(type_err(name, "a DNA sequence", other, line, col)),
            }
        }
        "to_array" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            // Materializes a Python iterable (or array) — element type is Unknown.
            Ok(array_of_unknown())
        }
        "to_dataframe" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::DataFrame)
        }
        // `dataframe({col: array, …})` — build a frame from in-memory columns. The
        // record's shape is validated at runtime (each field must be an array).
        "dataframe" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::DataFrame)
        }
        "to_tensor" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::Tensor)
        }
        "json.parse" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            // The JSON shape isn't known statically — Unknown (the permissive top).
            Ok(Type::Unknown)
        }
        "json.stringify" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::String)
        }
        "http.get" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            // Returns a `{status, body}` record; Unknown keeps field access permissive.
            Ok(Type::Unknown)
        }
        _ => Ok(Type::Unknown), // unreachable (BUILTIN_FNS gated), but stay permissive
    }
}

pub(super) fn array_method_type(name: &str, el: &Type, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "mean" | "std" | "median" | "var" | "quantile" => Type::Float,
        // A descriptive overview record (the `describe()` analogue).
        "summary" => Type::Record(vec![
            ("count".to_string(), Type::Int),
            ("mean".to_string(), Type::Float),
            ("std".to_string(), Type::Float),
            ("min".to_string(), Type::Float),
            ("median".to_string(), Type::Float),
            ("max".to_string(), Type::Float),
        ]),
        "sum" => Type::Num,
        "min" | "max" | "first" | "last" => el.clone(),
        "count" => Type::Int,
        "normalize" => Type::Array(Box::new(Type::Float)),
        "sort" | "reverse" | "drop_missing" | "take" | "drop" | "unique" => {
            Type::Array(Box::new(el.clone()))
        }
        // `enumerate` -> Array of (Int, element) tuples; `zip` -> Array of pairs
        // (the other side's element type isn't threaded, so its 2nd slot is Unknown).
        "enumerate" => Type::Array(Box::new(Type::Tuple(vec![Type::Int, el.clone()]))),
        "zip" => Type::Array(Box::new(Type::Tuple(vec![el.clone(), Type::Unknown]))),
        // `(value, count)` tuples — `top` for the n most frequent, `frequencies` for all.
        "top" | "frequencies" => Type::Array(Box::new(Type::Tuple(vec![el.clone(), Type::Int]))),
        "join" => Type::String,
        _ => {
            return Err(unknown_method(
                "Array",
                name,
                &crate::registry::methods_of(crate::registry::ARRAY_METHODS),
                line,
                col,
            ))
        }
    })
}

pub(super) fn string_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "upper" | "lower" | "reverse" | "trim" | "replace" => Type::String,
        "count" => Type::Int,
        "split" => Type::Array(Box::new(Type::String)),
        "contains" | "starts_with" | "ends_with" => Type::Bool,
        // FASTQ Phred+33 quality string → per-base integer quality scores.
        "phred" => Type::Array(Box::new(Type::Int)),
        _ => {
            return Err(unknown_method(
                "String",
                name,
                &crate::registry::methods_of(crate::registry::STRING_METHODS),
                line,
                col,
            ))
        }
    })
}

pub(super) fn dna_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "length" => Type::Int,
        "gc_content" => Type::Float,
        "complement" | "reverse_complement" => Type::Dna,
        "kmers" | "windows" => Type::Array(Box::new(Type::String)),
        // (kmer, count) tuples — the native packed spectrum (forward or strand-canonical).
        "kmer_counts" | "canonical_kmer_counts" => {
            Type::Array(Box::new(Type::Tuple(vec![Type::String, Type::Int])))
        }
        // 0-based index of the motif, or `missing` when absent.
        "find" => Type::Int,
        _ => {
            return Err(unknown_method(
                "Dna",
                name,
                &crate::registry::methods_of(crate::registry::DNA_METHODS),
                line,
                col,
            ))
        }
    })
}

pub(super) fn tensor_method_type(name: &str, nargs: usize, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "shape" => Type::Array(Box::new(Type::Int)),
        "ndim" | "count" => Type::Int,
        // sum/mean/min/max: 0 args → scalar Float; 1 axis arg → Tensor.
        "sum" | "mean" | "min" | "max" => {
            if nargs == 0 {
                Type::Float
            } else {
                Type::Tensor
            }
        }
        "flatten" | "reshape" | "transpose" | "t" | "inv" | "solve" => Type::Tensor,
        "matmul" | "dot" => Type::Unknown, // Float for vec·vec, Tensor otherwise
        "norm" | "det" => Type::Float,
        _ => {
            return Err(unknown_method(
                "Tensor",
                name,
                &crate::registry::methods_of(crate::registry::TENSOR_METHODS),
                line,
                col,
            ))
        }
    })
}

pub(super) fn df_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "where" | "filter" | "select" | "sort" | "head" | "cache" | "with" | "join" => {
            Type::DataFrame
        }
        "group" => Type::GroupBy,
        "count" => Type::Int,
        "columns" => Type::Array(Box::new(Type::String)),
        // One column's values as an array; element type is the runtime schema boundary.
        "column" => array_of_unknown(),
        _ => {
            return Err(unknown_method(
                "DataFrame",
                name,
                &crate::registry::methods_of(crate::registry::DF_METHODS),
                line,
                col,
            ))
        }
    })
}

pub(super) fn groupby_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "mean" | "sum" | "min" | "max" | "count" | "std" => Type::DataFrame,
        _ => {
            return Err(unknown_method(
                "GroupBy",
                name,
                &crate::registry::methods_of(crate::registry::GROUPBY_METHODS),
                line,
                col,
            ))
        }
    })
}

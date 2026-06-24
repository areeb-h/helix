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
        "read_csv" | "read_parquet" | "read_vcf" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err(name, "a string path", &args[0], line, col));
            }
            Ok(Type::DataFrame)
        }
        "read_fasta" | "read_fastq" => {
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
        "write_parquet" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::DataFrame) {
                return Err(type_err("write_parquet", "a DataFrame", &args[0], line, col));
            }
            if !compatible(&args[1], &Type::String) {
                return Err(type_err("write_parquet", "a string path", &args[1], line, col));
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
        "correlation" => {
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
        "t_test" => {
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
        "linear_regression" => {
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
        "to_tensor" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::Tensor)
        }
        "parse_json" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            // The JSON shape isn't known statically — Unknown (the permissive top).
            Ok(Type::Unknown)
        }
        "to_json" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::String)
        }
        "http_get" => {
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
        "sort" | "reverse" | "drop_missing" | "take" | "drop" => Type::Array(Box::new(el.clone())),
        // `enumerate` -> Array of (Int, element) tuples; `zip` -> Array of pairs
        // (the other side's element type isn't threaded, so its 2nd slot is Unknown).
        "enumerate" => Type::Array(Box::new(Type::Tuple(vec![Type::Int, el.clone()]))),
        "zip" => Type::Array(Box::new(Type::Tuple(vec![el.clone(), Type::Unknown]))),
        // `(value, count)` tuples for the n most frequent elements.
        "top" => Type::Array(Box::new(Type::Tuple(vec![el.clone(), Type::Int]))),
        _ => return Err(unknown_method("Array", name, ARRAY_METHODS, line, col)),
    })
}

pub(super) fn string_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "upper" | "lower" | "reverse" => Type::String,
        "count" => Type::Int,
        _ => return Err(unknown_method("String", name, STRING_METHODS, line, col)),
    })
}

pub(super) fn dna_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "length" => Type::Int,
        "gc_content" => Type::Float,
        "complement" | "reverse_complement" => Type::Dna,
        "kmers" => Type::Array(Box::new(Type::String)),
        // 0-based index of the motif, or `missing` when absent.
        "find" => Type::Int,
        _ => return Err(unknown_method("Dna", name, DNA_METHODS, line, col)),
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
        _ => return Err(unknown_method("Tensor", name, TENSOR_METHODS, line, col)),
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
        _ => return Err(unknown_method("DataFrame", name, DF_METHODS, line, col)),
    })
}

pub(super) fn groupby_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "mean" | "sum" | "min" | "max" | "count" | "std" => Type::DataFrame,
        _ => return Err(unknown_method("GroupBy", name, GROUPBY_AGGS, line, col)),
    })
}

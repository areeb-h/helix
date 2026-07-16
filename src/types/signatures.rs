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
    // `round` is special: `round(x)` → Int (nearest), `round(x, digits)` → Float.
    if name == "round" {
        if args.is_empty() || args.len() > 2 {
            return Err(HelixError::new(
                format!("`round` takes a number and an optional digit count, got {}", args.len()),
                line,
                col,
            ));
        }
        let a = &args[0];
        if matches!(a, Type::Array(_) | Type::Tensor | Type::Unknown) {
            return Ok(Type::Unknown);
        }
        if matches!(a, Type::Missing) {
            return Ok(Type::Missing);
        }
        if !is_numeric(a) {
            return Err(type_err("round", "a number or array of numbers", a, line, col));
        }
        return Ok(if args.len() == 1 { Type::Int } else { Type::Float });
    }
    // math: container/Unknown ⇒ Unknown; Missing ⇒ Missing (the false-positive guard)
    if MATH_UNARY_FLOAT.contains(&name)
        || matches!(
            name,
            "floor" | "ceil" | "trunc" | "abs" | "sign" | "is_nan" | "is_finite" | "is_infinite"
        )
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
            "floor" | "ceil" | "trunc" | "sign" => Type::Int,
            "is_nan" | "is_finite" | "is_infinite" => Type::Bool,
            "abs" => a.clone(),
            _ => Type::Float,
        });
    }
    match name {
        "print" => Ok(Type::Unit),
        "emit" => {
            if args.len() != 1 {
                return Err(arity_err("emit", 1, args.len(), line, col));
            }
            Ok(Type::Unit)
        }
        "write" | "elog" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::Unit)
        }
        "sleep" => {
            if args.len() != 1 {
                return Err(arity_err("sleep", 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::Float) {
                return Err(type_err("sleep", "a number of milliseconds", &args[0], line, col));
            }
            Ok(Type::Unit)
        }
        // `listen(port)` → an opaque network handle (the HTTP listener). The handle is a
        // runtime-only type (`Value::Net`); the checker sees `Unknown`, so `.accept()` /
        // `.respond()` on it dispatch at runtime — the opaque-type pattern shared with Dict.
        "listen" => {
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`listen` takes a port and an optional shard count, got {}", args.len()),
                    line,
                    col,
                ));
            }
            if !compatible(&args[0], &Type::Int) {
                return Err(type_err("listen", "a port number", &args[0], line, col));
            }
            if let Some(a) = args.get(1)
                && !compatible(a, &Type::Int)
            {
                return Err(type_err("listen", "a shard count", a, line, col));
            }
            Ok(Type::Unknown)
        }
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
            if args.is_empty() || args.len() > 3 {
                return Err(HelixError::new(
                    format!("`range` takes 1 to 3 arguments, got {}", args.len()),
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
        "read_csv" | "read_parquet" | "read_bcf" | "read_sam" | "read_gff"
        | "read_bed" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err(name, "a string path", &args[0], line, col));
            }
            Ok(Type::DataFrame)
        }
        // Monotonic clock: no arguments, returns seconds as a Float.
        "clock_monotonic" => {
            if !args.is_empty() {
                return Err(arity_err(name, 0, args.len(), line, col));
            }
            Ok(Type::Float)
        }
        // `read_int()` → read one integer from stdin (a line), or `missing` on EOF /
        // non-numeric input. The console-input primitive; non-deterministic, so (like
        // `print`/`sleep`) it lives outside the differential oracle.
        "read_int" => {
            if !args.is_empty() {
                return Err(arity_err(name, 0, args.len(), line, col));
            }
            Ok(Type::Int)
        }
        // generic readers + hashing + fs ops: one string argument each
        "read_text" | "read_json" | "read_dir" | "file_exists" | "sha256" | "remove_file"
        | "mkdir" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            let what = if name == "sha256" { "a string" } else { "a string path" };
            if !compatible(&args[0], &Type::String) {
                return Err(type_err(name, what, &args[0], line, col));
            }
            Ok(match name {
                "read_text" | "sha256" => Type::String,
                "file_exists" | "remove_file" | "mkdir" => Type::Bool,
                "read_dir" => Type::Array(Box::new(Type::String)),
                // JSON shape isn't known statically; Unknown keeps field/index access permissive.
                _ => Type::Unknown,
            })
        }
        // `read_vcf`/`read_bam` scan with one argument; the optional region second
        // argument runs an indexed query, so these readers accept one or two strings.
        "read_vcf" | "read_bam" => {
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
        "atan2" | "hypot" | "min" | "max" => {
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
        // three-arg numeric: `clamp(x, lo, hi)` (scalar; the array form is the `.clamp` method)
        "clamp" => {
            if args.len() != 3 {
                return Err(arity_err(name, 3, args.len(), line, col));
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
            Ok(Type::Num)
        }
        "log" => {
            // log(x) = natural log (1 arg) or log(x, base) (2 args). Broadcasts.
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`log` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                ));
            }
            if any(args, |t| matches!(t, Type::Array(_) | Type::Tensor | Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !is_numeric(a) {
                    return Err(type_err("log", "a number", a, line, col));
                }
            }
            Ok(Type::Float)
        }
        "gcd" => {
            if args.len() != 2 {
                return Err(arity_err("gcd", 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                // Float is allowed at the type level (an integer-valued float like 1.0 from
                // least_squares/lll is accepted at runtime); a fractional one errors there.
                if !matches!(a, Type::Int | Type::Num | Type::Float) {
                    return Err(type_err("gcd", "an integer", a, line, col));
                }
            }
            Ok(Type::Int)
        }
        "isqrt" => {
            if args.len() != 1 {
                return Err(arity_err("isqrt", 1, args.len(), line, col));
            }
            if matches!(args[0], Type::Unknown) {
                return Ok(Type::Unknown);
            }
            if matches!(args[0], Type::Missing) {
                return Ok(Type::Missing);
            }
            // Float is allowed at the type level (an integer-valued float is accepted at
            // runtime; a fractional or negative one errors there).
            if !matches!(args[0], Type::Int | Type::Num | Type::Float) {
                return Err(type_err("isqrt", "an integer", &args[0], line, col));
            }
            Ok(Type::Int)
        }
        "primes" => {
            if args.len() != 1 {
                return Err(arity_err("primes", 1, args.len(), line, col));
            }
            if matches!(args[0], Type::Unknown) {
                return Ok(Type::Unknown);
            }
            if matches!(args[0], Type::Missing) {
                return Ok(Type::Missing);
            }
            if !matches!(args[0], Type::Int | Type::Num | Type::Float) {
                return Err(type_err("primes", "an integer", &args[0], line, col));
            }
            Ok(Type::Array(Box::new(Type::Int)))
        }
        "chr" => {
            if args.len() != 1 {
                return Err(arity_err("chr", 1, args.len(), line, col));
            }
            if matches!(args[0], Type::Missing) {
                return Ok(Type::Missing);
            }
            if !matches!(args[0], Type::Int | Type::Num | Type::Unknown) {
                return Err(type_err("chr", "a codepoint integer", &args[0], line, col));
            }
            Ok(Type::String)
        }
        "ord" => {
            if args.len() != 1 {
                return Err(arity_err("ord", 1, args.len(), line, col));
            }
            if matches!(args[0], Type::Missing) {
                return Ok(Type::Missing);
            }
            if !matches!(args[0], Type::String | Type::Dna | Type::Unknown) {
                return Err(type_err("ord", "a single-character string", &args[0], line, col));
            }
            Ok(Type::Int)
        }
        "hmac_sha256" => {
            if args.len() != 2 {
                return Err(arity_err("hmac_sha256", 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !matches!(a, Type::String | Type::Unknown) {
                    return Err(type_err("hmac_sha256", "a string", a, line, col));
                }
            }
            Ok(Type::String)
        }
        "base64_encode" | "base64_decode" | "hex_encode" | "hex_decode" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if matches!(args[0], Type::Missing) {
                return Ok(Type::Missing);
            }
            if !matches!(args[0], Type::String | Type::Unknown) {
                return Err(type_err(name, "a string", &args[0], line, col));
            }
            Ok(Type::String)
        }
        "aes_keygen" => {
            if !args.is_empty() {
                return Err(arity_err("aes_keygen", 0, args.len(), line, col));
            }
            Ok(Type::String)
        }
        "aes_encrypt" | "aes_decrypt" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !matches!(a, Type::String | Type::Unknown) {
                    return Err(type_err(name, "a string", a, line, col));
                }
            }
            Ok(Type::String)
        }
        // A keypair record `{private, public}` — Unknown keeps field access permissive.
        "ed25519_keygen" => {
            if !args.is_empty() {
                return Err(arity_err("ed25519_keygen", 0, args.len(), line, col));
            }
            Ok(Type::Unknown)
        }
        "ed25519_sign" | "ed25519_verify" => {
            let expected = if name == "ed25519_sign" { 2 } else { 3 };
            if args.len() != expected {
                return Err(arity_err(name, expected, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !matches!(a, Type::String | Type::Unknown) {
                    return Err(type_err(name, "a string", a, line, col));
                }
            }
            Ok(if name == "ed25519_sign" { Type::String } else { Type::Bool })
        }
        "rational" => {
            if args.len() != 2 {
                return Err(arity_err("rational", 2, args.len(), line, col));
            }
            // an exact rational; Unknown keeps arithmetic on it permissive
            Ok(Type::Unknown)
        }
        "numerator" | "denominator" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::Int)
        }
        // autodiff: a tracked value / its forward value / a gradient. The graph node
        // is a runtime-only value, so the checker stays permissive (Unknown).
        "variable" | "value_of" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            Ok(Type::Unknown)
        }
        "gradient" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            Ok(Type::Unknown)
        }
        // argmax/argmin over an array or tensor → the Int index of the extreme value.
        "argmax" | "argmin" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !matches!(&args[0], Type::Array(_) | Type::Tensor | Type::Unknown) {
                return Err(type_err(name, "an array or tensor of numbers", &args[0], line, col));
            }
            Ok(Type::Int)
        }
        "to_float" => {
            if args.len() != 1 {
                return Err(arity_err("to_float", 1, args.len(), line, col));
            }
            Ok(Type::Float)
        }
        "to_int" => {
            if args.len() != 1 {
                return Err(arity_err("to_int", 1, args.len(), line, col));
            }
            Ok(Type::Int)
        }
        // `dict()` → an empty Dict (ADR 0020); a runtime type, so `Unknown` to the checker.
        "dict" => {
            if !args.is_empty() {
                return Err(arity_err("dict", 0, args.len(), line, col));
            }
            Ok(Type::Unknown)
        }
        "lll" => {
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`lll` takes a basis and an optional delta, got {}", args.len()),
                    line,
                    col,
                ));
            }
            // A basis (array of vectors) -> the reduced basis (array of float arrays);
            // shape/element validation is the runtime's job.
            Ok(Type::Array(Box::new(Type::Array(Box::new(Type::Float)))))
        }
        "lll_exact" => {
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`lll_exact` takes a basis and an optional delta, got {}", args.len()),
                    line,
                    col,
                ));
            }
            // Exact integer LLL: an integer basis -> the reduced integer basis.
            Ok(Type::Array(Box::new(Type::Array(Box::new(Type::Int)))))
        }
        "align" => {
            if args.len() < 2 || args.len() > 4 {
                return Err(HelixError::new(
                    format!("`align` takes (a, b), (a, b, mode), or (a, b, mode, scoring), got {}", args.len()),
                    line,
                    col,
                ));
            }
            // two sequences (+ optional mode string and/or scoring record) -> a record
            Ok(Type::Record(vec![
                ("score".to_string(), Type::Int),
                ("matches".to_string(), Type::Int),
                ("length".to_string(), Type::Int),
                ("a_aligned".to_string(), array_of_unknown()),
                ("b_aligned".to_string(), array_of_unknown()),
            ]))
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
            let nums = || Type::Array(Box::new(Type::Float));
            Ok(Type::Record(vec![
                ("slope".to_string(), Type::Float),
                ("intercept".to_string(), Type::Float),
                ("r_squared".to_string(), Type::Float),
                ("slope_std_error".to_string(), Type::Float),
                ("slope_p_value".to_string(), Type::Float),
                ("rss".to_string(), Type::Float),
                ("predictions".to_string(), nums()),
                ("residuals".to_string(), nums()),
            ]))
        }
        "multiple_regression" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            // The first two args (predictors, y) drive the result; an optional 3rd is
            // the boolean `intercept` flag (validated at runtime).
            if any(&args[..2], |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(&args[..2], |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in &args[..2] {
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
                ("rss".to_string(), Type::Float),
                ("predictions".to_string(), nums()),
                ("residuals".to_string(), nums()),
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
        "http_get" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            // Returns a `{status, body}` record; Unknown keeps field access permissive.
            Ok(Type::Unknown)
        }
        "http_post" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            // `(url, body)` → a `{status, body}` record (Unknown keeps field access permissive).
            Ok(Type::Unknown)
        }
        "http_request" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            // `({method, url, body?, headers?})` → `{status, body, headers}` (Unknown: permissive).
            Ok(Type::Unknown)
        }
        "http_stream" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            // `({method, url, …})` → a streaming Net handle (Unknown, like `listen`; methods
            // `.status()`/`.next()` dispatch at runtime).
            Ok(Type::Unknown)
        }
        // Reproducible RNG: `random`/`randn` → Float array; `random_int` → Int array.
        // (argument values are validated at runtime).
        "random" | "randn" => Ok(Type::Array(Box::new(Type::Float))),
        "random_int" => Ok(Type::Array(Box::new(Type::Int))),
        "linspace" => Ok(Type::Array(Box::new(Type::Float))),
        // model-eval metrics over two arrays → a scalar Float
        "mse" | "rmse" | "mae" | "r2_score" => Ok(Type::Float),
        // information criteria for model selection → a scalar Float
        "aic" | "bic" => Ok(Type::Float),
        // classification metrics over two label arrays → a scalar Float
        "accuracy" | "precision" | "recall" | "f1_score" => Ok(Type::Float),
        // a binary confusion matrix → a `{tp, fp, fn, tn}` integer record
        "confusion_matrix" => Ok(Type::Record(vec![
            ("tp".to_string(), Type::Int),
            ("fp".to_string(), Type::Int),
            ("fn".to_string(), Type::Int),
            ("tn".to_string(), Type::Int),
        ])),
        "least_squares" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if any(&args[..2], |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(&args[..2], |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            let nums = || Type::Array(Box::new(Type::Float));
            Ok(Type::Record(vec![
                ("coefficients".to_string(), nums()),
                ("rss".to_string(), Type::Float),
                ("r_squared".to_string(), Type::Float),
                ("predictions".to_string(), nums()),
                ("residuals".to_string(), nums()),
            ]))
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
        // `length` is an alias for `count`; `index_of` is the first matching index (or
        // `missing` when absent, like `Dna.find` — typed `Int`).
        "count" | "length" | "index_of" => Type::Int,
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
        // `concat` keeps the element type; `flatten` removes one level of nesting.
        "concat" => Type::Array(Box::new(el.clone())),
        "flatten" => match el {
            Type::Array(inner) => Type::Array(inner.clone()),
            _ => Type::Array(Box::new(Type::Unknown)),
        },
        // descriptive stats over a numeric array
        "zscores" => Type::Array(Box::new(Type::Float)),
        "iqr" | "spread" | "standard_error" | "coefficient_of_variation" | "mean_gc" => Type::Float,
        // vector math
        "dot" | "norm" => Type::Float,
        "cumsum" => Type::Array(Box::new(Type::Float)),
        "product" => Type::Num,
        // ML helpers
        "argsort" => Type::Array(Box::new(Type::Int)),
        "softmax" => Type::Array(Box::new(Type::Float)),
        "clamp" | "bootstrap" => Type::Array(Box::new(el.clone())),
        "contains" => Type::Bool,
        "total_length" => Type::Int,
        // charts + text exports render to a String
        "bar_chart" | "histogram" | "line_chart" | "sparkline" | "scatter" | "svg_bar"
        | "svg_line" | "to_html" | "to_markdown" | "to_table" => Type::String,
        // a Dict is a runtime type (ADR 0020); the checker treats it as `Unknown` so
        // `.get`/`.contains`/indexing stay permissive (like `parse_json`'s result).
        "to_dict" => Type::Unknown,
        // writers perform I/O and return Unit
        "write_csv" | "write_tsv" | "write_json" | "write_fasta" | "write_fastq" => Type::Unit,
        // reproducible sampling: shuffle/sample keep the element type; choice yields one
        "shuffle" | "sample" => Type::Array(Box::new(el.clone())),
        "choice" => el.clone(),
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
        "take" | "drop" | "repeat" | "ljust" | "rjust" | "center" => Type::String,
        "count" | "length" => Type::Int,
        "split" => Type::Array(Box::new(Type::String)),
        "contains" | "starts_with" | "ends_with" => Type::Bool,
        // FASTQ Phred+33 quality string → per-base integer quality scores.
        "phred" => Type::Array(Box::new(Type::Int)),
        // parse a JSON string (shape unknown statically); write the text to a file.
        "parse_json" => Type::Unknown,
        "to_float" => Type::Float,
        "to_int" => Type::Int,
        "write_to" | "append_to" => Type::Unit,
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
        "length" | "count" => Type::Int,
        "gc_content" | "at_content" => Type::Float,
        "complement" | "reverse_complement" => Type::Dna,
        "kmers" | "windows" | "codons" => Type::Array(Box::new(Type::String)),
        // (kmer, count) tuples — the native packed spectrum (forward or strand-canonical).
        "kmer_counts" | "canonical_kmer_counts" => {
            Type::Array(Box::new(Type::Tuple(vec![Type::String, Type::Int])))
        }
        // 0-based index of the motif, or `missing` when absent.
        "find" => Type::Int,
        // Longest run of one identical base (a QC signal).
        "longest_homopolymer" => Type::Int,
        // All 0-based match positions (overlapping); cumulative GC-skew walk per base.
        "find_all" | "gc_skew" => Type::Array(Box::new(Type::Int)),
        // Per-base tally `{A, C, G, T, N}` and Hamming distance to another sequence.
        "base_counts" => Type::Record(vec![
            ("A".to_string(), Type::Int),
            ("C".to_string(), Type::Int),
            ("G".to_string(), Type::Int),
            ("T".to_string(), Type::Int),
            ("N".to_string(), Type::Int),
        ]),
        "hamming" => Type::Int,
        // Pairwise alignment result record (ADR 0015).
        "align" => Type::Record(vec![
            ("score".to_string(), Type::Int),
            ("cigar".to_string(), Type::String),
            ("query".to_string(), Type::String),
            ("target".to_string(), Type::String),
            ("start".to_string(), Type::Int),
            ("end".to_string(), Type::Int),
        ]),
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

/// Type of a record **dynamic-access** method (`get`/`has`/`keys`/`values`/`items`, see
/// `record_method`). `get` is permissive (Unknown — the field value's type isn't statically
/// known); `has` is Bool; the enumerators are arrays. Static `rec.field` access is typed the
/// normal way (this is the escape hatch for runtime-unknown shapes).
pub(super) fn record_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "get" => Type::Unknown,
        "has" => Type::Bool,
        "keys" => Type::Array(Box::new(Type::String)),
        "values" => Type::Array(Box::new(Type::Unknown)),
        "items" => Type::Array(Box::new(Type::Unknown)),
        other => {
            return Err(HelixError::new(format!("type Record has no method `{other}`"), line, col)
                .hint(
                    "records have dynamic access `get`/`has`/`keys`/`values`/`items` — or use \
                     `rec.field` directly for a known field.",
                ));
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
        "flatten" | "reshape" | "transpose" | "t" | "inv" | "solve" | "softmax" => Type::Tensor,
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
        "where" | "filter" | "select" | "sort" | "head" | "cache" | "with" | "join" | "vstack"
        | "unique" => Type::DataFrame,
        "group" => Type::GroupBy,
        "count" => Type::Int,
        "columns" => Type::Array(Box::new(Type::String)),
        // One column's values as an array; element type is the runtime schema boundary.
        "column" => array_of_unknown(),
        // serialize/write the frame
        "write_csv" | "write_tsv" | "write_json" | "write_parquet" => Type::Unit,
        "to_html" | "to_markdown" | "to_table" => Type::String,
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

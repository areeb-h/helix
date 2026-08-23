//! Builtins: the math standard library (broadcasting, missing-propagating) — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

#[inline]
pub(super) fn a_random(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    crate::rng::random(&args, line, col)
}

#[inline]
pub(super) fn a_randn(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    crate::rng::randn(&args, line, col)
}

#[inline]
pub(super) fn a_random_int(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    crate::rng::random_int(&args, line, col)
}

#[inline]
pub(super) fn a_sqrt(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_floor(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        if matches!(&args[0], Value::Node(_)) {
            return Err(crate::autodiff::not_differentiable(name, line, col));
        }
        let f: fn(f64) -> f64 = match name {
            "floor" => f64::floor,
            "ceil" => f64::ceil,
            "trunc" => f64::trunc,
            _ => unreachable!(),
        };
        apply_round_fn(name, f, &args[0], line, col)
    
}

#[inline]
pub(super) fn a_round(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // `round(x)` → nearest integer (Int); `round(x, d)` → round to `d`
        // decimal places (Float). Both broadcast over arrays and pass `missing`.
        if args.is_empty() || args.len() > 2 {
            return Err(HelixError::new(
                format!("`round` takes a number and an optional digit count, got {} arguments", args.len()),
                line,
                col,
            ));
        }
        if matches!(&args[0], Value::Node(_)) {
            return Err(crate::autodiff::not_differentiable("round", line, col));
        }
        if args.len() == 1 {
            return apply_round_fn("round", f64::round, &args[0], line, col);
        }
        let d = as_int(&args[1], "round", line, col)?;
        // Clamp the digit count into f64's decimal-exponent span before the
        // `as i32` narrowing. Without the clamp, a large `d` like `2^31` wraps
        // to `i32::MIN`, so `10^d` underflows to 0 and every result is `0/0`
        // = NaN. Beyond ~10^±308 the scale is inf/0 anyway, so nothing is lost.
        let scale = 10f64.powi(d.clamp(-308, 308) as i32);
        broadcast_unary(&args[0], &|s| match s {
            Value::Int(i) => Ok(Value::Float(*i as f64)),
            Value::Float(x) => {
                // Rounding to more precision than the scaled value can hold (so
                // `x * scale` overflows to ±inf) is a no-op — return `x`
                // unchanged rather than the inf/inf = NaN the formula would give.
                let r = (x * scale).round() / scale;
                Ok(Value::Float(if r.is_finite() { r } else { *x }))
            }
            other => Err(type_err("round", "a number or array of numbers", other, line, col)),
        })
    
}

#[inline]
pub(super) fn a_abs(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        // A tracked (autodiff) argument builds a graph node — same routing as
        // the unary-float family above; `abs` lives in its own arm only because
        // it also handles Ints.
        if matches!(&args[0], Value::Node(_)) {
            return crate::autodiff::unary_builtin(name, &args[0], line, col);
        }
        // `wrapping_abs` matches the wrapping-on-overflow convention used by the
        // arithmetic ops; a packed array maps over its buffer (no per-element box).
        super::super::apply_abs(args.into_iter().next().unwrap(), line, col)
    
}

#[inline]
pub(super) fn a_sign(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        if matches!(&args[0], Value::Node(_)) {
            return Err(crate::autodiff::not_differentiable("sign", line, col));
        }
        super::super::apply_sign(&args[0], line, col)
    
}

// IEEE float predicates → Bool (Bool array / 0.0-or-1.0 tensor mask when
// broadcast). An `Int`/`Rational` is always finite, never NaN/inf. These let a
// program guard a `NaN`/`inf` (e.g. from an `exp` overflow) BEFORE a comparison
// — which raises on a non-orderable `NaN` rather than silently mis-ordering.

#[inline]
pub(super) fn a_is_nan(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        float_predicate(&args[0], name, f64::is_nan, false, line, col)
    
}

#[inline]
pub(super) fn a_is_finite(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        float_predicate(&args[0], name, f64::is_finite, true, line, col)
    
}

#[inline]
pub(super) fn a_is_infinite(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        float_predicate(&args[0], name, f64::is_infinite, false, line, col)
    
}

// Monotonic seconds since the first call (process start), for `t0 =
// clock_monotonic()` … `clock_monotonic() - t0` timing. Impure + monotonic
// (never decreases); the absolute value is meaningless, only differences are.

#[inline]
pub(super) fn a_log(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    match args.len() {
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
}

#[inline]
pub(super) fn a_atan2(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 2, line, col)?;
        match two_nums(name, &args[0], &args[1], line, col)? {
            None => Ok(Value::Missing),
            Some((y, x)) => Ok(Value::Float(y.atan2(x))),
        }
    
}

#[inline]
pub(super) fn a_hypot(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 2, line, col)?;
        if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
            return Ok(Value::Missing);
        }
        if matches!(args[0], Value::Node(_)) || matches!(args[1], Value::Node(_)) {
            return crate::autodiff::binary_builtin(name, &args[0], &args[1], line, col);
        }
        if matches!(args[0], Value::Tensor(_)) || matches!(args[1], Value::Tensor(_)) {
            match (super::tensor_operand(&args[0]), super::tensor_operand(&args[1])) {
                (Some(ta), Some(tb)) => {
                    return Ok(Value::Tensor(Rc::new(crate::tensor::zip_with(
                        &ta,
                        &tb,
                        f64::hypot,
                        line,
                        col,
                    )?)))
                }
                (a, _) => {
                    let bad = if a.is_none() { &args[0] } else { &args[1] };
                    return Err(type_err(name, "a number or tensor", bad, line, col));
                }
            }
        }
        match two_nums(name, &args[0], &args[1], line, col)? {
            None => Ok(Value::Missing),
            Some((a, b)) => Ok(Value::Float(a.hypot(b))),
        }
    
}

#[inline]
pub(super) fn a_gcd(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_primes(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // All primes below n as a packed Int array — the native Sieve of
        // Eratosthenes. The sieve is an inherently MUTABLE algorithm Helix's
        // immutable surface cannot express efficiently (functional trial
        // division is O(N√N)); like the tensor `.matmul()`, it delegates to
        // Rust. `primes(10000000).count()` = 664579 in ~sieve time, not ~90 s.
        arity(name, &args, 1, line, col)?;
        if matches!(args[0], Value::Missing) {
            return Ok(Value::Missing);
        }
        let n = as_int(&args[0], "primes", line, col)?;
        if n > 100_000_000 {
            return Err(HelixError::new(
                format!("`primes` supports n up to 100000000, got {n}"),
                line,
                col,
            )
            .hint("the sieve buffer grows with n; sieve in segments for larger bounds."));
        }
        Ok(Value::int_array(sieve_primes(n)))
    
}

#[inline]
pub(super) fn a_isqrt(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        if matches!(args[0], Value::Missing) {
            return Ok(Value::Missing);
        }
        let n = as_int(&args[0], "isqrt", line, col)?;
        if n < 0 {
            return Err(HelixError::new(
                format!(
                    "`isqrt` got {n}, but the integer square root is undefined for a negative number"
                ),
                line,
                col,
            )
            .hint("isqrt(n) = floor(sqrt(n)) and needs n >= 0."));
        }
        Ok(Value::Int(isqrt_i64(n)))
    
}

#[inline]
pub(super) fn a_rational(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_numerator(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_lll(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_lll_exact(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
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

#[inline]
pub(super) fn a_min(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 2, line, col)?;
        if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
            return Ok(Value::Missing);
        }
        // A tracked argument builds a graph node (ties send the gradient
        // to the FIRST argument — the convention relu's kink pins).
        if matches!(args[0], Value::Node(_)) || matches!(args[1], Value::Node(_)) {
            return crate::autodiff::binary_builtin(name, &args[0], &args[1], line, col);
        }
        // PLAIN tensors broadcast elementwise, exactly as the tracked pair
        // does — the sweep found `max(tensor, 0.0)` refusing while
        // `max(variable(tensor), 0.0)` answered (the forked-by-capability
        // wound), and while `relu(t)` — documented as max(t, 0) — worked.
        if matches!(args[0], Value::Tensor(_)) || matches!(args[1], Value::Tensor(_)) {
            match (super::tensor_operand(&args[0]), super::tensor_operand(&args[1])) {
                (Some(ta), Some(tb)) => {
                    let f: fn(f64, f64) -> f64 = if name == "min" { f64::min } else { f64::max };
                    return Ok(Value::Tensor(Rc::new(crate::tensor::zip_with(
                        &ta, &tb, f, line, col,
                    )?)));
                }
                (a, _) => {
                    let bad = if a.is_none() { &args[0] } else { &args[1] };
                    return Err(type_err(name, "a number or tensor", bad, line, col));
                }
            }
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

#[inline]
pub(super) fn a_clamp(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // `clamp(x, lo, hi)` — the scalar companion to the array `.clamp(lo, hi)` method,
        // mirroring `min`/`max`: it returns one of the three ORIGINAL values (so an `Int`
        // stays `Int`), never coercing to `Float`. `lo > hi` is a caller error.
        arity(name, &args, 3, line, col)?;
        if args.iter().any(|a| matches!(a, Value::Missing)) {
            return Ok(Value::Missing);
        }
        // A tracked argument: clamp IS min(max(x, lo), hi), so the tape
        // gets exactly that composition — gradient 1 inside the band
        // (boundaries included, per max/min's ties-to-first rule), 0
        // outside. Plain bounds are still validated first.
        if args.iter().any(|a| matches!(a, Value::Node(_))) {
            if let (Some(lo), Some(hi)) = (args[1].as_f64(), args[2].as_f64())
                && lo > hi
            {
                return Err(HelixError::new(
                    format!("`clamp` needs lo <= hi, got lo = {lo}, hi = {hi}"),
                    line,
                    col,
                )
                .hint("clamp(x, lo, hi) bounds x to [lo, hi]; pass the low bound before the high one."));
            }
            let m = crate::autodiff::binary_builtin("max", &args[0], &args[1], line, col)?;
            return crate::autodiff::binary_builtin("min", &m, &args[2], line, col);
        }
        // Plain tensors: clamp IS min(max(x, lo), hi), broadcast — the same
        // composition the tracked path uses. Plain scalar bounds still refuse
        // lo > hi first.
        if args.iter().any(|a| matches!(a, Value::Tensor(_))) {
            if let (Some(lo), Some(hi)) = (args[1].as_f64(), args[2].as_f64())
                && lo > hi
            {
                return Err(HelixError::new(
                    format!("`clamp` needs lo <= hi, got lo = {lo}, hi = {hi}"),
                    line,
                    col,
                )
                .hint("clamp(x, lo, hi) bounds x to [lo, hi]; pass the low bound before the high one."));
            }
            match (
                super::tensor_operand(&args[0]),
                super::tensor_operand(&args[1]),
                super::tensor_operand(&args[2]),
            ) {
                (Some(tx), Some(tlo), Some(thi)) => {
                    let m = crate::tensor::zip_with(&tx, &tlo, f64::max, line, col)?;
                    return Ok(Value::Tensor(Rc::new(crate::tensor::zip_with(
                        &m, &thi, f64::min, line, col,
                    )?)));
                }
                (a, b, _) => {
                    let bad = if a.is_none() {
                        &args[0]
                    } else if b.is_none() {
                        &args[1]
                    } else {
                        &args[2]
                    };
                    return Err(type_err(name, "a number or tensor", bad, line, col));
                }
            }
        }
        let x = args[0].as_f64().ok_or_else(|| type_err(name, "a number", &args[0], line, col))?;
        let lo = args[1].as_f64().ok_or_else(|| type_err(name, "a number", &args[1], line, col))?;
        let hi = args[2].as_f64().ok_or_else(|| type_err(name, "a number", &args[2], line, col))?;
        if lo > hi {
            return Err(HelixError::new(
                format!("`clamp` needs lo <= hi, got lo = {lo}, hi = {hi}"),
                line,
                col,
            )
            .hint("clamp(x, lo, hi) bounds x to [lo, hi]; pass the low bound before the high one."));
        }
        Ok(if x < lo {
            args[1].clone()
        } else if x > hi {
            args[2].clone()
        } else {
            args[0].clone()
        })
    
}

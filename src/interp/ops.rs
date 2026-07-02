//! Binary-operator evaluation (`eval_binary`) and its scalar helpers
//! (`arith`/`compare`/`num_operand`/`values_equal`), shared by the tree-walker
//! and the VM's `binary()` fast path. Includes the array/tensor broadcasting and
//! `missing` propagation rules (ADR-0001).

use std::rc::Rc;

use crate::ast::BinOp;
use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};
use crate::error::HelixError;
use crate::tensor;
use crate::value::Value;

pub(crate) fn eval_binary(
    op: &BinOp,
    mut l: Value,
    mut r: Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    use BinOp::*;
    // Missing propagates through every arithmetic, comparison, and equality
    // operator — including `missing == missing` -> missing, so equality can
    // never be used to test for missingness. Use `.is_missing()` instead.
    if matches!(l, Value::Missing) || matches!(r, Value::Missing) {
        return Ok(Value::Missing);
    }

    // Autodiff: if either operand is a tracked value, build a graph node instead of
    // computing a plain result (so `gradient(...)` can backpropagate through it).
    if matches!(l, Value::Node(_)) || matches!(r, Value::Node(_)) {
        return crate::autodiff::binary(op, &l, &r, line, col);
    }

    // A Python operand forwards to Python's operator protocol (`a + b`, `a < b`, …).
    if matches!(l, Value::PyObject(_)) || matches!(r, Value::PyObject(_)) {
        return crate::python::binary(op, &l, &r, line, col);
    }

    // Elementwise broadcasting for arithmetic: array⊕scalar, scalar⊕array, and
    // array⊕array (same length). Comparison/equality deliberately do NOT
    // broadcast — `==` is whole-value, avoiding NumPy's "ambiguous truth value"
    // trap; use `.map`/`.where` for elementwise predicates.
    if matches!(op, Add | Sub | Mul | Div | FloorDiv | Mod | Pow) {
        // Memory fast path: when an array operand is a unique temporary (`Rc` count 1, as
        // every intermediate in a chain like `(a+b)*c` is), reuse its buffer in place
        // instead of allocating a new one. Falls through untouched (`l`/`r` intact) when
        // nothing is reusable.
        if let Some(v) = try_inplace_broadcast(op, &mut l, &mut r) {
            return Ok(v);
        }
        // Fast path: `Add`/`Sub`/`Mul` over packed numeric arrays run as a tight
        // loop on the `i64`/`f64` buffer (no per-element `Value` boxing or dispatch)
        // and keep the result packed. Other ops / `Values` arrays fall through.
        if let Some(v) = typed_broadcast(op, &l, &r) {
            return Ok(v);
        }
        match (&l, &r) {
            (Value::Array(a), Value::Array(b)) => {
                if a.len() != b.len() {
                    return Err(HelixError::new(
                        format!(
                            "cannot `{}` arrays of different lengths ({} and {})",
                            op.symbol(),
                            a.len(),
                            b.len()
                        ),
                        line,
                        col,
                    )
                    .hint("elementwise operations need matching lengths."));
                }
                let mut out = Vec::with_capacity(a.len());
                for (x, y) in a.iter_values().zip(b.iter_values()) {
                    out.push(eval_binary(op, x, y, line, col)?);
                }
                return Ok(Value::array_sniff(out));
            }
            (Value::Array(a), scalar) => {
                let mut out = Vec::with_capacity(a.len());
                for x in a.iter_values() {
                    out.push(eval_binary(op, x, scalar.clone(), line, col)?);
                }
                return Ok(Value::array_sniff(out));
            }
            (scalar, Value::Array(b)) => {
                let mut out = Vec::with_capacity(b.len());
                for y in b.iter_values() {
                    out.push(eval_binary(op, scalar.clone(), y, line, col)?);
                }
                return Ok(Value::array_sniff(out));
            }
            // Tensor arithmetic: tensor⊕tensor (NumPy broadcasting), tensor⊕scalar.
            (Value::Tensor(a), Value::Tensor(b)) => {
                return Ok(Value::Tensor(Rc::new(tensor::elementwise(op, a, b, line, col)?)));
            }
            (Value::Tensor(a), s) if s.as_f64().is_some() => {
                return Ok(Value::Tensor(Rc::new(tensor::scalar_op(
                    op,
                    a,
                    s.as_f64().unwrap(),
                    true,
                ))));
            }
            (s, Value::Tensor(b)) if s.as_f64().is_some() => {
                return Ok(Value::Tensor(Rc::new(tensor::scalar_op(
                    op,
                    b,
                    s.as_f64().unwrap(),
                    false,
                ))));
            }
            _ => {}
        }
    }

    // Exact arbitrary-precision rational arithmetic/comparison when a Rational is
    // present (a scalar; rational *arrays* broadcast element-wise to here).
    if matches!(l, Value::Rational(_)) || matches!(r, Value::Rational(_)) {
        return rational_binary(op, &l, &r, line, col);
    }
    match op {
        Add | Sub | Mul => arith(op, &l, &r, line, col),
        Div => {
            let a = num_operand(op, &l, line, col)?;
            let b = num_operand(op, &r, line, col)?;
            if b == 0.0 {
                return Err(HelixError::new("division by zero", line, col)
                    .hint("guard the denominator, e.g. `if d != 0` or check your data."));
            }
            Ok(Value::Float(a / b))
        }
        Mod => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => {
                if *b == 0 {
                    Err(HelixError::new("modulo by zero", line, col))
                } else {
                    Ok(Value::Int(a.rem_euclid(*b)))
                }
            }
            _ => {
                let a = num_operand(op, &l, line, col)?;
                let b = num_operand(op, &r, line, col)?;
                Ok(Value::Float(a.rem_euclid(b)))
            }
        },
        FloorDiv => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => {
                if *b == 0 {
                    Err(HelixError::new("integer division by zero", line, col)
                        .hint("guard the divisor, e.g. `if d != 0`."))
                } else {
                    Ok(Value::Int(a.div_euclid(*b)))
                }
            }
            _ => {
                let a = num_operand(op, &l, line, col)?;
                let b = num_operand(op, &r, line, col)?;
                if b == 0.0 {
                    return Err(HelixError::new("division by zero", line, col));
                }
                Ok(Value::Float(a.div_euclid(b)))
            }
        },
        Pow => match (&l, &r) {
            // Integer power stays Int when the exponent is a non-negative,
            // in-range integer and the result doesn't overflow.
            (Value::Int(a), Value::Int(b)) if *b >= 0 && *b <= u32::MAX as i64 => {
                match a.checked_pow(*b as u32) {
                    Some(v) => Ok(Value::Int(v)),
                    None => Ok(Value::Float((*a as f64).powf(*b as f64))),
                }
            }
            _ => {
                let a = num_operand(op, &l, line, col)?;
                let b = num_operand(op, &r, line, col)?;
                // Strength-reduce an integer exponent: `x ** 2` is x*x (powi), far
                // cheaper than the exp/log `powf` and what numpy does too. A fractional
                // exponent (e.g. `x ** 0.5`) still uses powf.
                let v = if b.fract() == 0.0 && b.abs() <= i32::MAX as f64 {
                    a.powi(b as i32)
                } else {
                    a.powf(b)
                };
                Ok(Value::Float(v))
            }
        },
        Eq => Ok(Value::Bool(values_equal(&l, &r))),
        Ne => Ok(Value::Bool(!values_equal(&l, &r))),
        Lt | Gt | Le | Ge => compare(op, &l, &r, line, col),
        BitAnd | BitOr | BitXor | Shl | Shr => bitwise(op, &l, &r, line, col),
        And | Or | Coalesce => unreachable!("handled with short-circuit in eval"),
    }
}

/// Integer bitwise operators. Both operands must be `Int`. Shift amounts are
/// restricted to `0..=63` (a wider or negative shift is undefined for `i64`, so it
/// is a clean error rather than a panic or silent wrap) — keeping the operator
/// total and the three engines in agreement.
pub(crate) fn bitwise(
    op: &BinOp,
    l: &Value,
    r: &Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let (a, b) = match (l, r) {
        (Value::Int(a), Value::Int(b)) => (*a, *b),
        _ => {
            return Err(HelixError::new(
                format!("`{}` needs two integers", op.symbol()),
                line,
                col,
            )
            .hint("bitwise operators (`&` `|` `^` `<<` `>>`) work on integers only."));
        }
    };
    match op {
        BinOp::BitAnd => Ok(Value::Int(a & b)),
        BinOp::BitOr => Ok(Value::Int(a | b)),
        BinOp::BitXor => Ok(Value::Int(a ^ b)),
        BinOp::Shl | BinOp::Shr => {
            if !(0..=63).contains(&b) {
                return Err(HelixError::new(
                    format!("shift amount {b} is out of range"),
                    line,
                    col,
                )
                .hint("shift by 0..=63 bits (an i64 has 64 bits)."));
            }
            Ok(Value::Int(if matches!(op, BinOp::Shl) { a << b } else { a >> b }))
        }
        _ => unreachable!("non-bitwise op in bitwise()"),
    }
}

/// Vectorized `Add`/`Sub`/`Mul` over **typed** numeric arrays — a tight loop on the
/// packed `i64`/`f64` buffer producing a packed result, with **no** per-element
/// `Value` allocation or operator dispatch. Returns `None` (defer to the general,
/// element-by-element path) for other operators, a `Values` array, a non-numeric
/// scalar, or a length mismatch. Semantics match [`arith`] exactly: `Int`×`Int`
/// wraps (two's complement); any `Float` widens both sides to `f64`.
/// Packed-array elementwise math at or above this length runs in parallel (rayon).
/// Below it the small-array hot path stays sequential. The map is order-preserving,
/// so the output buffer is byte-identical to the sequential one.
const PAR_BCAST_THRESHOLD: usize = 1 << 15;

fn pz_f64(a: &[f64], b: &[f64], f: impl Fn(f64, f64) -> f64 + Sync) -> Vec<f64> {
    if a.len() >= PAR_BCAST_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter().zip(b.par_iter()).map(|(&x, &y)| f(x, y)).collect()
    } else {
        a.iter().zip(b).map(|(&x, &y)| f(x, y)).collect()
    }
}
fn pm_f64(a: &[f64], f: impl Fn(f64) -> f64 + Sync) -> Vec<f64> {
    if a.len() >= PAR_BCAST_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter().map(|&x| f(x)).collect()
    } else {
        a.iter().map(|&x| f(x)).collect()
    }
}
fn pz_i64(a: &[i64], b: &[i64], f: impl Fn(i64, i64) -> i64 + Sync) -> Vec<i64> {
    if a.len() >= PAR_BCAST_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter().zip(b.par_iter()).map(|(&x, &y)| f(x, y)).collect()
    } else {
        a.iter().zip(b).map(|(&x, &y)| f(x, y)).collect()
    }
}
fn pm_i64(a: &[i64], f: impl Fn(i64) -> i64 + Sync) -> Vec<i64> {
    if a.len() >= PAR_BCAST_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter().map(|&x| f(x)).collect()
    } else {
        a.iter().map(|&x| f(x)).collect()
    }
}

// `Add`/`Sub`/`Mul` over `i64`/`f64`, hoisted so the in-place and allocating broadcast
// paths share one definition (so they agree to the bit). Integer arithmetic wraps.
fn iop(op: &BinOp, x: i64, y: i64) -> i64 {
    match op {
        BinOp::Add => x.wrapping_add(y),
        BinOp::Sub => x.wrapping_sub(y),
        _ => x.wrapping_mul(y),
    }
}
fn fop(op: &BinOp, x: f64, y: f64) -> f64 {
    match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        _ => x * y,
    }
}
fn scalar_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

// In-place maps/zips — write back into a uniquely-owned buffer instead of allocating a
// new one. Order-preserving and parallel past the same threshold, so the result is
// byte-identical to the allocating `pm_*`/`pz_*` path. `dst[i] = f(dst[i])` /
// `dst[i] = f(dst[i], other[i])`.
fn pm_f64_inplace(a: &mut [f64], f: impl Fn(f64) -> f64 + Sync + Send) {
    if a.len() >= PAR_BCAST_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter_mut().for_each(|x| *x = f(*x));
    } else {
        a.iter_mut().for_each(|x| *x = f(*x));
    }
}
fn pm_i64_inplace(a: &mut [i64], f: impl Fn(i64) -> i64 + Sync + Send) {
    if a.len() >= PAR_BCAST_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter_mut().for_each(|x| *x = f(*x));
    } else {
        a.iter_mut().for_each(|x| *x = f(*x));
    }
}
fn zip_f64_inplace(a: &mut [f64], b: &[f64], f: impl Fn(f64, f64) -> f64 + Sync + Send) {
    if a.len() >= PAR_BCAST_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter_mut().zip(b.par_iter()).for_each(|(x, &y)| *x = f(*x, y));
    } else {
        a.iter_mut().zip(b).for_each(|(x, &y)| *x = f(*x, y));
    }
}
fn zip_i64_inplace(a: &mut [i64], b: &[i64], f: impl Fn(i64, i64) -> i64 + Sync + Send) {
    if a.len() >= PAR_BCAST_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter_mut().zip(b.par_iter()).for_each(|(x, &y)| *x = f(*x, y));
    } else {
        a.iter_mut().zip(b).for_each(|(x, &y)| *x = f(*x, y));
    }
}

/// Reuse a uniquely-owned (`Rc` strong-count 1) array buffer for `Add`/`Sub`/`Mul`,
/// mutating it in place rather than allocating a fresh one — the key saving in
/// elementwise chains (`(a + b) * c`, `xs * 2 + 1`), where every intermediate is a unique
/// temporary. A bound variable has strong-count > 1 (the binding plus the evaluated copy),
/// so `Rc::get_mut` returns `None` and we never mutate shared data. Only type-preserving
/// cases reuse in place (Floats±num-scalar, Ints±int-scalar, Floats∘Floats, Ints∘Ints);
/// everything else returns `None` and falls through to the allocating path. Byte-identical
/// result, so both engines stay in lockstep.
fn try_inplace_broadcast(op: &BinOp, l: &mut Value, r: &mut Value) -> Option<Value> {
    use crate::value::ArrayData;
    if !matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) {
        return None;
    }
    // array ⊕ scalar  → reuse the (unique) array.
    if let (Value::Array(a), Some(sf)) = (&mut *l, scalar_f64(r)) {
        let done = match Rc::get_mut(a) {
            Some(ArrayData::Floats(v)) => {
                pm_f64_inplace(v, |x| fop(op, x, sf));
                true
            }
            Some(ArrayData::Ints(v)) => {
                if let Value::Int(k) = *r {
                    pm_i64_inplace(v, |x| iop(op, x, k));
                    true
                } else {
                    false // Ints array ⊕ Float scalar → Floats; can't reuse the i64 buffer
                }
            }
            _ => false,
        };
        if done {
            return Some(std::mem::replace(l, Value::Unit));
        }
    }
    // scalar ⊕ array  → reuse the (unique) array, with the scalar as the left operand.
    if let (Some(sf), Value::Array(a)) = (scalar_f64(l), &mut *r) {
        let done = match Rc::get_mut(a) {
            Some(ArrayData::Floats(v)) => {
                pm_f64_inplace(v, |x| fop(op, sf, x));
                true
            }
            Some(ArrayData::Ints(v)) => {
                if let Value::Int(k) = *l {
                    pm_i64_inplace(v, |x| iop(op, k, x));
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if done {
            return Some(std::mem::replace(r, Value::Unit));
        }
    }
    // array ⊕ array (same length, same packed type) → reuse a unique buffer: prefer the
    // left, else the right (with the operands swapped in the closure to keep `Sub` order).
    let same_len = matches!((&*l, &*r), (Value::Array(a), Value::Array(b)) if a.len() == b.len());
    if same_len {
        if let (Value::Array(a), Value::Array(b)) = (&mut *l, &*r) {
            let done = match (Rc::get_mut(a), &**b) {
                (Some(ArrayData::Floats(va)), ArrayData::Floats(vb)) => {
                    zip_f64_inplace(va, vb, |x, y| fop(op, x, y));
                    true
                }
                (Some(ArrayData::Ints(va)), ArrayData::Ints(vb)) => {
                    zip_i64_inplace(va, vb, |x, y| iop(op, x, y));
                    true
                }
                _ => false,
            };
            if done {
                return Some(std::mem::replace(l, Value::Unit));
            }
        }
        if let (Value::Array(a), Value::Array(b)) = (&*l, &mut *r) {
            let done = match (&**a, Rc::get_mut(b)) {
                (ArrayData::Floats(va), Some(ArrayData::Floats(vb))) => {
                    zip_f64_inplace(vb, va, |y, x| fop(op, x, y)); // store into b; a is left
                    true
                }
                (ArrayData::Ints(va), Some(ArrayData::Ints(vb))) => {
                    zip_i64_inplace(vb, va, |y, x| iop(op, x, y));
                    true
                }
                _ => false,
            };
            if done {
                return Some(std::mem::replace(r, Value::Unit));
            }
        }
    }
    None
}

fn typed_broadcast(op: &BinOp, l: &Value, r: &Value) -> Option<Value> {
    use crate::value::ArrayData;
    if !matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow) {
        return None;
    }
    fn f64_view(ad: &ArrayData) -> Option<std::borrow::Cow<'_, [f64]>> {
        match ad {
            ArrayData::Floats(v) => Some(std::borrow::Cow::Borrowed(v)),
            ArrayData::Ints(v) => Some(std::borrow::Cow::Owned(v.iter().map(|&n| n as f64).collect())),
            ArrayData::Values(_) => None,
        }
    }
    // `/` is always float; a zero divisor must raise the *same* error as the scalar
    // path, so on any zero we bail to the general path (which produces that error).
    if matches!(op, BinOp::Div) {
        return match (l, r) {
            (Value::Array(a), Value::Array(b)) => {
                if a.len() != b.len() {
                    return None;
                }
                let (fa, fb) = (f64_view(a)?, f64_view(b)?);
                if fb.contains(&0.0) {
                    return None;
                }
                Some(Value::float_array(pz_f64(&fa, &fb, |x, y| x / y)))
            }
            (Value::Array(a), s) => {
                let (fa, sf) = (f64_view(a)?, scalar_f64(s)?);
                if sf == 0.0 {
                    return None;
                }
                Some(Value::float_array(pm_f64(&fa, |x| x / sf)))
            }
            (s, Value::Array(a)) => {
                let (fa, sf) = (f64_view(a)?, scalar_f64(s)?);
                if fa.contains(&0.0) {
                    return None;
                }
                Some(Value::float_array(pm_f64(&fa, |x| sf / x)))
            }
            _ => None,
        };
    }
    // `**` over a FLOAT-base array (e.g. `xs ** 2`). An `Int`-base power must stay
    // `Int` (and may overflow to Float per element), so it's left to the general path.
    if matches!(op, BinOp::Pow) {
        return match (l, r) {
            (Value::Array(a), s) => match &**a {
                ArrayData::Floats(fa) => {
                    let sf = scalar_f64(s)?;
                    // Strength-reduce an integer exponent (`xs ** 2` → x*x via powi),
                    // matching the scalar path so array and scalar agree to the bit.
                    let out = if sf.fract() == 0.0 && sf.abs() <= i32::MAX as f64 {
                        let n = sf as i32;
                        pm_f64(fa, move |x| x.powi(n))
                    } else {
                        pm_f64(fa, move |x| x.powf(sf))
                    };
                    Some(Value::float_array(out))
                }
                _ => None,
            },
            _ => None,
        };
    }
    match (l, r) {
        // array ⊕ array (same length)
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return None; // length-mismatch error belongs to the general path
            }
            if let (ArrayData::Ints(xa), ArrayData::Ints(xb)) = (&**a, &**b) {
                return Some(Value::int_array(pz_i64(xa, xb, |x, y| iop(op, x, y))));
            }
            let (fa, fb) = (f64_view(a)?, f64_view(b)?);
            Some(Value::float_array(pz_f64(&fa, &fb, |x, y| fop(op, x, y))))
        }
        // array ⊕ scalar  (`a[i] op s`)
        (Value::Array(a), s) => {
            if let (ArrayData::Ints(xa), Value::Int(k)) = (&**a, s) {
                let k = *k;
                return Some(Value::int_array(pm_i64(xa, |x| iop(op, x, k))));
            }
            let (fa, sf) = (f64_view(a)?, scalar_f64(s)?);
            Some(Value::float_array(pm_f64(&fa, |x| fop(op, x, sf))))
        }
        // scalar ⊕ array  (`s op a[i]`)
        (s, Value::Array(a)) => {
            if let (Value::Int(k), ArrayData::Ints(xa)) = (s, &**a) {
                let k = *k;
                return Some(Value::int_array(pm_i64(xa, |x| iop(op, k, x))));
            }
            let (fa, sf) = (f64_view(a)?, scalar_f64(s)?);
            Some(Value::float_array(pm_f64(&fa, |x| fop(op, sf, x))))
        }
        _ => None,
    }
}

fn arith(op: &BinOp, l: &Value, r: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    if let (Value::Int(a), Value::Int(b)) = (l, r) {
        // Integer overflow wraps (two's complement), matching the JIT and Rust
        // release / Go / Java semantics — never a debug-build panic. Values beyond
        // the i64 range should use floats.
        let v = match op {
            BinOp::Add => a.wrapping_add(*b),
            BinOp::Sub => a.wrapping_sub(*b),
            BinOp::Mul => a.wrapping_mul(*b),
            _ => unreachable!(),
        };
        return Ok(Value::Int(v));
    }
    let a = num_operand(op, l, line, col)?;
    let b = num_operand(op, r, line, col)?;
    let v = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        _ => unreachable!(),
    };
    Ok(Value::Float(v))
}

fn num_operand(op: &BinOp, v: &Value, line: usize, col: usize) -> Result<f64, HelixError> {
    v.as_f64().ok_or_else(|| {
        let err = HelixError::new(
            format!(
                "operator `{}` needs numbers, but got a {}",
                op.symbol(),
                v.type_name()
            ),
            line,
            col,
        );
        // Same nudge as the type checker, for operands only known at runtime.
        if matches!(op, BinOp::Add) && matches!(v, Value::Str(_)) {
            err.hint("strings don't join with `+` — use interpolation `\"{a}{b}\"`, or `xs.join(sep)` for a list of strings.")
        } else {
            err
        }
    })
}

pub(crate) fn values_equal(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => (*a as f64) == *b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Dna(a), Value::Dna(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Rational(a), Value::Rational(b)) => a == b,
        (Value::Rational(a), Value::Int(b)) | (Value::Int(b), Value::Rational(a)) => {
            **a == BigRational::from_integer((*b).into())
        }
        (Value::Rational(a), Value::Float(b)) | (Value::Float(b), Value::Rational(a)) => {
            a.to_f64() == Some(*b)
        }
        (Value::Array(a), Value::Array(b)) => {
            use crate::value::ArrayData::{Floats, Ints};
            match (&**a, &**b) {
                // Two packed columns of the same kind compare as native slices — no
                // per-element boxing (`Vec<i64>`/`Vec<f64>` `PartialEq`; `NaN != NaN`
                // preserved, matching the per-element rule).
                (Ints(xa), Ints(xb)) => xa == xb,
                (Floats(fa), Floats(fb)) => fa == fb,
                // Mixed (incl. whole-array Int-vs-Float) / `Values` keep the general
                // per-element path so cross-type collapse (`1 == 1.0`) stays exact.
                _ => {
                    a.len() == b.len()
                        && a.to_values()
                            .iter()
                            .zip(b.to_values().iter())
                            .all(|(x, y)| values_equal(x, y))
                }
            }
        }
        // Tuples compare structurally, element by element (`(a, b) == (a, b)`).
        (Value::Tuple(a), Value::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| values_equal(x, y))
        }
        // Records compare by field → value, independent of field order (a record is its
        // mapping). No duplicate keys, so equal length + every field matching is exact.
        (Value::Record(a), Value::Record(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.iter().any(|(k2, v2)| k == k2 && values_equal(v, v2)))
        }
        // A dict is its key→value mapping: equal iff same keys with structurally-equal
        // values. Keys compare exactly (`DictKey: Eq`); values recurse through `values_equal`.
        (Value::Dict(a), Value::Dict(b)) => {
            a.len() == b.len()
                && a.iter().all(|(k, v)| b.get(k).is_some_and(|v2| values_equal(v, v2)))
        }
        _ => false,
    }
}

fn compare(op: &BinOp, l: &Value, r: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let ord = match (l, r) {
        (Value::Str(a), Value::Str(b)) => a.cmp(b),
        // DNA orders lexicographically like a string — enables canonical-k-mer /
        // sorting-by-sequence code (`dna(a) < dna(b)`).
        (Value::Dna(a), Value::Dna(b)) => a.cmp(b),
        // Compare integers exactly as i64 — a prior `as f64` cast lost precision
        // above 2^53 and disagreed with the JIT. Now all engines agree.
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        _ => {
            let a = num_operand(op, l, line, col)?;
            let b = num_operand(op, r, line, col)?;
            a.partial_cmp(&b).ok_or_else(|| {
                HelixError::new("cannot compare these values (NaN?)", line, col).hint(
                    "a NaN has no order — guard it first with `is_nan(x)` / `is_finite(x)`.",
                )
            })?
        }
    };
    use std::cmp::Ordering::*;
    let res = match op {
        BinOp::Lt => ord == Less,
        BinOp::Gt => ord == Greater,
        BinOp::Le => ord != Greater,
        BinOp::Ge => ord != Less,
        _ => unreachable!(),
    };
    Ok(Value::Bool(res))
}

/// An `Int` or `Rational` as a `BigRational`; `None` for anything else.
fn to_rational(v: &Value) -> Option<BigRational> {
    match v {
        Value::Int(i) => Some(BigRational::from_integer((*i).into())),
        Value::Rational(r) => Some((**r).clone()),
        _ => None,
    }
}

/// The f64 value of an `Int`/`Float`/`Rational` (for mixing rationals with floats).
fn to_f64_any(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Rational(r) => r.to_f64(),
        _ => None,
    }
}

/// `(a/b)^exp` exactly for an integer exponent. Caller guards `exp` magnitude and
/// the `0^negative` case.
fn ratio_pow(base: &BigRational, exp: i64) -> BigRational {
    let n = exp.unsigned_abs() as usize;
    let p = num_traits::pow(base.clone(), n);
    if exp < 0 {
        p.recip()
    } else {
        p
    }
}

/// f64 result of a binary op (used when a rational is mixed with a float).
fn float_binary_result(op: &BinOp, a: f64, b: f64, line: usize, col: usize) -> Result<Value, HelixError> {
    use BinOp::*;
    Ok(match op {
        Add => Value::Float(a + b),
        Sub => Value::Float(a - b),
        Mul => Value::Float(a * b),
        Div => {
            if b == 0.0 {
                return Err(HelixError::new("division by zero", line, col));
            }
            Value::Float(a / b)
        }
        FloorDiv => {
            if b == 0.0 {
                return Err(HelixError::new("division by zero", line, col));
            }
            Value::Float(a.div_euclid(b))
        }
        Mod => Value::Float(a.rem_euclid(b)),
        Pow => Value::Float(a.powf(b)),
        Eq => Value::Bool(a == b),
        Ne => Value::Bool(a != b),
        Lt => Value::Bool(a < b),
        Gt => Value::Bool(a > b),
        Le => Value::Bool(a <= b),
        Ge => Value::Bool(a >= b),
        BitAnd | BitOr | BitXor | Shl | Shr => {
            return Err(HelixError::new("bitwise operators need integers", line, col));
        }
        And | Or | Coalesce => unreachable!("short-circuit ops never reach here"),
    })
}

/// Exact rational arithmetic/comparison. With a float operand it drops to f64
/// (exactness already lost); two Int/Rational operands stay exact.
pub(crate) fn rational_binary(op: &BinOp, l: &Value, r: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    use BinOp::*;
    if matches!(l, Value::Float(_)) || matches!(r, Value::Float(_)) {
        match (to_f64_any(l), to_f64_any(r)) {
            (Some(a), Some(b)) => return float_binary_result(op, a, b, line, col),
            _ => {
                return Err(HelixError::new(
                    format!("`{}` needs numbers", op.symbol()),
                    line,
                    col,
                ))
            }
        }
    }
    let (a, b) = match (to_rational(l), to_rational(r)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(HelixError::new(
                format!("`{}` needs numbers", op.symbol()),
                line,
                col,
            ))
        }
    };
    Ok(match op {
        Add => Value::Rational(Rc::new(a + b)),
        Sub => Value::Rational(Rc::new(a - b)),
        Mul => Value::Rational(Rc::new(a * b)),
        Div => {
            if b.is_zero() {
                return Err(HelixError::new("division by zero", line, col));
            }
            Value::Rational(Rc::new(a / b))
        }
        FloorDiv => {
            if b.is_zero() {
                return Err(HelixError::new("integer division by zero", line, col));
            }
            Value::Rational(Rc::new((a / b).floor()))
        }
        Mod => {
            // Rational modulo is unusual; it's computed in f64 via rem_euclid. A rational
            // too large to represent in f64 would silently become NaN — error instead of
            // returning a lie.
            match (a.to_f64(), b.to_f64()) {
                (Some(af), Some(bf)) => Value::Float(af.rem_euclid(bf)),
                _ => {
                    return Err(HelixError::new(
                        "rational is too large to take a modulo (exceeds f64 range)",
                        line,
                        col,
                    ))
                }
            }
        }
        Pow => {
            if b.is_integer() {
                let exp = b.to_i64().filter(|e| e.unsigned_abs() <= 4096).ok_or_else(|| {
                    HelixError::new("rational exponent is out of range (max +/-4096)", line, col)
                })?;
                if exp < 0 && a.is_zero() {
                    return Err(HelixError::new("0 cannot be raised to a negative power", line, col));
                }
                Value::Rational(Rc::new(ratio_pow(&a, exp)))
            } else {
                match (a.to_f64(), b.to_f64()) {
                    (Some(af), Some(bf)) => Value::Float(af.powf(bf)),
                    _ => {
                        return Err(HelixError::new(
                            "rational is too large to raise to a fractional power (exceeds f64 range)",
                            line,
                            col,
                        ))
                    }
                }
            }
        }
        Eq => Value::Bool(a == b),
        Ne => Value::Bool(a != b),
        Lt => Value::Bool(a < b),
        Gt => Value::Bool(a > b),
        Le => Value::Bool(a <= b),
        Ge => Value::Bool(a >= b),
        BitAnd | BitOr | BitXor | Shl | Shr => {
            return Err(HelixError::new("bitwise operators need integers", line, col));
        }
        And | Or | Coalesce => unreachable!("short-circuit ops never reach here"),
    })
}

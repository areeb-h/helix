//! Binary-operator evaluation (`eval_binary`) and its scalar helpers
//! (`arith`/`compare`/`num_operand`/`values_equal`), shared by the tree-walker
//! and the VM's `binary()` fast path. Includes the array/tensor broadcasting and
//! `missing` propagation rules (ADR-0001).

use std::rc::Rc;

use crate::ast::BinOp;
use crate::error::HelixError;
use crate::tensor;
use crate::value::Value;

pub(crate) fn eval_binary(
    op: &BinOp,
    l: Value,
    r: Value,
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

    // Elementwise broadcasting for arithmetic: array⊕scalar, scalar⊕array, and
    // array⊕array (same length). Comparison/equality deliberately do NOT
    // broadcast — `==` is whole-value, avoiding NumPy's "ambiguous truth value"
    // trap; use `.map`/`.where` for elementwise predicates.
    if matches!(op, Add | Sub | Mul | Div | Mod | Pow) {
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
                for (x, y) in a.iter().zip(b.iter()) {
                    out.push(eval_binary(op, x.clone(), y.clone(), line, col)?);
                }
                return Ok(Value::Array(Rc::new(out)));
            }
            (Value::Array(a), scalar) => {
                let mut out = Vec::with_capacity(a.len());
                for x in a.iter() {
                    out.push(eval_binary(op, x.clone(), scalar.clone(), line, col)?);
                }
                return Ok(Value::Array(Rc::new(out)));
            }
            (scalar, Value::Array(b)) => {
                let mut out = Vec::with_capacity(b.len());
                for y in b.iter() {
                    out.push(eval_binary(op, scalar.clone(), y.clone(), line, col)?);
                }
                return Ok(Value::Array(Rc::new(out)));
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

    match op {
        Add | Sub | Mul => arith(op, &l, &r, line, col),
        Div => {
            let a = num_operand(op, &l, line, col)?;
            let b = num_operand(op, &r, line, col)?;
            if b == 0.0 {
                return Err(HelixError::new("division by zero", line, col)
                    .hint("guard the denominator, e.g. `if d != 0` (coming soon) or check your data."));
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
                Ok(Value::Float(a.powf(b)))
            }
        },
        Eq => Ok(Value::Bool(values_equal(&l, &r))),
        Ne => Ok(Value::Bool(!values_equal(&l, &r))),
        Lt | Gt | Le | Ge => compare(op, &l, &r, line, col),
        And | Or | Coalesce => unreachable!("handled with short-circuit in eval"),
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
        HelixError::new(
            format!(
                "operator `{}` needs numbers, but got a {}",
                op.symbol(),
                v.type_name()
            ),
            line,
            col,
        )
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
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| values_equal(x, y))
        }
        _ => false,
    }
}

fn compare(op: &BinOp, l: &Value, r: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let ord = match (l, r) {
        (Value::Str(a), Value::Str(b)) => a.cmp(b),
        // Compare integers exactly as i64 — a prior `as f64` cast lost precision
        // above 2^53 and disagreed with the JIT. Now all engines agree.
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        _ => {
            let a = num_operand(op, l, line, col)?;
            let b = num_operand(op, r, line, col)?;
            a.partial_cmp(&b).ok_or_else(|| {
                HelixError::new("cannot compare these values (NaN?)", line, col)
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

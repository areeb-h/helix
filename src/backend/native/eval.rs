//! Column-expression evaluation — **through the interpreter's own scalar kernel**
//! (ADR 0034's principle made literal): every cell-level operation calls
//! `interp::ops::eval_binary` / `interp::eval_unary`, so a column expression
//! cannot mean anything different from the same expression on scalars. A typed
//! fast path may shadow this later (ADR 0033 Stage 3) behind the same
//! differential tests; correctness stays defined HERE.

use crate::ast::{BinOp, UnOp};
use crate::backend::ColExpr;
use crate::error::HelixError;
use crate::value::Value;

use super::NativeFrame;

/// An evaluated (sub)expression: one value for every row, or a scalar that
/// broadcasts. Keeping scalars unexpanded makes `x + 1` one kernel call per row,
/// not a materialized constant column.
pub enum Evaled {
    Scalar(Value),
    Rows(Vec<Value>),
}

impl Evaled {
    pub fn into_rows(self, n: usize) -> Vec<Value> {
        match self {
            Evaled::Rows(r) => r,
            Evaled::Scalar(v) => vec![v; n],
        }
    }
}

/// Evaluate `expr` over the frame's rows.
pub fn eval(frame: &NativeFrame, expr: &ColExpr, line: usize, col: usize) -> Result<Evaled, HelixError> {
    match expr {
        ColExpr::Lit(v) => Ok(Evaled::Scalar(v.clone())),
        ColExpr::Col(name) => {
            let c = frame.col(name, line, col)?;
            Ok(Evaled::Rows((0..c.len()).map(|i| c.get(i)).collect()))
        }
        ColExpr::Unary(op, inner) => {
            let inner = eval(frame, inner, line, col)?;
            unary(op, inner, line, col)
        }
        ColExpr::Binary(op, l, r) => {
            let l = eval(frame, l, line, col)?;
            let r = eval(frame, r, line, col)?;
            binary(op, l, r, line, col)
        }
        // Classification, never a comparison: this is the ONE float question that
        // must stay answerable ON a NaN, since it is what the compare error tells
        // people to reach for (ADR 0036 policy 5).
        ColExpr::FloatPred(kind, inner) => {
            let inner = eval(frame, inner, line, col)?;
            let ask = |v: &Value| -> Value {
                // `missing` PROPAGATES (ADR 0001) — `missing.is_nan()` is `missing`,
                // not `false`, exactly as it is on scalars. `is_missing` is the one
                // operation that looks AT absence instead of propagating it; this is
                // not that operation. Answering `false` here would also quietly claim
                // "this is a number, and it is fine".
                if matches!(v, Value::Missing) {
                    return Value::Missing;
                }
                let f = v.as_f64();
                Value::Bool(match (kind, f) {
                    (crate::backend::FloatPredKind::IsNan, Some(x)) => x.is_nan(),
                    (crate::backend::FloatPredKind::IsFinite, Some(x)) => x.is_finite(),
                    // A non-number is neither NaN nor finite.
                    (_, None) => false,
                })
            };
            Ok(match inner {
                Evaled::Scalar(v) => Evaled::Scalar(ask(&v)),
                Evaled::Rows(rows) => Evaled::Rows(rows.iter().map(ask).collect()),
            })
        }
        ColExpr::IsMissing(inner) => {
            // `is_missing` answers Bool for every input — the one operation that
            // looks AT missing instead of propagating it (ADR 0001).
            let inner = eval(frame, inner, line, col)?;
            Ok(match inner {
                Evaled::Scalar(v) => Evaled::Scalar(Value::Bool(matches!(v, Value::Missing))),
                Evaled::Rows(rows) => Evaled::Rows(
                    rows.into_iter().map(|v| Value::Bool(matches!(v, Value::Missing))).collect(),
                ),
            })
        }
    }
}

fn unary(op: &UnOp, v: Evaled, line: usize, col: usize) -> Result<Evaled, HelixError> {
    match v {
        Evaled::Scalar(v) => Ok(Evaled::Scalar(crate::interp::eval_unary(op, v, line, col)?)),
        Evaled::Rows(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for (i, v) in rows.into_iter().enumerate() {
                out.push(crate::interp::eval_unary(op, v, line, col).map_err(|e| at_row(e, i))?);
            }
            Ok(Evaled::Rows(out))
        }
    }
}

fn binary(op: &BinOp, l: Evaled, r: Evaled, line: usize, col: usize) -> Result<Evaled, HelixError> {
    // `and`/`or`/`??` short-circuit on scalars, so the kernel refuses them; a
    // column has no evaluation order to cut short — `logic.rs` carries their
    // oracle-pinned elementwise truth tables.
    let one = |a: Value, b: Value| match op {
        BinOp::And => super::logic::and(&a, &b, line, col),
        BinOp::Or => super::logic::or(&a, &b, line, col),
        BinOp::Coalesce => Ok(super::logic::coalesce(&a, &b)),
        _ => crate::interp::ops::eval_binary(op, a, b, line, col),
    };
    match (l, r) {
        (Evaled::Scalar(a), Evaled::Scalar(b)) => Ok(Evaled::Scalar(one(a, b)?)),
        (Evaled::Rows(rows), Evaled::Scalar(b)) => {
            let mut out = Vec::with_capacity(rows.len());
            for (i, a) in rows.into_iter().enumerate() {
                out.push(one(a, b.clone()).map_err(|e| at_row(e, i))?);
            }
            Ok(Evaled::Rows(out))
        }
        (Evaled::Scalar(a), Evaled::Rows(rows)) => {
            let mut out = Vec::with_capacity(rows.len());
            for (i, b) in rows.into_iter().enumerate() {
                out.push(one(a.clone(), b).map_err(|e| at_row(e, i))?);
            }
            Ok(Evaled::Rows(out))
        }
        (Evaled::Rows(la), Evaled::Rows(rb)) => {
            debug_assert_eq!(la.len(), rb.len(), "frame columns share one length");
            let mut out = Vec::with_capacity(la.len());
            for (i, (a, b)) in la.into_iter().zip(rb).enumerate() {
                out.push(one(a, b).map_err(|e| at_row(e, i))?);
            }
            Ok(Evaled::Rows(out))
        }
    }
}

/// A cell-level error (division by zero, a type mismatch) names the row it
/// happened on — the frame-sized counterpart of a position.
fn at_row(e: HelixError, i: usize) -> HelixError {
    e.hint(format!("at row {i} of the frame."))
}

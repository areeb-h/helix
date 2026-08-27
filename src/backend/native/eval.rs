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
        // A String test down a column — the mirror of the polars `str_udf`, and like it
        // routed through `call_method` so the two cannot mean different things.
        ColExpr::StrMethod(f, recv, args) => {
            // The type question from the COLUMN'S KIND when the receiver is a bare
            // column: the same schema decision polars makes, which is what keeps the two
            // agreeing on an EMPTY frame — there is no row there to learn from, and the
            // column is still the wrong type.
            let schema_probe = match &**recv {
                ColExpr::Col(name) => col_probe(frame.col(name, line, col)?),
                _ => None,
            };
            if let Some(p) = schema_probe {
                crate::backend::probe_str_call(&p, *f, args, line, col)?;
            }
            let evaled = eval(frame, recv, line, col)?;
            // Anything else has no schema entry, so its first non-missing value answers —
            // the same fallback, and the same reasoning, as `refuse_non_numeric`.
            let value_probe = match &**recv {
                // A bare column already answered from its kind, above.
                ColExpr::Col(_) => None,
                _ => representative(&evaled).filter(|v| !matches!(v, Value::Str(_))),
            };
            if let Some(v) = value_probe {
                crate::backend::probe_str_call(v, *f, args, line, col)?;
            }
            let ask = |v: &Value| -> Result<Value, HelixError> {
                // `missing` PROPAGATES (ADR 0001), exactly as it does for `FloatPred`
                // above and for `missing.starts_with("h")` on a scalar. Answering
                // `false` would also quietly claim "this is text, and it does not match".
                if matches!(v, Value::Missing) {
                    return Ok(Value::Missing);
                }
                crate::interp::call_method(v, f.spelling(), args, line, col)
            };
            Ok(match evaled {
                Evaled::Scalar(v) => Evaled::Scalar(ask(&v)?),
                Evaled::Rows(rows) => {
                    let mut out = Vec::with_capacity(rows.len());
                    for (i, v) in rows.iter().enumerate() {
                        // A cell of an unexpected type keeps its row: at that point the
                        // row genuinely IS the new information.
                        out.push(ask(v).map_err(|e| at_row(e, i))?);
                    }
                    Evaled::Rows(out)
                }
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

/// A stand-in Helix value of the type this column holds, for `probe_str_call`.
///
/// `None` for a String column (the case that is fine) and for an all-missing one, whose
/// dtype is unknowable — polars answers `None` for its `Null` dtype for the same reason,
/// so the two agree without either knowing about the other.
fn col_probe(c: &super::columns::Col) -> Option<Value> {
    use super::columns::Col;
    match c {
        Col::I64 { .. } => Some(Value::Int(0)),
        Col::F64 { .. } => Some(Value::Float(0.0)),
        Col::Bool { .. } => Some(Value::Bool(false)),
        Col::Str { .. } | Col::Null { .. } => None,
    }
}

/// The first value that is not `missing`, which decides a column's type: a frame
/// column is homogeneous, so one non-missing cell answers for all of them.
fn representative(e: &Evaled) -> Option<&Value> {
    match e {
        Evaled::Scalar(v) => (!matches!(v, Value::Missing)).then_some(v),
        Evaled::Rows(rows) => rows.iter().find(|v| !matches!(v, Value::Missing)),
    }
}

/// Refuse arithmetic on a non-numeric operand BEFORE the row loop, so the error names
/// no row.
///
/// The row suffix is right for a **cell** error — `division by zero` happened at row 7
/// because row 7 holds a zero, and both backends say so. A **type** error is not a cell
/// error: every row fails identically, the column is what is wrong, and "at row 0"
/// invites you to go and look at a row whose data is blameless. The polars backend
/// decides this from the schema and names no row (`non_numeric_operand`); deciding it
/// here from the first non-missing value is the same conclusion by the same reasoning,
/// and it is what keeps the two backends byte-identical on the message.
///
/// An all-missing column has no representative, so it falls through to the per-row
/// path — where `missing` propagates rather than failing (ADR 0001), which is correct.
/// A later cell of a different type also falls through, and is still caught per row
/// WITH its row, because at that point the row genuinely is the new information.
fn refuse_non_numeric(op: &BinOp, l: &Evaled, r: &Evaled, line: usize, col: usize) -> Result<(), HelixError> {
    if !matches!(
        op,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::FloorDiv | BinOp::Pow
    ) {
        return Ok(());
    }
    for side in [l, r] {
        if let Some(v) = representative(side).filter(|v| v.as_f64().is_none()) {
            {
                // Ask the scalar kernel for the error rather than reproducing it: one
                // semantics means one message, and the kernel owns the wording and the
                // `+`-on-a-String nudge.
                return match crate::interp::ops::eval_binary(op, v.clone(), v.clone(), line, col) {
                    Err(e) => Err(e),
                    // Unreachable for a non-numeric operand, but a total runtime never
                    // assumes: if the kernel accepts it, so does the frame.
                    Ok(_) => Ok(()),
                };
            }
        }
    }
    Ok(())
}

fn binary(op: &BinOp, l: Evaled, r: Evaled, line: usize, col: usize) -> Result<Evaled, HelixError> {
    refuse_non_numeric(op, &l, &r, line, col)?;
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
///
/// The row ADDS to whatever the scalar kernel already advised; it used to replace it.
/// That mattered as soon as comparisons started raising: the compare error's whole
/// value is the sentence "a NaN has no order — guard it first with `is_nan(x)`", and
/// dropping it in the frame case removed the advice from exactly the place a user is
/// most likely to hit the error and least likely to know the fix.
pub(super) fn at_row(e: HelixError, i: usize) -> HelixError {
    let row = format!("at row {i} of the frame.");
    match &e.hint {
        Some(h) if !h.is_empty() => {
            let combined = format!("{h} ({row})");
            e.hint(combined)
        }
        _ => e.hint(row),
    }
}

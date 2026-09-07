//! Constant control flow (ADR 0051). Once the fold, or a specialization, has made the
//! deciding operand of an `if`, an `and`, an `or`, a `??` or a `not` a literal, the branch
//! it selects replaces the whole expression — exactly and only where the language's own
//! rule already decides the answer without the other operand: `true or x` never looks at
//! `x`, `false and x` never does, `missing ?? x` is `x`, a present literal `?? x` is the
//! literal. `true and x` is NOT `x` for every `x` — the walker's three-valued `and` errors
//! on a non-boolean right side where the bare `x` would not — so that one is rewritten only
//! when `x` is itself a literal the rule can settle.

use crate::ast::{BinOp, Expr, UnOp};

/// Simplify `e` in place where a literal decides it; `true` when something changed.
pub(crate) fn constant(e: &mut Expr) -> bool {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => match **cond {
            Expr::Bool(true) => {
                let taken = std::mem::replace(&mut **then_branch, Expr::Missing);
                *e = taken;
                true
            }
            Expr::Bool(false) => {
                let taken = std::mem::replace(&mut **else_branch, Expr::Missing);
                *e = taken;
                true
            }
            _ => false,
        },
        Expr::Binary { op: BinOp::And, left, right, .. } => match (&**left, &**right) {
            (Expr::Bool(false), _) => {
                *e = Expr::Bool(false);
                true
            }
            (Expr::Bool(true), Expr::Bool(b)) => {
                *e = Expr::Bool(*b);
                true
            }
            (Expr::Bool(true), Expr::Missing) => {
                *e = Expr::Missing;
                true
            }
            _ => false,
        },
        Expr::Binary { op: BinOp::Or, left, right, .. } => match (&**left, &**right) {
            (Expr::Bool(true), _) => {
                *e = Expr::Bool(true);
                true
            }
            (Expr::Bool(false), Expr::Bool(b)) => {
                *e = Expr::Bool(*b);
                true
            }
            (Expr::Bool(false), Expr::Missing) => {
                *e = Expr::Missing;
                true
            }
            _ => false,
        },
        Expr::Binary { op: BinOp::Coalesce, left, right, .. } => {
            if matches!(**left, Expr::Missing) {
                let taken = std::mem::replace(&mut **right, Expr::Missing);
                *e = taken;
                true
            } else if is_present_literal(left) {
                let taken = std::mem::replace(&mut **left, Expr::Missing);
                *e = taken;
                true
            } else {
                false
            }
        }
        Expr::Unary { op: UnOp::Not, expr, .. } => match **expr {
            Expr::Bool(b) => {
                *e = Expr::Bool(!b);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// A literal that is never `missing`: a number, a string, a boolean, or an array, tuple or
/// record literal (whatever they hold, the container itself is present).
pub(crate) fn is_present_literal(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Array(_) | Expr::Tuple(_) | Expr::Record(_)
    )
}

/// A literal in the sense the fold writes back: a scalar, `missing`, or an array, tuple or
/// record of literals.
pub(crate) fn is_literal(e: &Expr) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => true,
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().all(is_literal),
        Expr::Record(fields) => fields.iter().all(|(_, v)| is_literal(v)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(src: &str) -> Expr {
        let toks = crate::lexer::lex(src).unwrap_or_else(|e| panic!("{}", e.message));
        let stmts = crate::parser::parse(toks).unwrap_or_else(|e| panic!("{}", e.message));
        match stmts.into_iter().next() {
            Some(crate::ast::Stmt::Expr(e)) => e,
            other => panic!("not an expression statement: {other:?}"),
        }
    }

    /// Each rule rewrites exactly the decided shapes and leaves the undecided ones.
    #[test]
    fn a_literal_decides_only_what_the_language_decides_without_the_other_operand() {
        for (src, want) in [
            ("if true then 1 else 2", "1"),
            ("if false then 1 else 2", "2"),
            ("true or x", "true"),
            ("false and x", "false"),
            ("false or true", "true"),
            ("true and false", "false"),
            ("missing ?? 3", "3"),
            ("4 ?? x", "4"),
            ("not true", "false"),
        ] {
            let mut e = parsed(src);
            assert!(constant(&mut e), "{src}");
            let w = parsed(want);
            assert_eq!(format!("{e:?}"), format!("{w:?}"), "{src}");
        }
        for src in ["true and x", "false or x", "if c then 1 else 2", "x ?? 3", "not x", "missing and x"] {
            let mut e = parsed(src);
            assert!(!constant(&mut e), "{src} must stay");
        }
    }
}

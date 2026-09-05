//! The AST walker: one exhaustive preorder traversal for every pass that
//! inspects programs (the lints today; any future analysis). The point is the
//! exhaustive match — adding an `Expr` variant fails compilation HERE, and
//! every consumer inherits the fix, instead of each pass hand-rolling its own
//! 25-arm walk and silently missing the new node.

use crate::ast::{Expr, InterpPart, Stmt};

/// Call `f` on `e` and every expression beneath it, preorder.
pub fn walk_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match e {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Missing
        | Expr::Ident { .. }
        | Expr::Column { .. } => {}
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(inner, _) = p {
                    walk_expr(inner, f);
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                walk_expr(x, f);
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                walk_expr(v, f);
            }
        }
        Expr::RecordUpdate { base, fields, .. } => {
            walk_expr(base, f);
            for (_, v) in fields {
                walk_expr(v, f);
            }
        }
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } => walk_expr(recv, f),
        Expr::Unary { expr, .. } | Expr::Try { expr, .. } => walk_expr(expr, f),
        Expr::Binary { left, right, .. } => {
            walk_expr(left, f);
            walk_expr(right, f);
        }
        Expr::Call { args, .. } => {
            for a in args {
                walk_expr(a, f);
            }
        }
        Expr::Method { recv, args, named, .. } => {
            walk_expr(recv, f);
            for a in args {
                walk_expr(a, f);
            }
            for (_, v) in named {
                walk_expr(v, f);
            }
        }
        Expr::CallValue { callee, args, .. } => {
            walk_expr(callee, f);
            for a in args {
                walk_expr(a, f);
            }
        }
        Expr::Index { recv, index, .. } => {
            walk_expr(recv, f);
            walk_expr(index, f);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            walk_expr(recv, f);
            for o in [start, stop, step].into_iter().flatten() {
                walk_expr(o, f);
            }
        }
        Expr::Lambda { defaults, bound, body, .. } => {
            // The defaults and the origin of a synthesized bound-function lambda are
            // expressions of the enclosing scope — a frame predicate's captures
            // (`column_arg_captures`) are collected through this walk, and `df.where(flag)`
            // inside a closure names `flag` only through the origin.
            for d in defaults {
                walk_expr(d, f);
            }
            if let Some(origin) = bound {
                walk_expr(origin, f);
            }
            walk_expr(body, f);
        }
        Expr::Let { bindings, body, .. } => {
            for (_, v) in bindings {
                walk_expr(v, f);
            }
            walk_expr(body, f);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            walk_expr(cond, f);
            walk_expr(then_branch, f);
            walk_expr(else_branch, f);
        }
        Expr::Match { scrutinee, arms, .. } => {
            walk_expr(scrutinee, f);
            for a in arms {
                if let Some(g) = &a.guard {
                    walk_expr(g, f);
                }
                walk_expr(&a.body, f);
            }
        }
    }
}

/// Call `f` on every expression a statement holds (and everything beneath).
pub fn walk_stmt(s: &Stmt, f: &mut impl FnMut(&Expr)) {
    match s {
        Stmt::Assign { value, .. } | Stmt::Destructure { value, .. } | Stmt::Expr(value) => {
            walk_expr(value, f)
        }
        Stmt::Func { defaults, body, .. } => {
            for d in defaults.iter().flatten() {
                walk_expr(d, f);
            }
            walk_expr(body, f);
        }
        Stmt::Import { .. } => {}
    }
}

/// Best-effort source position of an expression — most variants carry one;
/// literals do not, so a container falls back to the first positioned child.
pub fn expr_pos(e: &Expr) -> Option<(usize, usize)> {
    let mut found = None;
    walk_expr(e, &mut |x| {
        if found.is_some() {
            return;
        }
        found = match x {
            Expr::Ident { line, col, .. }
            | Expr::Column { line, col, .. }
            | Expr::RecordUpdate { line, col, .. }
            | Expr::Field { line, col, .. }
            | Expr::FieldOrMissing { line, col, .. }
            | Expr::Unary { line, col, .. }
            | Expr::Binary { line, col, .. }
            | Expr::Call { line, col, .. }
            | Expr::Method { line, col, .. }
            | Expr::CallValue { line, col, .. }
            | Expr::Index { line, col, .. }
            | Expr::Slice { line, col, .. }
            | Expr::If { line, col, .. }
            | Expr::Try { line, col, .. }
            | Expr::Match { line, col, .. } => Some((*line, *col)),
            _ => None,
        };
    });
    found
}

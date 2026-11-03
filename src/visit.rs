//! The AST walker: one exhaustive preorder traversal for every pass that
//! inspects programs (the lints today; any future analysis). The point is the
//! exhaustive match — adding an `Expr` variant fails compilation HERE, and
//! every consumer inherits the fix, instead of each pass hand-rolling its own
//! 25-arm walk and silently missing the new node.

use crate::ast::{Expr, InterpPart, Stmt};

/// Call `f` on `e` and every expression beneath it, preorder.
pub fn walk_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    for_each_child(e, &mut |c| walk_expr(c, f));
}

/// Call `f` on each DIRECT child of `e`, in evaluation order — the one exhaustive match
/// every walk goes through. A pass that has to treat a node's children differently (a
/// branch counted as "either", not "both") recurses through this instead of `walk_expr`.
pub fn for_each_child(e: &Expr, f: &mut impl FnMut(&Expr)) {
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
                    f(inner);
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                f(x);
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                f(v);
            }
        }
        Expr::RecordUpdate { parts, .. } => {
            for p in parts {
                f(p.expr());
            }
        }
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } | Expr::Part { recv, .. } => f(recv),
        Expr::Unary { expr, .. } | Expr::Try { expr, .. } => f(expr),
        Expr::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        Expr::Call { args, .. } => {
            for a in args {
                f(a);
            }
        }
        Expr::Method { recv, args, named, .. } => {
            f(recv);
            for a in args {
                f(a);
            }
            for (_, v) in named {
                f(v);
            }
        }
        Expr::CallValue { callee, args, .. } => {
            f(callee);
            for a in args {
                f(a);
            }
        }
        Expr::Index { recv, index, .. } => {
            f(recv);
            f(index);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            f(recv);
            for o in [start, stop, step].into_iter().flatten() {
                f(o);
            }
        }
        Expr::Lambda { defaults, bound, body, .. } => {
            // The defaults and the origin of a synthesized bound-function lambda are
            // expressions of the enclosing scope — a frame predicate's captures
            // (`column_arg_captures`) are collected through this walk, and `df.where(flag)`
            // inside a closure names `flag` only through the origin.
            for d in defaults {
                f(d);
            }
            if let Some(origin) = bound {
                f(origin);
            }
            f(body);
        }
        Expr::Let { bindings, body, .. } => {
            for (_, v) in bindings {
                f(v);
            }
            f(body);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            f(cond);
            f(then_branch);
            f(else_branch);
        }
        Expr::Match { scrutinee, arms, .. } => {
            f(scrutinee);
            for a in arms {
                if let Some(g) = &a.guard {
                    f(g);
                }
                f(&a.body);
            }
        }
    }
}

/// Call `f` on every expression a statement holds (and everything beneath).
pub fn walk_stmt(s: &Stmt, f: &mut impl FnMut(&Expr)) {
    match s {
        Stmt::Assign { value, .. } | Stmt::Expr(value) => walk_expr(value, f),
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
            | Expr::Part { line, col, .. }
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

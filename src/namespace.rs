//! Compile-time resolution of the native namespaces (`bio`, `stats`, `io`, `json`,
//! `http`). Domain builtins are registered under dotted paths (`bio.read_vcf`,
//! `stats.t_test`); a call `bio.read_vcf(path)` parses as a method on a bare `bio`
//! identifier, and this pass rewrites it into a direct `Call("bio.read_vcf", args)`
//! that rides the ordinary builtin path — the same compile-time strategy the module
//! loader uses to turn `alias.member(...)` into a direct call (`module::rw`).
//!
//! Namespaces are predefined and need no import, but they are **shadowable**: a local
//! binding named `stats` wins, exactly as a module alias does, so the names are only
//! reserved as receivers, never as values. A bare namespace (or a namespace field
//! without a call) is reported by the type checker, which recognizes these names.
//!
//! Dotted paths can never be user identifiers, so the rewritten name cannot collide
//! with anything the user wrote — the same guarantee the module loader's `$`-mangling
//! relies on.

use std::collections::HashSet;

use crate::ast::{Expr, InterpPart, Stmt};

/// The native namespace names, resolved at compile time.
pub const NAMESPACES: &[&str] = &["bio", "stats", "io", "json", "http"];

/// Whether `name` is a native namespace.
pub fn is_namespace(name: &str) -> bool {
    NAMESPACES.contains(&name)
}

/// The dotted path a now-retired flat builtin name moved to, for a "did you mean?"
/// hint that eases the rename (e.g. `read_vcf` -> `bio.read_vcf`).
pub fn retired_path(name: &str) -> Option<&'static str> {
    Some(match name {
        "read_csv" => "io.read_csv",
        "read_parquet" => "io.read_parquet",
        "read_fasta" => "bio.read_fasta",
        "read_fastq" => "bio.read_fastq",
        "read_vcf" => "bio.read_vcf",
        "write_parquet" => "io.write_parquet",
        "http_get" => "http.get",
        "correlation" => "stats.correlation",
        "t_test" => "stats.t_test",
        "linear_regression" => "stats.linear_regression",
        "multiple_regression" => "stats.multiple_regression",
        "normal_cdf" => "stats.normal_cdf",
        "normal_pdf" => "stats.normal_pdf",
        "parse_json" => "json.parse",
        "to_json" => "json.stringify",
        _ => return None,
    })
}

/// Rewrite namespaced calls (`bio.read_vcf(args)`) into direct builtin calls across the
/// whole program. Runs after module loading, on the flattened statement list.
pub fn resolve(stmts: &mut [Stmt]) {
    let bound = HashSet::new();
    for s in stmts.iter_mut() {
        rw_stmt(s, &bound);
    }
}

fn rw_stmt(s: &mut Stmt, bound: &HashSet<String>) {
    match s {
        Stmt::Func { params, body, .. } => {
            let mut b = bound.clone();
            b.extend(params.iter().map(|(p, _)| p.clone()));
            rw(body, &b);
        }
        Stmt::Assign { value, .. } | Stmt::Destructure { value, .. } => rw(value, bound),
        Stmt::Expr(e) => rw(e, bound),
        Stmt::Import { .. } => {}
    }
}

/// Rewrite an expression in place. `bound` is the set of local names in scope; a
/// namespace shadowed by a local is left alone.
fn rw(e: &mut Expr, bound: &HashSet<String>) {
    // `ns.member(args)` where `ns` is an unshadowed namespace → `Call("ns.member", args)`.
    if let Expr::Method { recv, name, args, line, col } = e
        && let Expr::Ident { name: ns, .. } = recv.as_ref()
        && is_namespace(ns)
        && !bound.contains(ns)
    {
        let path = format!("{ns}.{name}");
        for a in args.iter_mut() {
            rw(a, bound);
        }
        *e = Expr::Call { name: path, args: std::mem::take(args), line: *line, col: *col };
        return;
    }

    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => {}
        Expr::Ident { .. } => {}
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(e) = p {
                    rw(e, bound);
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                rw(x, bound);
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                rw(v, bound);
            }
        }
        Expr::Field { recv, .. } => rw(recv, bound),
        Expr::Unary { expr, .. } => rw(expr, bound),
        Expr::Binary { left, right, .. } => {
            rw(left, bound);
            rw(right, bound);
        }
        Expr::Call { args, .. } => {
            for a in args.iter_mut() {
                rw(a, bound);
            }
        }
        Expr::Method { recv, args, .. } => {
            rw(recv, bound);
            for a in args.iter_mut() {
                rw(a, bound);
            }
        }
        Expr::Index { recv, index, .. } => {
            rw(recv, bound);
            rw(index, bound);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            rw(recv, bound);
            for x in [start, stop, step].into_iter().flatten() {
                rw(x, bound);
            }
        }
        Expr::Lambda { params, body } => {
            let mut b = bound.clone();
            b.extend(params.iter().cloned());
            rw(body, &b);
        }
        Expr::Let { bindings, body } => {
            let mut b = bound.clone();
            for (n, v) in bindings.iter_mut() {
                rw(v, &b);
                b.insert(n.clone());
            }
            rw(body, &b);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            rw(cond, bound);
            rw(then_branch, bound);
            rw(else_branch, bound);
        }
        Expr::Try { expr, .. } => rw(expr, bound),
    }
}

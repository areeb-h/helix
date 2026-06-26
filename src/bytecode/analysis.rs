//! Static program analysis the compiler runs before lowering: `memoizable_fns`
//! (which functions are pure, mutable-global-free, and overlapping-recursive — so
//! safe to auto-memoize) and the AST-walking helpers it needs (`has_method`,
//! `any_call`, `count_self_calls`, `reads_mutable`, `children`).

use super::*;

/// A top-level function reduced to the parts the memoization analysis reads:
/// its name, parameter list, and body expression (all borrowed from `program`).
type FuncSig<'a> = (&'a str, &'a [(String, Option<crate::ast::TypeAnn>)], &'a Expr);

/// The names of functions that are **safe and worthwhile to memoize**:
///   * **pure** — never reaches `print`/`read_*`/`write_*` (transitively), so a
///     cache hit can't skip a side effect;
///   * **reads no mutable global** — so the result is a function of its arguments
///     alone (immutable globals like `pi` are fine; they never change);
///   * **overlapping recursion** — at least two self-calls in the body, the
///     signature of exponential redundancy (linear recursion stays on the JIT,
///     where one fast native call per step beats a cached bytecode step).
///
/// This is the static half of the automatic "under the hood" cache; the VM gates
/// on all-`Int` arguments at runtime (float keys are excluded — NaN/precision).
pub fn memoizable_fns(program: &[Stmt]) -> HashSet<String> {
    let funcs: Vec<FuncSig> = program
        .iter()
        .filter_map(|s| match s {
            Stmt::Func { name, params, body, .. } => Some((name.as_str(), params.as_slice(), body)),
            _ => None,
        })
        .collect();

    // Names of mutable top-level bindings.
    let mut mutable: HashSet<&str> = HashSet::new();
    for s in program {
        match s {
            Stmt::Assign { name, mutable: true, .. } => {
                mutable.insert(name.as_str());
            }
            Stmt::Destructure { names, mutable: true, .. } => {
                for n in names {
                    mutable.insert(n.as_str());
                }
            }
            _ => {}
        }
    }

    // Purity fixpoint: impure if it reaches an impure builtin, an impure user fn,
    // or *any method call* — methods are assumed potentially side-effecting
    // (fail-closed), so the analysis stays sound even as the VM widens to compile
    // method calls rather than relying on the compiler rejecting them today.
    let mut impure: HashSet<&str> = HashSet::new();
    loop {
        let mut changed = false;
        for &(name, _, body) in &funcs {
            if !impure.contains(name)
                && (has_method(body)
                    || any_call(body, &|n| {
                        crate::registry::is_impure_builtin(n) || impure.contains(n)
                    }))
            {
                impure.insert(name);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Reads-a-mutable-global, **transitively** (its own fixpoint, mirroring
    // purity): a function that reaches a mutable global through a callee is not a
    // function of its arguments alone, so it must not be memoized.
    let mut reads_mut: HashSet<&str> = HashSet::new();
    for &(name, params, body) in &funcs {
        let bound: HashSet<&str> = params.iter().map(|(p, _)| p.as_str()).collect();
        if reads_mutable(body, &bound, &mutable) {
            reads_mut.insert(name);
        }
    }
    loop {
        let snapshot = reads_mut.clone();
        let mut changed = false;
        for &(name, _, body) in &funcs {
            if !snapshot.contains(name) && any_call(body, &|n| snapshot.contains(n)) {
                reads_mut.insert(name);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut result = HashSet::new();
    for &(name, _, body) in &funcs {
        if !impure.contains(name)
            && !reads_mut.contains(name)
            && count_self_calls(body, name) >= 2
        {
            result.insert(name.to_string());
        }
    }
    result
}

/// True if any method call appears anywhere in the tree.
fn has_method(e: &Expr) -> bool {
    matches!(e, Expr::Method { .. }) || children(e).into_iter().any(has_method)
}

/// True if any free-function call in the tree satisfies `pred` on its name.
fn any_call(e: &Expr, pred: &dyn Fn(&str) -> bool) -> bool {
    match e {
        Expr::Call { name, args, .. } => {
            pred(name) || args.iter().any(|a| any_call(a, pred))
        }
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing
        | Expr::Ident { .. } | Expr::Column { .. } => false,
        Expr::Interp(parts) => parts
            .iter()
            .any(|p| matches!(p, InterpPart::Expr(e) if any_call(e, pred))),
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().any(|x| any_call(x, pred)),
        Expr::Record(fs) => fs.iter().any(|(_, v)| any_call(v, pred)),
        Expr::Field { recv, .. } => any_call(recv, pred),
        Expr::Unary { expr, .. } => any_call(expr, pred),
        Expr::Binary { left, right, .. } => any_call(left, pred) || any_call(right, pred),
        Expr::Method { recv, args, .. } => {
            any_call(recv, pred) || args.iter().any(|a| any_call(a, pred))
        }
        Expr::Index { recv, index, .. } => any_call(recv, pred) || any_call(index, pred),
        Expr::Slice { recv, start, stop, step, .. } => {
            any_call(recv, pred)
                || [start, stop, step]
                    .iter()
                    .any(|o| o.as_ref().is_some_and(|x| any_call(x, pred)))
        }
        Expr::Lambda { body, .. } => any_call(body, pred),
        Expr::Let { bindings, body } => {
            bindings.iter().any(|(_, v)| any_call(v, pred)) || any_call(body, pred)
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            any_call(cond, pred) || any_call(then_branch, pred) || any_call(else_branch, pred)
        }
        Expr::Try { expr, .. } => any_call(expr, pred),
        Expr::Match { scrutinee, arms, .. } => {
            any_call(scrutinee, pred)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(|g| any_call(g, pred)) || any_call(&a.body, pred)
                })
        }
    }
}

/// Count direct recursive calls to `name` in the body.
fn count_self_calls(e: &Expr, name: &str) -> usize {
    let mut n = 0;
    count_self_calls_into(e, name, &mut n);
    n
}

fn count_self_calls_into(e: &Expr, name: &str, n: &mut usize) {
    if let Expr::Call { name: callee, .. } = e
        && callee == name {
            *n += 1;
        }
    for child in children(e) {
        count_self_calls_into(child, name, n);
    }
}

/// True if the expression reads a mutable global that isn't shadowed by a
/// parameter or `let` binding (which would make it not a pure function of args).
fn reads_mutable(e: &Expr, bound: &HashSet<&str>, mutable: &HashSet<&str>) -> bool {
    match e {
        Expr::Ident { name, .. } => {
            mutable.contains(name.as_str()) && !bound.contains(name.as_str())
        }
        Expr::Lambda { params, body } => {
            let mut b = bound.clone();
            for p in params {
                b.insert(p.as_str());
            }
            reads_mutable(body, &b, mutable)
        }
        Expr::Let { bindings, body } => {
            let mut b = bound.clone();
            for (n, v) in bindings {
                if reads_mutable(v, &b, mutable) {
                    return true;
                }
                b.insert(n.as_str());
            }
            reads_mutable(body, &b, mutable)
        }
        _ => children(e).into_iter().any(|c| reads_mutable(c, bound, mutable)),
    }
}

/// The immediate child expressions of a node (for generic traversal). `Lambda`
/// and `Let` are handled by their callers because they introduce bindings.
fn children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing
        | Expr::Ident { .. } | Expr::Column { .. } => vec![],
        Expr::Interp(parts) => parts
            .iter()
            .filter_map(|p| match p {
                InterpPart::Expr(e) => Some(&**e),
                _ => None,
            })
            .collect(),
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().collect(),
        Expr::Record(fs) => fs.iter().map(|(_, v)| v).collect(),
        Expr::Field { recv, .. } => vec![recv],
        Expr::Unary { expr, .. } => vec![expr],
        Expr::Binary { left, right, .. } => vec![left, right],
        Expr::Call { args, .. } => args.iter().collect(),
        Expr::Method { recv, args, .. } => {
            let mut v = vec![&**recv];
            v.extend(args.iter());
            v
        }
        Expr::Index { recv, index, .. } => vec![recv, index],
        Expr::Slice { recv, start, stop, step, .. } => {
            let mut v = vec![&**recv];
            for o in [start, stop, step].into_iter().flatten() {
                v.push(o);
            }
            v
        }
        Expr::Lambda { body, .. } => vec![body],
        Expr::Let { bindings, body } => {
            let mut v: Vec<&Expr> = bindings.iter().map(|(_, e)| e).collect();
            v.push(body);
            v
        }
        Expr::If { cond, then_branch, else_branch, .. } => vec![cond, then_branch, else_branch],
        Expr::Try { expr, .. } => vec![expr],
        Expr::Match { scrutinee, arms, .. } => {
            let mut v: Vec<&Expr> = vec![scrutinee];
            for a in arms {
                if let Some(g) = &a.guard {
                    v.push(g);
                }
                v.push(&a.body);
            }
            v
        }
    }
}

/// The **free variables** of a lambda: identifier names referenced in `body` —
/// whether as a value (`Expr::Ident`) or as a call target (`Expr::Call` name) —
/// that are *not* bound by the lambda's `params` or by a `let`/nested-lambda
/// binding inside `body`. Each name appears once, in first-seen order.
///
/// This is the raw lexical analysis; the engines decide which of these to *capture*
/// (an enclosing local) versus resolve normally (a global, builtin, or top-level
/// function). Call targets are included so a captured **function**-valued local
/// (`fn compose(f, g) = (x => f(g(x)))`) is captured too.
pub(crate) fn free_names(params: &[String], body: &Expr) -> Vec<String> {
    let mut bound: Vec<&str> = params.iter().map(|s| s.as_str()).collect();
    let mut free: Vec<String> = Vec::new();
    collect_free(body, &mut bound, &mut free);
    free
}

fn note_free(name: &str, bound: &[&str], free: &mut Vec<String>) {
    if !bound.contains(&name) && !free.iter().any(|f| f == name) {
        free.push(name.to_string());
    }
}

fn collect_free<'a>(e: &'a Expr, bound: &mut Vec<&'a str>, free: &mut Vec<String>) {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => {}
        // A `@column` reference names a frame column, not a variable — it never
        // captures a free variable, so it contributes nothing here.
        Expr::Column { .. } => {}
        Expr::Ident { name, .. } => note_free(name, bound, free),
        Expr::Call { name, args, .. } => {
            note_free(name, bound, free);
            for a in args {
                collect_free(a, bound, free);
            }
        }
        Expr::Lambda { params, body } => {
            let n = bound.len();
            bound.extend(params.iter().map(|s| s.as_str()));
            collect_free(body, bound, free);
            bound.truncate(n);
        }
        Expr::Let { bindings, body } => {
            let n = bound.len();
            for (name, v) in bindings {
                collect_free(v, bound, free); // a binding's value sees earlier bindings
                bound.push(name.as_str());
            }
            collect_free(body, bound, free);
            bound.truncate(n);
        }
        Expr::Method { recv, args, .. } => {
            collect_free(recv, bound, free);
            for a in args {
                collect_free(a, bound, free);
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_free(left, bound, free);
            collect_free(right, bound, free);
        }
        Expr::Unary { expr, .. } => collect_free(expr, bound, free),
        Expr::If { cond, then_branch, else_branch, .. } => {
            collect_free(cond, bound, free);
            collect_free(then_branch, bound, free);
            collect_free(else_branch, bound, free);
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                collect_free(x, bound, free);
            }
        }
        Expr::Record(fs) => {
            for (_, v) in fs {
                collect_free(v, bound, free);
            }
        }
        Expr::Field { recv, .. } => collect_free(recv, bound, free),
        Expr::Index { recv, index, .. } => {
            collect_free(recv, bound, free);
            collect_free(index, bound, free);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            collect_free(recv, bound, free);
            for o in [start, stop, step].into_iter().flatten() {
                collect_free(o, bound, free);
            }
        }
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(e) = p {
                    collect_free(e, bound, free);
                }
            }
        }
        Expr::Try { expr, .. } => collect_free(expr, bound, free),
        Expr::Match { scrutinee, arms, .. } => {
            collect_free(scrutinee, bound, free);
            for arm in arms {
                let n = bound.len();
                // A pattern's bindings (possibly several, for tuple/record patterns)
                // are locals of both the guard and the arm body.
                push_pattern_binds(&arm.pattern, bound);
                if let Some(g) = &arm.guard {
                    collect_free(g, bound, free);
                }
                collect_free(&arm.body, bound, free);
                bound.truncate(n);
            }
        }
    }
}

/// Push a pattern's bound names (`&str`, borrowing the AST) — recursive for
/// tuple/record patterns.
fn push_pattern_binds<'a>(pat: &'a crate::ast::Pattern, out: &mut Vec<&'a str>) {
    use crate::ast::Pattern;
    match pat {
        Pattern::Bind(name) => out.push(name.as_str()),
        Pattern::Tuple(pats) => pats.iter().for_each(|p| push_pattern_binds(p, out)),
        Pattern::Record(fields) => fields.iter().for_each(|(_, p)| push_pattern_binds(p, out)),
        _ => {}
    }
}

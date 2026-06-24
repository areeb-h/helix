//! Static program analysis the compiler runs before lowering: `memoizable_fns`
//! (which functions are pure, mutable-global-free, and overlapping-recursive — so
//! safe to auto-memoize) and the AST-walking helpers it needs (`has_method`,
//! `any_call`, `count_self_calls`, `reads_mutable`, `children`).

use super::*;

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
    let funcs: Vec<(&str, &[(String, Option<crate::ast::TypeAnn>)], &Expr)> = program
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
                    || any_call(body, &|n| IMPURE_BUILTINS.contains(&n) || impure.contains(n)))
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
        | Expr::Ident { .. } => false,
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
    }
}

/// Count direct recursive calls to `name` in the body.
fn count_self_calls(e: &Expr, name: &str) -> usize {
    let mut n = 0;
    count_self_calls_into(e, name, &mut n);
    n
}

fn count_self_calls_into(e: &Expr, name: &str, n: &mut usize) {
    if let Expr::Call { name: callee, .. } = e {
        if callee == name {
            *n += 1;
        }
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
        | Expr::Ident { .. } => vec![],
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
    }
}

/// True if the program uses `try` anywhere. Programs that use `try` run on the
/// tree-walker, which supports error recovery directly; the bytecode VM does not
/// yet implement exception handling, so the runner routes around it (see `main.rs`).
pub(crate) fn uses_try(stmts: &[Stmt]) -> bool {
    fn expr_uses_try(e: &Expr) -> bool {
        matches!(e, Expr::Try { .. }) || children(e).iter().any(|c| expr_uses_try(c))
    }
    stmts.iter().any(|s| match s {
        Stmt::Assign { value, .. } | Stmt::Destructure { value, .. } => expr_uses_try(value),
        Stmt::Func { body, .. } => expr_uses_try(body),
        Stmt::Expr(e) => expr_uses_try(e),
        Stmt::Import { .. } => false,
    })
}

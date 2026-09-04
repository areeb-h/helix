//! UFCS, resolved by the receiver at the earliest layer that knows it (ADR 0045).
//!
//! A method call whose name is also a declared `fn` has two readings: a method of the
//! receiver's type, or the free function with the receiver as its first argument — and,
//! for a record, a third: a function held in a field of that name. Only the receiver can
//! settle it. The parser used to settle it anyway, at parse time and by NAME: any
//! `x.f(a)` whose `f` was a declared fn that no type owned became `f(x, a)` before either
//! engine ran. That was right for every program it could see and wrong for the one it
//! could not — a record's own `f` could never win against a free `fn f` — and it could
//! not see a PyObject receiver at all.
//!
//! This pass runs after the checker and makes the same rewrite where the checker has
//! PROVED the receiver's type and that type rules the method reading out: it is not
//! `Unknown`, it is not a `Record` (which may carry a field of that name), it is not a
//! frame (which routes elsewhere), and it does not own the name as a real method. For
//! those receivers the call is a call, and it becomes one here — before the compiler and
//! the JIT see it. Every other receiver keeps its method node, and both engines decide at
//! run time through the same route in the same order: a real method, else a
//! function-valued field, else the free fn.
//!
//! WHY THIS LAYER EXISTS BEYOND CORRECTNESS. The JIT's kernel analysis admits a `Call`
//! and not a `Method`, so `range(0, n).map(it.f(1))` fused into native code only because
//! the parser had already rewritten it. Removing that rewrite without this pass measured
//! 25 -> 108 ns per element; `helix jit-explain` reported "0 kernel sites offered" where
//! it had reported 1. The receiver deciding at run time is the RULE; deciding it at
//! compile time where the type is proven is the same rule applied where it is cheapest.
//!
//! The scope discipline mirrors the compiler's `ufcs_fn_slot` and the walker's
//! `ufcs_decl_fn`, which have to agree with each other and with this: a parameter, a
//! `let`, a lambda parameter, a match binding, or a global of the same name shadows the
//! `fn`, and a shadowed name is left alone. The three answer one question and must keep
//! answering it identically — `ufcs_is_decided_by_the_receiver_at_every_layer` in
//! tests/cli.rs is where that is held.

use crate::ast::{Expr, InterpPart, Stmt};
use crate::types::{Type, TypeMap};
use std::collections::HashSet;

/// Rewrite every method call whose receiver's proven type rules the method reading out
/// into the free call it is. Runs once, after `types::check`, on the AST every engine
/// and the JIT will consume — so all of them see the same program.
pub fn resolve_by_type(program: &mut [Stmt], types: &TypeMap) {
    let mut fns = HashSet::new();
    let mut globals = HashSet::new();
    for s in program.iter() {
        match s {
            Stmt::Func { name, .. } => {
                fns.insert(name.clone());
            }
            Stmt::Assign { name, .. } => {
                globals.insert(name.clone());
            }
            Stmt::Destructure { names, .. } => globals.extend(names.iter().cloned()),
            _ => {}
        }
    }
    let cx = Cx { fns, globals, types };
    for s in program.iter_mut() {
        match s {
            Stmt::Func { params, body, .. } => {
                let bound: HashSet<String> = params.iter().map(|(p, _)| p.clone()).collect();
                walk(body, &cx, &bound);
            }
            Stmt::Assign { value, .. } | Stmt::Destructure { value, .. } => {
                walk(value, &cx, &HashSet::new())
            }
            Stmt::Expr(e) => walk(e, &cx, &HashSet::new()),
            Stmt::Import { .. } => {}
        }
    }
}

struct Cx<'a> {
    fns: HashSet<String>,
    globals: HashSet<String>,
    types: &'a TypeMap,
}

/// Children first, so every receiver the checker keyed by address is still where the
/// checker left it when its own call is decided. A rewrite moves exactly one node — the
/// receiver, out of its `Box` and into the new call's arguments — and that node's type is
/// the one fact this pass has already consumed.
fn walk(e: &mut Expr, cx: &Cx, bound: &HashSet<String>) {
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
                if let InterpPart::Expr(x, _) = p {
                    walk(x, cx, bound);
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                walk(x, cx, bound);
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                walk(v, cx, bound);
            }
        }
        Expr::RecordUpdate { base, fields, .. } => {
            walk(base, cx, bound);
            for (_, v) in fields {
                walk(v, cx, bound);
            }
        }
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } => walk(recv, cx, bound),
        Expr::Unary { expr, .. } => walk(expr, cx, bound),
        Expr::Binary { left, right, .. } => {
            walk(left, cx, bound);
            walk(right, cx, bound);
        }
        Expr::Call { args, .. } => {
            for a in args {
                walk(a, cx, bound);
            }
        }
        Expr::CallValue { callee, args, .. } => {
            walk(callee, cx, bound);
            for a in args {
                walk(a, cx, bound);
            }
        }
        Expr::Index { recv, index, .. } => {
            walk(recv, cx, bound);
            walk(index, cx, bound);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            walk(recv, cx, bound);
            for x in [start, stop, step].into_iter().flatten() {
                walk(x, cx, bound);
            }
        }
        Expr::Lambda { params, body, .. } => {
            let mut b = bound.clone();
            b.extend(params.iter().cloned());
            walk(std::rc::Rc::make_mut(body), cx, &b);
        }
        Expr::Let { bindings, body, .. } => {
            let mut b = bound.clone();
            for (n, v) in bindings.iter_mut() {
                walk(v, cx, &b);
                b.insert(n.clone());
            }
            walk(body, cx, &b);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            walk(cond, cx, bound);
            walk(then_branch, cx, bound);
            walk(else_branch, cx, bound);
        }
        Expr::Try { expr, .. } => walk(expr, cx, bound),
        Expr::Match { scrutinee, arms, .. } => {
            walk(scrutinee, cx, bound);
            for arm in arms.iter_mut() {
                let mut b = bound.clone();
                for name in crate::interp::pattern_binding_names(&arm.pattern) {
                    b.insert(name);
                }
                if let Some(g) = &mut arm.guard {
                    walk(g, cx, &b);
                }
                walk(&mut arm.body, cx, &b);
            }
        }
        Expr::Method { .. } => {
            if let Expr::Method { recv, args, named, .. } = e {
                walk(recv, cx, bound);
                for a in args.iter_mut() {
                    walk(a, cx, bound);
                }
                for (_, v) in named.iter_mut() {
                    walk(v, cx, bound);
                }
            }
            if let Some(call) = call_reading(e, cx, bound) {
                *e = call;
            }
        }
    }
}

/// The free-call reading of a method node, when the receiver's proven type leaves no
/// other. `None` means "keep the method node and let the engines decide".
fn call_reading(e: &mut Expr, cx: &Cx, bound: &HashSet<String>) -> Option<Expr> {
    let Expr::Method { recv, name, args, named, ufcs, line, col } = e else {
        return None;
    };
    // Named arguments resolve against a callee's signature; that is a free call's job and
    // the compiler's, not this pass's. Same refusal the parser's rewrite made.
    if !named.is_empty() {
        return None;
    }
    // The spelling the compiler would resolve: the module-qualified name when `rw` filled
    // it in, the written name otherwise. `ufcs_fn_slot` and `ufcs_decl_fn` look this up.
    let free = ufcs.as_deref().unwrap_or(name.as_str());
    // A shadowed name is not the fn — a parameter, `let`, lambda parameter, match binding
    // or global of that name is what `x.f(a)` reaches, and those are not callable in
    // method position. Mirrors `ufcs_fn_slot`'s three refusals exactly.
    if bound.contains(free) || cx.globals.contains(free) || !cx.fns.contains(free) {
        return None;
    }
    let t = cx.types.get(&(&**recv as *const Expr))?;
    // The receivers whose method reading the type does NOT rule out: an unknown one, a
    // record (a field of this name may hold a function), and the frame types, which route
    // by their own rules. `Missing` is bottom and left to the engines on principle.
    if matches!(
        t,
        Type::Unknown | Type::Missing | Type::Record(_) | Type::DataFrame | Type::GroupBy
    ) {
        return None;
    }
    // A type that OWNS the name keeps its method — `xs.count()` beside `fn count` is still
    // the Array's count. Universal methods are owned by every type, so they never rewrite.
    if crate::registry::type_owns_method(&t.to_string(), name) {
        return None;
    }
    let mut call_args = Vec::with_capacity(args.len() + 1);
    call_args.push(std::mem::replace(&mut **recv, Expr::Missing));
    call_args.append(args);
    Some(Expr::Call { name: free.to_string(), args: call_args, line: *line, col: *col })
}

//! The effect closure of a user function, over the call graph — in two buckets.
//!
//! Helix already classifies every BUILTIN along two independent axes, and both are held
//! exhaustively by drift guards:
//!
//! - **Authority** — [`crate::capability::effect_of`] / `method_effect_of`, the fs/net/process
//!   categories the sandbox gates. `no_ungated_effectful_builtins` walks `BUILTINS` and forces
//!   every effectful one to be either gated or in a justified `harmless` allowlist.
//! - **Reproducibility** — `BuiltinDef::pure` in the registry, which decides whether a result
//!   may be memoized.
//!
//! **The two axes are genuinely separate, and that is the point of this module.** `now()` and
//! `clock_monotonic()` are `pure: false` and yet hold no authority: they are in the harmless
//! allowlist precisely because reading a clock touches no file and no socket. A function built
//! from them is fully sandboxable and still not reproducible. Reporting only one axis would
//! hide exactly that case.
//!
//! What this module adds is PROPAGATION, and it keeps two questions apart that one report used
//! to blur (field build, 1.29 and 1.47 — one verdict was fail-open, another over-approximated,
//! from a single root: not separating what a function DOES from what a value flowing through it
//! CAN do):
//!
//! - **does** — what happens when the function is called: its own calls, closed over the call
//!   graph, plus every function it hands to a callee that CALLS it. `apply1(slurp, p)` with
//!   `apply1(f, x) = f(x)` reads a file: `apply1` calls its parameter, so the argument's
//!   effects are the caller's own.
//! - **carries** — what the values it builds or hands on can do: a lambda it returns or stores,
//!   a named function it passes to a position that does not call it. `define(table) = {save:
//!   (row) => row.write_to(table)}` writes nothing; the record it returns can.
//!
//! And where a callee cannot be seen — a parameter called (or handed to a verb that calls its
//! argument) with nothing known about it, a method no type owns (a record field holding a
//! function), a local bound to something other than a lambda — the answer is **unknown**, with
//! the call that forced it, never "no authority". A parameter callee is discharged at a call
//! site that passes a named function or a lambda, so `apply1` alone is unknown while
//! `apply1(slurp, p)` is `fs-read` and `apply1((q) => q + 1, x)` is pure.
//!
//! It is still not an effect system: nothing here infers, unifies or annotates. Everything is
//! syntactic — which is exactly why the answer is `unknown`, and not a guess, wherever syntax
//! runs out.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::ast::{Expr, Stmt};
use crate::capability::Effect;

/// Verbs that CALL a function argument, with the positions of those arguments. A parameter
/// handed to one of these is a callee the tool cannot see, exactly as a parameter called
/// directly is; a lambda or a named function handed to one is called, so its effects are the
/// caller's own. Every parser verb that takes a bare bound name is here (drift-guarded), plus
/// the folds whose function is the SECOND argument.
pub(crate) const HIGHER_ORDER_VERBS: &[&str] = &[
    "map", "any", "all", "filter", "where", "count_where", "flat_map", "take_while",
    "drop_while", "position", "sort_by", "min_by", "max_by", "reduce", "scan", "zipmap",
];

/// The argument positions a higher-order verb calls, or `None` for any other name.
fn fn_arg_positions(verb: &str) -> Option<&'static [usize]> {
    const FIRST: &[usize] = &[0];
    const SECOND: &[usize] = &[1];
    match verb {
        "reduce" | "scan" | "zipmap" => Some(SECOND),
        v if HIGHER_ORDER_VERBS.contains(&v) => Some(FIRST),
        _ => None,
    }
}

/// What one bucket of a function reaches.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reach {
    /// Authority categories reached, each with the call path that introduced it.
    pub effects: Vec<(Effect, Vec<String>)>,
    /// `None` when nothing irreproducible is reached; otherwise the name that makes it so
    /// and the path to it.
    pub nondeterministic: Option<(String, Vec<String>)>,
    /// A callee the tool cannot resolve, and the path to it. Reported as UNKNOWN authority —
    /// the failure direction that is safe for an audit tool.
    pub unknown: Option<(String, Vec<String>)>,
}

impl Reach {
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.nondeterministic.is_none() && self.unknown.is_none()
    }
}

/// What one function reaches.
#[derive(Debug, Clone, PartialEq)]
pub struct FnEffects {
    pub name: String,
    /// When the function is called.
    pub does: Reach,
    /// What the values it builds or hands on can do.
    pub carries: Reach,
}

impl FnEffects {
    /// Reproducible as far as the tool can see: nothing irreproducible reached when called,
    /// and no callee it could not resolve.
    pub fn deterministic(&self) -> bool {
        self.does.nondeterministic.is_none() && self.does.unknown.is_none()
    }
}

/// Every top-level function in a loaded program — `fn` definitions and top-level bindings
/// whose value is a lambda — with its effect closure.
///
/// Recursion terminates on a visited set rather than a depth limit: a cycle contributes
/// nothing new by definition, since effects are a union.
pub fn closure(stmts: &[Stmt]) -> Vec<FnEffects> {
    let prog = Program::new(stmts);
    prog.fns.keys().map(|name| prog.report(name)).collect()
}

/// The shape of an argument or a callee, as far as syntax shows it.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    /// An identifier naming a top-level function.
    Fn(String),
    /// An identifier naming a parameter of the enclosing function.
    Param(String),
    /// An identifier bound to a lambda inside the body (`g = (x) => …`).
    LocalLambda(String),
    /// Any other identifier — a local bound to something the tool does not follow.
    Local(String),
    Lambda,
    Other,
}

#[derive(Debug, Clone)]
enum Kind {
    /// `name(args)`.
    Free { name: String, args: Vec<Shape> },
    /// `recv.name(args)`.
    Method { name: String, recv: Shape, args: Vec<Shape> },
    /// `(callee)(args)` — a value called.
    Value(Shape),
    /// A top-level function used as a value.
    Ref(String),
}

/// One thing a body reaches for, and whether it sits inside a lambda that is not called here
/// (returned or stored), which puts it in the `carries` bucket.
#[derive(Debug, Clone)]
struct Site {
    kind: Kind,
    carried: bool,
}

struct FnInfo<'a> {
    params: Vec<&'a str>,
    body: &'a Expr,
}

struct Program<'a> {
    fns: BTreeMap<&'a str, FnInfo<'a>>,
    /// Per function: the indices of the parameters it calls — directly, through a verb that
    /// calls its argument, or by handing them to a callee that calls them (a fixpoint).
    param_callees: BTreeMap<&'a str, BTreeSet<usize>>,
    /// Per function: every site in its body, with its bucket.
    sites: BTreeMap<&'a str, Vec<Site>>,
    /// Per function: names bound to lambdas inside the body.
    lambda_locals: BTreeMap<&'a str, HashSet<String>>,
}

/// A site with the addresses its classification needs: the arguments' (receiver first for a
/// method), so a lambda or a function reference among them can be placed, and the callee's.
struct RawSite {
    ptr: *const Expr,
    kind: Kind,
    arg_ptrs: Vec<*const Expr>,
    callee_ptr: Option<*const Expr>,
}

struct RawLambda {
    ptr: *const Expr,
    /// Every node address inside this lambda's body (nested lambdas included).
    inside: HashSet<*const Expr>,
    /// The `let` name this lambda is bound to, when it is a binding's value.
    bound_as: Option<String>,
}

struct Raw {
    sites: Vec<RawSite>,
    lambdas: Vec<RawLambda>,
    lambda_locals: HashSet<String>,
}

fn shape(e: &Expr, fns: &BTreeMap<&str, FnInfo>, params: &[&str], lambda_locals: &HashSet<String>) -> Shape {
    match e {
        Expr::Ident { name, .. } => {
            if params.contains(&name.as_str()) {
                Shape::Param(name.clone())
            } else if lambda_locals.contains(name) {
                Shape::LocalLambda(name.clone())
            } else if fns.contains_key(name.as_str()) {
                Shape::Fn(name.clone())
            } else {
                Shape::Local(name.clone())
            }
        }
        Expr::Lambda { .. } => Shape::Lambda,
        _ => Shape::Other,
    }
}

/// Every site and lambda in one function body.
///
/// Uses the shared [`crate::visit::walk_expr`], whose exhaustive match means a new `Expr`
/// variant fails compilation there rather than being silently skipped here.
fn collect(body: &Expr, fns: &BTreeMap<&str, FnInfo>, params: &[&str]) -> Raw {
    // Names bound to lambdas, and the origins of synthesized bound-function lambdas:
    // `xs.map(f)` parses to `(it) => f(it)` keeping `f` as its origin, and that origin is the
    // lambda's own reference to `f`, not a second, stored use of it.
    let mut lambda_locals: HashSet<String> = HashSet::new();
    let mut ignore: HashSet<*const Expr> = HashSet::new();
    let mut bound_as: HashMap<*const Expr, String> = HashMap::new();
    crate::visit::walk_expr(body, &mut |x| match x {
        Expr::Let { bindings, .. } => {
            for (n, v) in bindings {
                if matches!(v, Expr::Lambda { .. }) {
                    lambda_locals.insert(n.clone());
                    bound_as.insert(v as *const Expr, n.clone());
                }
            }
        }
        Expr::Lambda { bound: Some(origin), .. } => {
            ignore.insert(&**origin as *const Expr);
        }
        _ => {}
    });
    let mut sites: Vec<RawSite> = Vec::new();
    let mut lambdas: Vec<RawLambda> = Vec::new();
    crate::visit::walk_expr(body, &mut |x| {
        let ptr = x as *const Expr;
        match x {
            Expr::Call { name, args, .. } => sites.push(RawSite {
                ptr,
                kind: Kind::Free {
                    name: name.clone(),
                    args: args.iter().map(|a| shape(a, fns, params, &lambda_locals)).collect(),
                },
                arg_ptrs: args.iter().map(|a| a as *const Expr).collect(),
                callee_ptr: None,
            }),
            Expr::Method { recv, name, args, .. } => sites.push(RawSite {
                ptr,
                kind: Kind::Method {
                    name: name.clone(),
                    recv: shape(recv, fns, params, &lambda_locals),
                    args: args.iter().map(|a| shape(a, fns, params, &lambda_locals)).collect(),
                },
                arg_ptrs: std::iter::once(&**recv as *const Expr)
                    .chain(args.iter().map(|a| a as *const Expr))
                    .collect(),
                callee_ptr: None,
            }),
            Expr::CallValue { callee, args, .. } => sites.push(RawSite {
                ptr,
                kind: Kind::Value(shape(callee, fns, params, &lambda_locals)),
                arg_ptrs: args.iter().map(|a| a as *const Expr).collect(),
                callee_ptr: Some(&**callee as *const Expr),
            }),
            Expr::Ident { name, .. }
                if !ignore.contains(&ptr)
                    && !params.contains(&name.as_str())
                    && !lambda_locals.contains(name)
                    && fns.contains_key(name.as_str()) =>
            {
                sites.push(RawSite { ptr, kind: Kind::Ref(name.clone()), arg_ptrs: vec![], callee_ptr: None })
            }
            Expr::Lambda { body: lbody, .. } => {
                let mut inside: HashSet<*const Expr> = HashSet::new();
                crate::visit::walk_expr(lbody, &mut |y| {
                    inside.insert(y as *const Expr);
                });
                lambdas.push(RawLambda { ptr, inside, bound_as: bound_as.get(&ptr).cloned() });
            }
            _ => {}
        }
    });
    Raw { sites, lambdas, lambda_locals }
}

/// Does this site call the lambda-local `n` — directly, as a value, or by handing it to a
/// verb or a function that calls its argument?
fn calls_local(
    k: &Kind,
    n: &str,
    fns: &BTreeMap<&str, FnInfo>,
    pcs: &BTreeMap<&str, BTreeSet<usize>>,
) -> bool {
    let is_n = |s: &Shape| matches!(s, Shape::LocalLambda(m) if m == n);
    match k {
        Kind::Free { name, args } => {
            name == n
                || (!fns.contains_key(name.as_str())
                    && fn_arg_positions(name).is_some_and(|ps| ps.iter().any(|j| args.get(*j).is_some_and(is_n))))
                || pcs.get(name.as_str()).is_some_and(|p| p.iter().any(|j| args.get(*j).is_some_and(is_n)))
        }
        Kind::Method { name, recv, args } => {
            let all: Vec<&Shape> = std::iter::once(recv).chain(args.iter()).collect();
            (!fns.contains_key(name.as_str())
                && fn_arg_positions(name).is_some_and(|ps| ps.iter().any(|j| args.get(*j).is_some_and(is_n))))
                || pcs.get(name.as_str()).is_some_and(|p| p.iter().any(|j| all.get(*j).is_some_and(|s| is_n(s))))
        }
        Kind::Value(s) => is_n(s),
        Kind::Ref(_) => false,
    }
}

/// For each site of a body: does it sit inside a lambda that is NOT called here?
///
/// A lambda is called here when it is at a position whose receiver calls it — the callee of a
/// value call, a function argument of a higher-order verb, an argument of a function at a
/// parameter that function calls — or when it is bound to a name that a does-context site
/// calls. The second condition depends on the answer for the enclosing lambdas, so it is an
/// inner fixpoint.
fn bucket_sites(r: &Raw, fns: &BTreeMap<&str, FnInfo>, pcs: &BTreeMap<&str, BTreeSet<usize>>) -> Vec<bool> {
    let mut does_pos: HashSet<*const Expr> = HashSet::new();
    for s in &r.sites {
        match &s.kind {
            Kind::Free { name, .. } => {
                if let Some(p) = pcs.get(name.as_str()) {
                    does_pos.extend(p.iter().filter_map(|j| s.arg_ptrs.get(*j).copied()));
                } else if let Some(ps) = fn_arg_positions(name) {
                    does_pos.extend(ps.iter().filter_map(|j| s.arg_ptrs.get(*j).copied()));
                }
            }
            Kind::Method { name, .. } => {
                if let Some(p) = pcs.get(name.as_str()) {
                    // UFCS: the receiver is argument 0, and `arg_ptrs` is receiver-first.
                    does_pos.extend(p.iter().filter_map(|j| s.arg_ptrs.get(*j).copied()));
                } else if let Some(ps) = fn_arg_positions(name) {
                    does_pos.extend(ps.iter().filter_map(|j| s.arg_ptrs.get(*j + 1).copied()));
                }
            }
            Kind::Value(_) => {
                if let Some(c) = s.callee_ptr {
                    does_pos.insert(c);
                }
            }
            Kind::Ref(_) => {}
        }
    }
    let mut lambda_does: Vec<bool> = r.lambdas.iter().map(|l| does_pos.contains(&l.ptr)).collect();
    // A site is carried inside a lambda that is not called here; a named function used as a
    // value is carried unless it sits where its receiver calls it (`{go: slurp}` carries,
    // `apply1(slurp, p)` does).
    let site_carried = |lambda_does: &[bool]| -> Vec<bool> {
        r.sites
            .iter()
            .map(|s| {
                let in_uncalled_lambda =
                    r.lambdas.iter().zip(lambda_does).any(|(l, d)| !*d && l.inside.contains(&s.ptr));
                match s.kind {
                    Kind::Ref(_) => in_uncalled_lambda || !does_pos.contains(&s.ptr),
                    _ => in_uncalled_lambda,
                }
            })
            .collect()
    };
    loop {
        let sc = site_carried(&lambda_does);
        let mut changed = false;
        for (li, l) in r.lambdas.iter().enumerate() {
            if lambda_does[li] {
                continue;
            }
            let Some(n) = &l.bound_as else { continue };
            if r.sites.iter().zip(&sc).any(|(s, carried)| !*carried && calls_local(&s.kind, n, fns, pcs)) {
                lambda_does[li] = true;
                changed = true;
            }
        }
        if !changed {
            return sc;
        }
    }
}

impl<'a> Program<'a> {
    fn new(stmts: &'a [Stmt]) -> Self {
        let mut fns: BTreeMap<&'a str, FnInfo<'a>> = BTreeMap::new();
        for s in stmts {
            match s {
                Stmt::Func { name, params, body, .. } => {
                    fns.insert(name.as_str(), FnInfo { params: params.iter().map(|(p, _)| p.as_str()).collect(), body });
                }
                // A top-level binding whose value is a lambda IS a function definition,
                // spelled differently — reported like one, and resolvable as a callee.
                Stmt::Assign { name, value: Expr::Lambda { params, body, .. }, .. } => {
                    fns.insert(name.as_str(), FnInfo { params: params.iter().map(|p| p.as_str()).collect(), body });
                }
                _ => {}
            }
        }
        let raw: BTreeMap<&str, Raw> =
            fns.iter().map(|(n, f)| (*n, collect(f.body, &fns, &f.params))).collect();
        // Which parameters does each function call? Monotone: a parameter found to be a
        // callee makes more argument positions "called", which can move a lambda from
        // carried to does, whose sites can name more parameter callees.
        let mut param_callees: BTreeMap<&str, BTreeSet<usize>> =
            fns.keys().map(|n| (*n, BTreeSet::new())).collect();
        loop {
            let mut changed = false;
            for (name, f) in &fns {
                let r = &raw[name];
                let carried = bucket_sites(r, &fns, &param_callees);
                let idx = |p: &str| f.params.iter().position(|q| *q == p);
                let mut pcs = param_callees[name].clone();
                for (site, carried) in r.sites.iter().zip(&carried) {
                    if *carried {
                        continue;
                    }
                    match &site.kind {
                        Kind::Free { name: callee, args } => {
                            if let Some(i) = idx(callee) {
                                pcs.insert(i);
                            } else if let Some(cp) = param_callees.get(callee.as_str()) {
                                for j in cp {
                                    if let Some(Shape::Param(p)) = args.get(*j)
                                        && let Some(i) = idx(p)
                                    {
                                        pcs.insert(i);
                                    }
                                }
                            } else if let Some(ps) = fn_arg_positions(callee) {
                                for j in ps {
                                    if let Some(Shape::Param(p)) = args.get(*j)
                                        && let Some(i) = idx(p)
                                    {
                                        pcs.insert(i);
                                    }
                                }
                            }
                        }
                        Kind::Method { name: m, recv, args } => {
                            if let Some(cp) = param_callees.get(m.as_str()) {
                                let all: Vec<&Shape> = std::iter::once(recv).chain(args.iter()).collect();
                                for j in cp {
                                    if let Some(Shape::Param(p)) = all.get(*j)
                                        && let Some(i) = idx(p)
                                    {
                                        pcs.insert(i);
                                    }
                                }
                            } else if let Some(ps) = fn_arg_positions(m) {
                                for j in ps {
                                    if let Some(Shape::Param(p)) = args.get(*j)
                                        && let Some(i) = idx(p)
                                    {
                                        pcs.insert(i);
                                    }
                                }
                            }
                        }
                        Kind::Value(Shape::Param(p)) => {
                            if let Some(i) = idx(p) {
                                pcs.insert(i);
                            }
                        }
                        _ => {}
                    }
                }
                if pcs != param_callees[name] {
                    param_callees.insert(name, pcs);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let sites: BTreeMap<&str, Vec<Site>> = fns
            .keys()
            .map(|n| {
                let r = &raw[n];
                let carried = bucket_sites(r, &fns, &param_callees);
                (*n, r.sites.iter().zip(carried).map(|(s, c)| Site { kind: s.kind.clone(), carried: c }).collect())
            })
            .collect();
        let lambda_locals = raw.iter().map(|(n, r)| (*n, r.lambda_locals.clone())).collect();
        Program { fns, param_callees, sites, lambda_locals }
    }

    fn report(&self, root: &'a str) -> FnEffects {
        let mut w = Walk { prog: self, seen: BTreeSet::new(), does: Reach::default(), carries: Reach::default() };
        w.visit(root, &mut Vec::new(), false);
        // The root's own parameter callees are unknown here: nothing has been passed yet.
        if w.does.unknown.is_none()
            && let Some(pcs) = self.param_callees.get(root)
            && let Some(j) = pcs.iter().next()
            && let Some(p) = self.fns[root].params.get(*j)
        {
            w.does.unknown = Some((p.to_string(), vec![root.to_string(), p.to_string()]));
        }
        for reach in [&mut w.does, &mut w.carries] {
            reach.effects.sort_by_key(|(e, _)| e.label());
        }
        FnEffects { name: root.to_string(), does: w.does, carries: w.carries }
    }
}

struct Walk<'p, 'a> {
    prog: &'p Program<'a>,
    /// Visited (function, bucket) pairs. First path wins, not shortest: the walk is
    /// deterministic, so this is stable, and a reader wants *a* witness rather than the best.
    seen: BTreeSet<(String, bool)>,
    does: Reach,
    carries: Reach,
}

impl Walk<'_, '_> {
    fn bucket(&mut self, carried: bool) -> &mut Reach {
        if carried { &mut self.carries } else { &mut self.does }
    }

    fn visit(&mut self, name: &str, path: &mut Vec<String>, carried: bool) {
        if !self.seen.insert((name.to_string(), carried)) {
            return;
        }
        let prog = self.prog;
        let Some(sites) = prog.sites.get(name) else { return };
        let params: &[&str] = &prog.fns[name].params;
        let locals = &prog.lambda_locals[name];
        path.push(name.to_string());
        for site in sites {
            let c = carried || site.carried;
            match &site.kind {
                Kind::Free { name: callee, args } => {
                    if params.contains(&callee.as_str()) {
                        // This function's own parameter: its callers decide (see `report`).
                    } else if prog.fns.contains_key(callee.as_str()) {
                        self.discharge(callee, args.iter().collect(), path, c);
                        self.visit(callee, path, c);
                    } else if locals.contains(callee) {
                        // A lambda bound here; its body is walked where it sits.
                    } else if crate::registry::lookup(callee).is_some() || crate::capability::effect_of(callee).gated() {
                        self.record(callee, false, path, c);
                        self.unknown_locals(callee, args, path, c);
                    } else if crate::registry::any_type_owns_method(callee) {
                        // `map(xs, f)`: a method in its free spelling.
                        self.record(callee, true, path, c);
                        self.unknown_locals(callee, args, path, c);
                    } else {
                        // A local bound to something other than a lambda, or a name `check`
                        // would have refused.
                        self.mark_unknown(c, callee, path, &[callee]);
                    }
                }
                Kind::Method { name: m, recv, args } => {
                    if prog.fns.contains_key(m.as_str()) {
                        // UFCS: `p.slurp()` is `slurp(p)`; the receiver is argument 0.
                        let all: Vec<&Shape> = std::iter::once(recv).chain(args.iter()).collect();
                        self.discharge(m, all, path, c);
                        self.visit(m, path, c);
                    } else if fn_arg_positions(m).is_some()
                        || crate::registry::any_type_owns_method(m)
                        || crate::capability::method_effect_of(m).gated()
                    {
                        self.unknown_locals(m, args, path, c);
                    } else {
                        // A method no type owns: a record field holding a function, which the
                        // tool cannot see through — or a mistake `check` would have caught.
                        self.mark_unknown(c, m, path, &[m]);
                    }
                    self.record(m, true, path, c);
                }
                Kind::Value(shape) => match shape {
                    Shape::Fn(f) => self.visit(f, path, c),
                    Shape::Lambda | Shape::LocalLambda(_) | Shape::Param(_) => {}
                    Shape::Local(l) => self.mark_unknown(c, l, path, &[l]),
                    Shape::Other => self.mark_unknown(c, "a function value", path, &["a function value"]),
                },
                Kind::Ref(f) => self.visit(f, path, c),
            }
        }
        path.pop();
    }

    /// At a call of `callee`, every parameter `callee` calls must receive something the tool
    /// can see: a named function or a lambda (walked where they sit), or this function's own
    /// parameter (its callers decide). Anything else is a callee nobody can see.
    fn discharge(&mut self, callee: &str, args: Vec<&Shape>, path: &[String], carried: bool) {
        let Some(pcs) = self.prog.param_callees.get(callee) else { return };
        for j in pcs {
            match args.get(*j) {
                Some(Shape::Fn(_)) | Some(Shape::Lambda) | Some(Shape::LocalLambda(_)) | Some(Shape::Param(_)) => {}
                _ => {
                    let pname = self.prog.fns[callee].params.get(*j).copied().unwrap_or("?").to_string();
                    self.mark_unknown(carried, &pname, path, &[callee, &pname]);
                }
            }
        }
    }

    /// A local that is not a lambda, handed to a verb that calls it.
    fn unknown_locals(&mut self, verb: &str, args: &[Shape], path: &[String], carried: bool) {
        let Some(ps) = fn_arg_positions(verb) else { return };
        for j in ps {
            if let Some(Shape::Local(l)) = args.get(*j) {
                self.mark_unknown(carried, l, path, &[verb, l]);
            }
        }
    }

    fn mark_unknown(&mut self, carried: bool, who: &str, path: &[String], tail: &[&str]) {
        let b = self.bucket(carried);
        if b.unknown.is_none() {
            let mut p = path.to_vec();
            p.extend(tail.iter().map(|s| s.to_string()));
            b.unknown = Some((who.to_string(), p));
        }
    }

    fn record(&mut self, callee: &str, is_method: bool, path: &[String], carried: bool) {
        let eff = if is_method {
            crate::capability::method_effect_of(callee)
        } else {
            crate::capability::effect_of(callee)
        };
        // A METHOD'S PURITY IS NOT IN THE BUILTIN CATALOG, so only free calls answer the
        // reproducibility question here. Claiming determinism from an incomplete axis would
        // be worse than saying nothing, so an unknown name is treated as reproducible only
        // when the catalog actually says so. Output is an effect, not an input: `print`
        // cannot change what a function COMPUTES, and calling a printing function
        // irreproducible would flag almost every program while telling no one anything.
        let irreproducible = !is_method
            && crate::registry::lookup(callee).is_some_and(|d| !d.pure)
            && !matches!(callee, "print" | "emit" | "write" | "elog");
        let b = self.bucket(carried);
        if eff.gated() && !b.effects.iter().any(|(e, _)| *e == eff) {
            let mut p = path.to_vec();
            p.push(callee.to_string());
            b.effects.push((eff, p));
        }
        if irreproducible && b.nondeterministic.is_none() {
            let mut p = path.to_vec();
            p.push(callee.to_string());
            b.nondeterministic = Some((callee.to_string(), p));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parser's list of verbs that take a bare bound name is exactly the list of verbs
    /// whose argument is CALLED — so a parameter handed to one is a callee here too.
    #[test]
    fn every_bound_fn_verb_calls_its_argument_here_too() {
        for v in crate::parser::BOUND_FN_VERBS {
            assert!(fn_arg_positions(v).is_some(), "`{v}` takes a bound name in the parser but is not a higher-order verb here");
        }
    }
}

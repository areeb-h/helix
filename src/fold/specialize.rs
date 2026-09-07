//! Specialization at load time (ADR 0051): a function compiled for what its call site knows.
//!
//! A call site often knows more than "a record": it knows the record's KEYS, because it
//! wrote the literal — `sql(m, {where: {city: req.city}, limit: 10})` — even when the values
//! come from a request. A function reading such a record asks it questions the shape alone
//! answers: `spec.limit?` is `missing` when there is no `limit` key, `spec.keys()` is a
//! constant, `type_of(spec)` is `"Record"`. And a call site often passes a CONSTANT — a
//! top-level name the sandbox holds, a scalar literal — that the callee reads field by field.
//!
//! So a call whose arguments carry any of that is rewritten to call a CLONE of the callee,
//! made once per function and per what is known (`sql$3`), in which every such question is
//! answered in place: the absent key is `missing`, the constant name is the global, the
//! literal is the literal. The fold then runs over the clone as over any function — a
//! constant `if` selects its branch, a key check over a literal list folds to `true`, a
//! field of a held global folds to its value — and what remains is the work the runtime
//! values genuinely need. Nothing about the runtime values is assumed: a present key's value
//! is still read at run time, and may be `missing`.
//!
//! A method through a record the sandbox holds — `M.sql(spec)`, the object API a library
//! builds by closing over a model — is seen through the same way: the closure's body becomes
//! a top-level function of its own, its captured values top-level bindings (with the literal
//! the fold would write for them), and the call a direct one, which the shape rule then
//! specializes. That is where the field build's renders live, and the field build's
//! `prepare`/`bind` — render once, bind per request — is what the two rules together give
//! every route without asking.
//!
//! What keeps it bounded: clones are memoized per (function, knowledge), capped per function
//! and per program, and never made for a body past a size; the depth of transitive
//! specialization is capped; a knowledge that folds nothing (a name the sandbox does not
//! hold) is `Any`, so it fragments nothing. `HELIX_NOSPECIALIZE=1` turns the pass off for an
//! A/B, as `HELIX_NOFOLD=1` turns off the fold it rides on.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::{Expr, InterpPart, Stmt, TypeAnn};
use crate::types::TypeMap;
use crate::value::{FuncVal, Value};

/// The most clones one program may hold.
const MAX_CLONES: usize = 64;
/// The most clones one function may have.
const MAX_PER_FN: usize = 8;
/// How far a specialization may follow calls into callees.
const MAX_DEPTH: usize = 4;
/// A body larger than this (in nodes) is not cloned.
const MAX_NODES: usize = 4_096;
/// A record with more keys than this is `Any`.
const MAX_KEYS: usize = 32;

/// A scalar literal, hashable so it can key a clone.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Lit {
    Int(i64),
    Float(u64),
    Str(String),
    Bool(bool),
    Missing,
}

impl Lit {
    fn from_expr(e: &Expr) -> Option<Lit> {
        Some(match e {
            Expr::Int(i) => Lit::Int(*i),
            Expr::Float(f) => Lit::Float(f.to_bits()),
            Expr::Str(s) => Lit::Str(s.clone()),
            Expr::Bool(b) => Lit::Bool(*b),
            Expr::Missing => Lit::Missing,
            _ => return None,
        })
    }

    fn to_expr(&self) -> Expr {
        match self {
            Lit::Int(i) => Expr::Int(*i),
            Lit::Float(b) => Expr::Float(f64::from_bits(*b)),
            Lit::Str(s) => Expr::Str(s.clone()),
            Lit::Bool(b) => Expr::Bool(*b),
            Lit::Missing => Expr::Missing,
        }
    }
}

/// What a call site knows about an argument.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Binding {
    /// A record literal: its keys, in order, and what is known of each value.
    Shape(Vec<(String, Binding)>),
    /// The argument IS this top-level immutable name, which the sandbox holds.
    Global(String),
    /// A scalar literal.
    Lit(Lit),
    Any,
}

impl Binding {
    fn shape(&self) -> Option<&[(String, Binding)]> {
        match self {
            Binding::Shape(fs) => Some(fs),
            _ => None,
        }
    }
}

/// The names bound in the body being rewritten, innermost last, and what is known of each.
struct Env {
    stack: Vec<(String, Binding)>,
}

impl Env {
    fn new() -> Self {
        Env { stack: Vec::new() }
    }
    fn mark(&self) -> usize {
        self.stack.len()
    }
    fn truncate(&mut self, mark: usize) {
        self.stack.truncate(mark);
    }
    fn bind(&mut self, name: &str, b: Binding) {
        self.stack.push((name.to_string(), b));
    }
    fn shadow(&mut self, name: &str) {
        self.bind(name, Binding::Any);
    }
    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.stack.iter().rev().find(|(n, _)| n == name).map(|(_, b)| b)
    }
}

/// One of the program's own functions, as it was before any fold: what a clone is made
/// from, and the addresses of its nodes, under which the checker recorded its types.
struct FnDef {
    params: Vec<(String, Option<TypeAnn>)>,
    defaults: Vec<Option<Expr>>,
    ret: Option<TypeAnn>,
    body: Expr,
    ptrs: Vec<*const Expr>,
    line: usize,
    col: usize,
}

pub(crate) struct Specializer<'t> {
    types: &'t mut TypeMap,
    funcs: HashMap<String, FnDef>,
    memo: HashMap<(String, Vec<Binding>), Option<String>>,
    per_fn: HashMap<String, usize>,
    total: usize,
    next_id: usize,
    devirt: HashMap<(String, String), Option<String>>,
    /// Clones and devirtualized functions, to be appended to the program.
    pending: Vec<Stmt>,
    /// How many of `pending` the sandbox has been handed already.
    handed: usize,
    /// Captured values hoisted to top-level bindings, to be placed at the program's start.
    hoisted: Vec<Stmt>,
    /// The same values, for the sandbox to hold at once.
    held: Vec<(String, Value)>,
    /// Top-level names the sandbox holds, as far as this pass has seen: what a call inside a
    /// clone may know an argument to be.
    globals: HashSet<String>,
    enabled: bool,
}

impl<'t> Specializer<'t> {
    pub(crate) fn new(program: &[Stmt], types: &'t mut TypeMap, enabled: bool) -> Self {
        let assigned: HashSet<&str> = program
            .iter()
            .flat_map(|s| match s {
                Stmt::Assign { name, .. } => vec![name.as_str()],
                Stmt::Destructure { names, .. } => names.iter().map(String::as_str).collect(),
                _ => Vec::new(),
            })
            .collect();
        // A top-level immutable name is a global inside a clone: `grp = NOGROUP` then
        // `grp.by` folds through the held record. (One the sandbox turns out not to hold
        // folds nothing and costs nothing but a memo key of its own.)
        let globals: HashSet<String> = program
            .iter()
            .flat_map(|s| match s {
                Stmt::Assign { name, mutable: false, .. } => vec![name.clone()],
                Stmt::Destructure { names, mutable: false, .. } => names.clone(),
                _ => Vec::new(),
            })
            .collect();
        let mut funcs = HashMap::new();
        for s in program {
            if let Stmt::Func { name, params, defaults, ret, body, line, col, .. } = s
                && !assigned.contains(name.as_str())
            {
                // The snapshot owns its lambda bodies: a body shared with the program would
                // keep the fold from rewriting the program's own in place.
                let mut own = body.clone();
                crate::fold::unshare_lambdas(&mut own);
                funcs.insert(
                    name.clone(),
                    FnDef {
                        params: params.clone(),
                        defaults: defaults.clone(),
                        ret: ret.clone(),
                        body: own,
                        ptrs: node_ptrs(body),
                        line: *line,
                        col: *col,
                    },
                );
            }
        }
        Specializer {
            types,
            funcs,
            memo: HashMap::new(),
            per_fn: HashMap::new(),
            total: 0,
            next_id: 1,
            devirt: HashMap::new(),
            pending: Vec::new(),
            handed: 0,
            hoisted: Vec::new(),
            held: Vec::new(),
            globals,
            enabled,
        }
    }

    /// What is known of `e` inside a clone: the names this pass has seen the sandbox hold
    /// are globals there.
    fn known(&self, e: &Expr, env: &Env) -> Binding {
        binding_of(e, env, &|n| self.globals.contains(n))
    }

    fn note_globals(&mut self, b: &Binding) {
        match b {
            Binding::Global(g) => {
                self.globals.insert(g.clone());
            }
            Binding::Shape(fs) => {
                for (_, v) in fs {
                    self.note_globals(v);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether `name` is a function a clone can be made of.
    pub(crate) fn knows(&self, name: &str) -> bool {
        self.enabled && self.funcs.contains_key(name)
    }

    /// The statements made since the last call, for the sandbox to hoist.
    pub(crate) fn new_pending(&mut self) -> &[Stmt] {
        let from = self.handed;
        self.handed = self.pending.len();
        &self.pending[from..]
    }

    /// Every pending statement, to be appended to the program.
    pub(crate) fn take_pending(&mut self) -> Vec<Stmt> {
        self.handed = 0;
        std::mem::take(&mut self.pending)
    }

    pub(crate) fn take_hoisted(&mut self) -> Vec<Stmt> {
        std::mem::take(&mut self.hoisted)
    }

    pub(crate) fn take_held(&mut self) -> Vec<(String, Value)> {
        std::mem::take(&mut self.held)
    }

    /// Forget the checker's types of every node under `e` — it is about to be dropped, and
    /// the map is keyed by address, which a later node could reuse.
    pub(crate) fn forget(&mut self, e: &Expr) {
        forget_types(self.types, e);
    }

    /// Forget the types of nodes by address — a sandbox copy that is going away.
    pub(crate) fn forget_ptrs(&mut self, ptrs: &[*const Expr]) {
        for p in ptrs {
            self.types.remove(p);
        }
    }

    /// Give `copy`, a tree of `original`'s structure, `original`'s types; the copy's node
    /// addresses, to forget them by later.
    pub(crate) fn type_copy(&mut self, original: &Expr, copy: &Expr) -> Vec<*const Expr> {
        copy_types(self.types, &node_ptrs(original), copy);
        node_ptrs(copy)
    }

    /// What a call site outside any clone knows about `e`: `bound` are the names local to
    /// the site, `held` says whether the sandbox holds a top-level name.
    pub(crate) fn binding_at(&mut self, e: &Expr, bound: &[String], held: &dyn Fn(&str) -> bool) -> Binding {
        let mut env = Env::new();
        for b in bound {
            env.shadow(b);
        }
        let b = binding_of(e, &env, held);
        self.note_globals(&b);
        b
    }

    /// The clone of `fname` for `args`, made if it does not exist yet; `None` when nothing is
    /// known, the function is not the program's own, or a cap is reached.
    pub(crate) fn specialize(&mut self, fname: &str, args: &[Binding], depth: usize) -> Option<String> {
        // A scalar literal alone earns no clone — passing it costs ten nanoseconds, and a
        // library's own internal calls carry them everywhere. A shape or a held name does.
        if !self.enabled
            || depth > MAX_DEPTH
            || !args.iter().any(|b| matches!(b, Binding::Shape(_) | Binding::Global(_)))
        {
            return None;
        }
        let known_fn = self.funcs.get(fname)?;
        let (params_len, defaults) = (known_fn.params.len(), known_fn.defaults.clone());
        if args.len() > params_len {
            return None;
        }
        // A parameter the call leaves to its default is known exactly when the default is a
        // scalar literal.
        let mut known: Vec<Binding> = args.to_vec();
        for i in args.len()..params_len {
            known.push(match defaults.get(i) {
                Some(Some(d)) => Lit::from_expr(d).map(Binding::Lit).unwrap_or(Binding::Any),
                _ => Binding::Any,
            });
        }
        let key = (fname.to_string(), known.clone());
        if let Some(r) = self.memo.get(&key) {
            return r.clone();
        }
        let def = &self.funcs[fname];
        let capped = self.total >= MAX_CLONES
            || self.per_fn.get(fname).copied().unwrap_or(0) >= MAX_PER_FN
            || def.ptrs.len() > MAX_NODES;
        if capped {
            self.memo.insert(key, None);
            return None;
        }
        let name = format!("{fname}${}", self.next_id);
        self.next_id += 1;
        self.total += 1;
        *self.per_fn.entry(fname.to_string()).or_insert(0) += 1;
        // Memoized BEFORE the body is rewritten, so a recursive call inside it reaches the
        // clone itself.
        self.memo.insert(key, Some(name.clone()));
        let (params, defaults, ret, mut body, ptrs, line, col) = (
            def.params.clone(),
            def.defaults.clone(),
            def.ret.clone(),
            def.body.clone(),
            def.ptrs.clone(),
            def.line,
            def.col,
        );
        crate::fold::unshare_lambdas(&mut body);
        copy_types(self.types, &ptrs, &body);
        let mut env = Env::new();
        for ((p, _), b) in params.iter().zip(known.iter()) {
            env.bind(p, b.clone());
        }
        self.substitute(&mut body, &mut env, depth + 1);
        self.pending.push(Stmt::Func { name: name.clone(), params, defaults, ret, exported: false, body, line, col });
        Some(name)
    }

    /// The closure `fv`, held as field `field` of the top-level record `global`, as a
    /// top-level function of its own — its captured values hoisted to top-level bindings —
    /// so a call through the field becomes a direct call. The closure's body is the
    /// sandbox's copy, which carries the checker's types of the body it was copied from.
    pub(crate) fn devirtualize(&mut self, global: &str, field: &str, fv: &FuncVal) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let key = (global.to_string(), field.to_string());
        if let Some(r) = self.devirt.get(&key) {
            return r.clone();
        }
        let made = self.devirtualize_now(global, field, fv);
        self.devirt.insert(key, made.clone());
        made
    }

    fn devirtualize_now(&mut self, global: &str, field: &str, fv: &FuncVal) -> Option<String> {
        if self.total >= MAX_CLONES {
            return None;
        }
        let name = format!("{global}${field}");
        // Every captured value must have the literal the fold would write for it; a value
        // without one (a frame, a closure over a closure) leaves the call as it is.
        let mut hoisted = Vec::new();
        let mut held = Vec::new();
        let mut renames: HashMap<String, String> = HashMap::new();
        for (cname, cval) in fv.captured.iter() {
            let hname = format!("{name}${cname}");
            let mut budget = crate::fold::MAX_LITERAL_NODES;
            let lit = crate::fold::to_expr(cval, &mut budget)?;
            hoisted.push(Stmt::Assign { name: hname.clone(), mutable: false, exported: false, value: lit, line: 0, col: 0 });
            held.push((hname.clone(), cval.clone()));
            self.globals.insert(hname.clone());
            renames.insert(cname.clone(), hname);
        }
        let mut defaults: Vec<Option<Expr>> = Vec::new();
        let required = fv.params.len().saturating_sub(fv.defaults.len());
        defaults.resize(required, None);
        for d in fv.defaults.iter() {
            let mut budget = crate::fold::MAX_LITERAL_NODES;
            defaults.push(Some(crate::fold::to_expr(d, &mut budget)?));
        }
        let mut body: Expr = (*fv.body).clone();
        crate::fold::unshare_lambdas(&mut body);
        copy_types(self.types, &node_ptrs(&fv.body), &body);
        rename(&mut body, &renames, &mut Vec::new());
        let params: Vec<(String, Option<TypeAnn>)> = fv.params.iter().map(|p| (p.clone(), None)).collect();
        self.total += 1;
        self.funcs.insert(
            name.clone(),
            FnDef {
                params: params.clone(),
                defaults: defaults.clone(),
                ret: None,
                body: body.clone(),
                ptrs: node_ptrs(&body),
                line: 0,
                col: 0,
            },
        );
        self.pending.push(Stmt::Func { name: name.clone(), params, defaults, ret: None, exported: false, body, line: 0, col: 0 });
        self.hoisted.extend(hoisted);
        self.held.extend(held);
        Some(name)
    }

    /// Rewrite `e` for what `env` knows: answer the questions a shape or a constant answers,
    /// and specialize the calls that pass such knowledge on.
    fn substitute(&mut self, e: &mut Expr, env: &mut Env, depth: usize) {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing | Expr::Column { .. } => {}
            Expr::Ident { name, .. } => match env.lookup(name) {
                Some(Binding::Global(g)) => {
                    let g = g.clone();
                    if let Expr::Ident { name, .. } = e {
                        *name = g;
                    }
                }
                Some(Binding::Lit(l)) => *e = l.to_expr(),
                _ => {}
            },
            Expr::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(x, _) = p {
                        self.substitute(x, env, depth);
                    }
                }
            }
            Expr::Array(xs) | Expr::Tuple(xs) => {
                for x in xs {
                    self.substitute(x, env, depth);
                }
            }
            Expr::Record(fields) => {
                for (_, v) in fields {
                    self.substitute(v, env, depth);
                }
            }
            Expr::RecordUpdate { parts, .. } => {
                for p in parts {
                    self.substitute(p.expr_mut(), env, depth);
                }
            }
            Expr::Field { recv, name, .. } => {
                self.substitute(recv, env, depth);
                // A present key whose value the call site wrote as a literal or a held name.
                let known = self.known(recv, env);
                if let Some(fs) = known.shape() {
                    match fs.iter().find(|(k, _)| k == name).map(|(_, b)| b) {
                        Some(Binding::Lit(l)) => *e = l.to_expr(),
                        Some(Binding::Global(g)) => {
                            let (line, col) = crate::visit::expr_pos(e).unwrap_or((0, 0));
                            *e = Expr::Ident { name: g.clone(), line, col };
                        }
                        _ => {}
                    }
                }
            }
            Expr::FieldOrMissing { recv, name, line, col } => {
                self.substitute(recv, env, depth);
                let known = self.known(recv, env);
                if let Some(fs) = known.shape() {
                    let (line, col) = (*line, *col);
                    match fs.iter().find(|(k, _)| k == name).map(|(_, b)| b) {
                        None => *e = Expr::Missing,
                        Some(Binding::Lit(l)) => *e = l.to_expr(),
                        Some(Binding::Global(g)) => *e = Expr::Ident { name: g.clone(), line, col },
                        Some(_) => {
                            let name = name.clone();
                            let recv = std::mem::replace(&mut **recv, Expr::Missing);
                            *e = Expr::Field { recv: Box::new(recv), name, line, col };
                        }
                    }
                }
            }
            Expr::Unary { expr, .. } => self.substitute(expr, env, depth),
            Expr::Binary { op, left, right, .. } => {
                self.substitute(left, env, depth);
                self.substitute(right, env, depth);
                // A record literal is never `missing`.
                if matches!(op, crate::ast::BinOp::Coalesce) && self.known(left, env).shape().is_some() {
                    let taken = std::mem::replace(&mut **left, Expr::Missing);
                    *e = taken;
                }
            }
            Expr::Call { name, args, .. } => {
                for a in args.iter_mut() {
                    self.substitute(a, env, depth);
                }
                if name == "type_of" && args.len() == 1 && self.known(&args[0], env).shape().is_some() {
                    *e = Expr::Str("Record".to_string());
                    return;
                }
                if self.funcs.contains_key(name.as_str()) {
                    let bindings: Vec<Binding> = args.iter().map(|a| self.known(a, env)).collect();
                    if let Some(n) = self.specialize(name, &bindings, depth) {
                        *name = n;
                    }
                }
            }
            Expr::Method { recv, name, args, named, line, col, .. } => {
                self.substitute(recv, env, depth);
                // A frame verb reads its arguments as written: a name in them stays a name.
                if crate::interp::takes_unevaluated_args(name) {
                    return;
                }
                let mark = env.mark();
                env.shadow("it");
                for a in args.iter_mut() {
                    self.substitute(a, env, depth);
                }
                for (_, v) in named.iter_mut() {
                    self.substitute(v, env, depth);
                }
                env.truncate(mark);
                if !named.is_empty() {
                    return;
                }
                let known = self.known(recv, env);
                let Some(fs) = known.shape() else { return };
                let (line, col) = (*line, *col);
                let field_of = |recv: &Expr, k: &str| Expr::Field { recv: Box::new(recv.clone()), name: k.to_string(), line, col };
                // Only what a record ANSWERS: the methods its type owns, and the universal
                // `is_missing`. (A record has no `count`; `keys().count()` folds on its own.)
                match (name.as_str(), args.len()) {
                    ("keys", 0) => *e = Expr::Array(fs.iter().map(|(k, _)| Expr::Str(k.clone())).collect()),
                    ("is_missing", 0) => *e = Expr::Bool(false),
                    ("values", 0) => *e = Expr::Array(fs.iter().map(|(k, _)| field_of(recv, k)).collect()),
                    ("items", 0) => {
                        *e = Expr::Array(
                            fs.iter().map(|(k, _)| Expr::Tuple(vec![Expr::Str(k.clone()), field_of(recv, k)])).collect(),
                        )
                    }
                    ("has", 1) => {
                        if let Expr::Str(k) = &args[0] {
                            *e = Expr::Bool(fs.iter().any(|(f, _)| f == k));
                        }
                    }
                    ("get", 1) | ("expect", 1) => {
                        if let Expr::Str(k) = &args[0] {
                            match fs.iter().find(|(f, _)| f == k) {
                                Some((_, Binding::Lit(l))) => *e = l.to_expr(),
                                Some((_, Binding::Global(g))) => *e = Expr::Ident { name: g.clone(), line, col },
                                Some(_) => *e = field_of(recv, k),
                                None if name == "get" => *e = Expr::Missing,
                                None => {}
                            }
                        }
                    }
                    ("get", 2) => {
                        if let Expr::Str(k) = &args[0] {
                            match fs.iter().find(|(f, _)| f == k) {
                                Some((_, Binding::Lit(l))) => *e = l.to_expr(),
                                Some((_, Binding::Global(g))) => *e = Expr::Ident { name: g.clone(), line, col },
                                Some(_) => *e = field_of(recv, k),
                                None => {
                                    let d = std::mem::replace(&mut args[1], Expr::Missing);
                                    *e = d;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Expr::CallValue { callee, args, .. } => {
                self.substitute(callee, env, depth);
                for a in args {
                    self.substitute(a, env, depth);
                }
            }
            Expr::Index { recv, index, .. } => {
                self.substitute(recv, env, depth);
                self.substitute(index, env, depth);
            }
            Expr::Slice { recv, start, stop, step, .. } => {
                self.substitute(recv, env, depth);
                for part in [start, stop, step].into_iter().flatten() {
                    self.substitute(part, env, depth);
                }
            }
            Expr::Lambda { params, defaults, body, .. } => {
                for d in defaults.iter_mut() {
                    self.substitute(d, env, depth);
                }
                let mark = env.mark();
                for p in params.iter() {
                    env.shadow(p);
                }
                if let Some(b) = Rc::get_mut(body) {
                    self.substitute(b, env, depth);
                }
                env.truncate(mark);
            }
            Expr::Let { bindings, body, .. } => {
                let mark = env.mark();
                for (n, v) in bindings.iter_mut() {
                    self.substitute(v, env, depth);
                    let b = self.known(v, env);
                    env.bind(n, b);
                }
                self.substitute(body, env, depth);
                env.truncate(mark);
            }
            Expr::If { cond, then_branch, else_branch, .. } => {
                self.substitute(cond, env, depth);
                self.substitute(then_branch, env, depth);
                self.substitute(else_branch, env, depth);
            }
            Expr::Try { expr, .. } => self.substitute(expr, env, depth),
            Expr::Match { scrutinee, arms, .. } => {
                self.substitute(scrutinee, env, depth);
                for arm in arms.iter_mut() {
                    let mark = env.mark();
                    for n in crate::interp::pattern_binding_names(&arm.pattern) {
                        env.shadow(&n);
                    }
                    if let Some(g) = &mut arm.guard {
                        self.substitute(g, env, depth);
                    }
                    self.substitute(&mut arm.body, env, depth);
                    env.truncate(mark);
                }
            }
        }
    }
}

/// What is known of `e` under `env`; a top-level name outside `env` is a `Global` when the
/// sandbox holds it (`held`), and `Any` otherwise — a name that folds nothing keys nothing.
fn binding_of(e: &Expr, env: &Env, held: &dyn Fn(&str) -> bool) -> Binding {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => {
            Lit::from_expr(e).map(Binding::Lit).unwrap_or(Binding::Any)
        }
        Expr::Record(fields) => {
            if fields.len() > MAX_KEYS {
                return Binding::Any;
            }
            let mut seen: HashSet<&str> = HashSet::new();
            for (k, _) in fields {
                if !seen.insert(k.as_str()) {
                    return Binding::Any;
                }
            }
            Binding::Shape(fields.iter().map(|(k, v)| (k.clone(), binding_of(v, env, held))).collect())
        }
        Expr::Ident { name, .. } => match env.lookup(name) {
            Some(b) => b.clone(),
            None if held(name) => Binding::Global(name.clone()),
            None => Binding::Any,
        },
        Expr::Field { recv, name, .. } => match binding_of(recv, env, held) {
            Binding::Shape(fs) => fs.into_iter().find(|(k, _)| k == name).map(|(_, b)| b).unwrap_or(Binding::Any),
            _ => Binding::Any,
        },
        _ => Binding::Any,
    }
}

/// Rename free occurrences of the captured names in a closure body to their hoisted
/// top-level bindings; a binder of the same name inside the body shadows it.
fn rename(e: &mut Expr, map: &HashMap<String, String>, shadow: &mut Vec<String>) {
    match e {
        Expr::Ident { name, .. } => {
            if !shadow.iter().any(|s| s == name)
                && let Some(to) = map.get(name)
            {
                *name = to.clone();
            }
        }
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing | Expr::Column { .. } => {}
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    rename(x, map, shadow);
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter_mut().for_each(|x| rename(x, map, shadow)),
        Expr::Record(fields) => fields.iter_mut().for_each(|(_, v)| rename(v, map, shadow)),
        Expr::RecordUpdate { parts, .. } => parts.iter_mut().for_each(|p| rename(p.expr_mut(), map, shadow)),
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } | Expr::Unary { expr: recv, .. } | Expr::Try { expr: recv, .. } => {
            rename(recv, map, shadow)
        }
        Expr::Binary { left, right, .. } => {
            rename(left, map, shadow);
            rename(right, map, shadow);
        }
        Expr::Call { args, .. } => args.iter_mut().for_each(|a| rename(a, map, shadow)),
        Expr::Method { recv, args, named, .. } => {
            rename(recv, map, shadow);
            shadow.push("it".to_string());
            args.iter_mut().for_each(|a| rename(a, map, shadow));
            named.iter_mut().for_each(|(_, v)| rename(v, map, shadow));
            shadow.pop();
        }
        Expr::CallValue { callee, args, .. } => {
            rename(callee, map, shadow);
            args.iter_mut().for_each(|a| rename(a, map, shadow));
        }
        Expr::Index { recv, index, .. } => {
            rename(recv, map, shadow);
            rename(index, map, shadow);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            rename(recv, map, shadow);
            for part in [start, stop, step].into_iter().flatten() {
                rename(part, map, shadow);
            }
        }
        Expr::Lambda { params, defaults, body, .. } => {
            defaults.iter_mut().for_each(|d| rename(d, map, shadow));
            let mark = shadow.len();
            shadow.extend(params.iter().cloned());
            if let Some(b) = Rc::get_mut(body) {
                rename(b, map, shadow);
            }
            shadow.truncate(mark);
        }
        Expr::Let { bindings, body, .. } => {
            let mark = shadow.len();
            for (n, v) in bindings.iter_mut() {
                rename(v, map, shadow);
                shadow.push(n.clone());
            }
            rename(body, map, shadow);
            shadow.truncate(mark);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            rename(cond, map, shadow);
            rename(then_branch, map, shadow);
            rename(else_branch, map, shadow);
        }
        Expr::Match { scrutinee, arms, .. } => {
            rename(scrutinee, map, shadow);
            for arm in arms.iter_mut() {
                let mark = shadow.len();
                shadow.extend(crate::interp::pattern_binding_names(&arm.pattern));
                if let Some(g) = &mut arm.guard {
                    rename(g, map, shadow);
                }
                rename(&mut arm.body, map, shadow);
                shadow.truncate(mark);
            }
        }
    }
}

/// The addresses of every node under `e`, preorder — the order `walk_expr` fixes, so two
/// trees of one structure zip.
fn node_ptrs(e: &Expr) -> Vec<*const Expr> {
    let mut out = Vec::new();
    crate::visit::walk_expr(e, &mut |x| out.push(x as *const Expr));
    out
}

/// Give a clone of a typed tree the types of the tree it was cloned from.
fn copy_types(types: &mut TypeMap, original: &[*const Expr], clone: &Expr) {
    let mine = node_ptrs(clone);
    if mine.len() != original.len() {
        return;
    }
    for (o, c) in original.iter().zip(mine) {
        if let Some(t) = types.get(o).cloned() {
            types.insert(c, t);
        }
    }
}

pub(crate) fn forget_types(types: &mut TypeMap, e: &Expr) {
    crate::visit::walk_expr(e, &mut |x| {
        types.remove(&(x as *const Expr));
    });
}

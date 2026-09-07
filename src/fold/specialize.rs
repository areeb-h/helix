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
//! literal is the literal. The clone is then reduced as far as what is known reaches — this
//! is a partial evaluator over one function body. A sub-expression closed under the
//! sandbox (`"city".split_once(" ")`, `SEED.s == ""`, `m.columns.contains("city")` on the
//! held model) is evaluated where it stands; a literal condition selects its branch; a
//! `map` or a `reduce` over the small literal array a shape produces (`w.items()` on
//! `{city: …}` is one element) is unrolled, a lambda applied to known arguments becomes a
//! `let`, and a `let` bound to a tuple of safe elements answers `c[0]` and `c.count()`. What
//! remains is the work the runtime values genuinely need: for the field build's where
//! clause, one branch on whether the value is `missing`, and the parameter list — the text
//! `city = $1` is a constant. Nothing about the runtime values is assumed: a present key's
//! value is still read at run time, and may be `missing`.
//!
//! A method through a record the sandbox holds — `M.sql(spec)`, the object API a library
//! builds by closing over a model — is seen through the same way: the closure's body becomes
//! a top-level function of its own, its captured values top-level bindings (with the literal
//! the fold would write for them), and the call a direct one, which the shape rule then
//! specializes.
//!
//! THE TYPE MAP IS KEYED BY NODE ADDRESS, and the compiler routes a frame verb, and the
//! receiver-directed rewrite a method call, by what it says of a receiver. So a function's
//! types are snapshotted BY VALUE when the function is recorded, before any fold frees a
//! node of it, and a clone takes them from that snapshot; a body this pass copies takes the
//! types of the live body it was copied from; and every node this pass drops or replaces is
//! forgotten, the slot it occupied included. The first cut kept the original body's
//! addresses instead: a fold freed one, a later typed node was allocated there, and a
//! clone's `c` received that node's type — `c.count()` became the module's `count(c)`,
//! deterministically for one program and for no smaller one (the field build's §1.51).
//!
//! What keeps it bounded: clones are memoized per (function, knowledge), capped per function
//! and per program, and never made for a body past a size; the depth of transitive
//! specialization is capped; only a shape or a held name earns a clone (a scalar literal
//! alone costs ten nanoseconds to pass); unrolling stops at eight elements. A frame verb
//! reads its arguments as written, so nothing inside them is rewritten.
//! `HELIX_NOSPECIALIZE=1` turns the pass off for an A/B, as `HELIX_NOFOLD=1` turns off the
//! fold it rides on.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::{BinOp, Expr, InterpPart, Stmt, TypeAnn};
use crate::types::{Type, TypeMap};
use crate::value::{FuncVal, Value};

use super::{simplify, Outcome, Sandbox};

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
/// A literal array longer than this is not unrolled.
const MAX_UNROLL: usize = 8;

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

/// A name bound in the body being rewritten: what is known of it, and — for a `let` bound
/// to a tuple or array literal of safe elements — the literal itself, so `c[0]` is its
/// first element and `c.count()` its length.
struct Local {
    name: String,
    b: Binding,
    lit: Option<Expr>,
}

/// The names bound in the body being rewritten, innermost last.
struct Env {
    stack: Vec<Local>,
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
        self.bind_with(name, b, None);
    }
    /// Bind `name`; what earlier locals knew THROUGH that name is retired, since they would
    /// now read the new binding.
    fn bind_with(&mut self, name: &str, b: Binding, lit: Option<Expr>) {
        for l in &mut self.stack {
            if l.lit.as_ref().is_some_and(|e| mentions(e, name)) {
                l.lit = None;
            }
            if matches!(&l.b, Binding::Global(g) if g == name) {
                l.b = Binding::Any;
            }
        }
        self.stack.push(Local { name: name.to_string(), b, lit });
    }
    fn shadow(&mut self, name: &str) {
        self.bind(name, Binding::Any);
    }
    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.stack.iter().rev().find(|l| l.name == name).map(|l| &l.b)
    }
    fn literal_of(&self, name: &str) -> Option<&Expr> {
        self.stack.iter().rev().find(|l| l.name == name).and_then(|l| l.lit.as_ref())
    }
    fn names(&self) -> Vec<String> {
        self.stack.iter().map(|l| l.name.clone()).collect()
    }
}

/// One of the program's own functions, as it was before any fold: what a clone is made
/// from, and the checker's types of its nodes, snapshotted by value in the order
/// `walk_expr` fixes — never by address, which a fold may free and a later node reuse.
struct FnDef {
    params: Vec<(String, Option<TypeAnn>)>,
    defaults: Vec<Option<Expr>>,
    ret: Option<TypeAnn>,
    body: Expr,
    types: Vec<Option<Type>>,
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
    /// Top-level names that are globals inside a clone: the program's immutable bindings,
    /// and the ones this pass hoisted.
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
                // keep the fold from rewriting the program's own in place. Its types are
                // taken now, by value, while every node of the original is still alive.
                let mut own = body.clone();
                crate::fold::unshare_lambdas(&mut own);
                funcs.insert(
                    name.clone(),
                    FnDef {
                        params: params.clone(),
                        defaults: defaults.clone(),
                        ret: ret.clone(),
                        types: snapshot_types(types, body),
                        body: own,
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

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// The checker's types, for the fold's own rewrites.
    pub(crate) fn types(&mut self) -> &mut TypeMap {
        self.types
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

    /// Put `new` where `e` is, forgetting the types of what was there — the nodes the old
    /// expression still held, and the slot itself, which named the old node.
    pub(crate) fn set(&mut self, e: &mut Expr, new: Expr) {
        simplify::replace(e, new, self.types);
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
        copy_types(self.types, original, copy);
        node_ptrs(copy)
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
    /// known, the function is not the program's own, or a cap is reached. `sb` and `done`
    /// are the sandbox and the program's prefix, for what the clone evaluates as it is made.
    pub(crate) fn specialize(
        &mut self,
        fname: &str,
        args: &[Binding],
        depth: usize,
        sb: &mut Sandbox,
        done: &[Stmt],
    ) -> Option<String> {
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
            || def.types.len() > MAX_NODES;
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
        let (params, defaults, ret, mut body, snapshot, line, col) = (
            def.params.clone(),
            def.defaults.clone(),
            def.ret.clone(),
            def.body.clone(),
            def.types.clone(),
            def.line,
            def.col,
        );
        crate::fold::unshare_lambdas(&mut body);
        restore_types(self.types, &snapshot, &body);
        let mut env = Env::new();
        for ((p, _), b) in params.iter().zip(known.iter()) {
            env.bind(p, b.clone());
        }
        self.substitute(&mut body, &mut env, depth + 1, sb, done);
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
        let mut body = typed_clone(self.types, &fv.body);
        replace_idents(&mut body, &|n| renames.get(n).map(|to| Expr::Ident { name: to.clone(), line: 0, col: 0 }), &mut Vec::new());
        let params: Vec<(String, Option<TypeAnn>)> = fv.params.iter().map(|p| (p.clone(), None)).collect();
        self.total += 1;
        self.funcs.insert(
            name.clone(),
            FnDef {
                params: params.clone(),
                defaults: defaults.clone(),
                ret: None,
                types: snapshot_types(self.types, &body),
                body: body.clone(),
                line: 0,
                col: 0,
            },
        );
        self.pending.push(Stmt::Func { name: name.clone(), params, defaults, ret: None, exported: false, body, line: 0, col: 0 });
        self.hoisted.extend(hoisted);
        self.held.extend(held);
        Some(name)
    }

    /// Rewrite `e` for what `env` knows — children first — then reduce it as far as what is
    /// known reaches: the questions a shape or a constant answers, a comprehension over a
    /// literal array unrolled, a lambda applied to known arguments made a `let`, a literal
    /// condition's branch, and a sub-expression closed under the sandbox evaluated.
    fn substitute(&mut self, e: &mut Expr, env: &mut Env, depth: usize, sb: &mut Sandbox, done: &[Stmt]) {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing | Expr::Column { .. } => return,
            Expr::Ident { name, .. } => {
                match env.lookup(name) {
                    Some(Binding::Global(g)) => {
                        let g = g.clone();
                        if let Expr::Ident { name, .. } = e {
                            *name = g;
                        }
                    }
                    Some(Binding::Lit(l)) => {
                        let lit = l.to_expr();
                        self.set(e, lit);
                    }
                    _ => {}
                }
                return;
            }
            Expr::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(x, _) = p {
                        self.substitute(x, env, depth, sb, done);
                    }
                }
            }
            Expr::Array(xs) | Expr::Tuple(xs) => {
                for x in xs {
                    self.substitute(x, env, depth, sb, done);
                }
            }
            Expr::Record(fields) => {
                for (_, v) in fields {
                    self.substitute(v, env, depth, sb, done);
                }
            }
            Expr::RecordUpdate { parts, .. } => {
                for p in parts {
                    self.substitute(p.expr_mut(), env, depth, sb, done);
                }
            }
            Expr::Field { recv, name, .. } => {
                self.substitute(recv, env, depth, sb, done);
                // A present key whose value the call site wrote as a literal or a held name.
                let known = self.known(recv, env);
                if let Some(fs) = known.shape() {
                    match fs.iter().find(|(k, _)| k == name).map(|(_, b)| b) {
                        Some(Binding::Lit(l)) => {
                            let lit = l.to_expr();
                            self.set(e, lit);
                        }
                        Some(Binding::Global(g)) => {
                            let (line, col) = crate::visit::expr_pos(e).unwrap_or((0, 0));
                            let g = Expr::Ident { name: g.clone(), line, col };
                            self.set(e, g);
                        }
                        _ => {}
                    }
                }
            }
            Expr::FieldOrMissing { recv, name, line, col } => {
                self.substitute(recv, env, depth, sb, done);
                let known = self.known(recv, env);
                if let Some(fs) = known.shape() {
                    let (line, col) = (*line, *col);
                    let new = match fs.iter().find(|(k, _)| k == name).map(|(_, b)| b) {
                        None => Expr::Missing,
                        Some(Binding::Lit(l)) => l.to_expr(),
                        Some(Binding::Global(g)) => Expr::Ident { name: g.clone(), line, col },
                        Some(_) => {
                            let name = name.clone();
                            let recv = std::mem::replace(&mut **recv, Expr::Missing);
                            Expr::Field { recv: Box::new(recv), name, line, col }
                        }
                    };
                    self.set(e, new);
                }
            }
            Expr::Unary { expr, .. } => self.substitute(expr, env, depth, sb, done),
            Expr::Binary { op, left, right, .. } => {
                self.substitute(left, env, depth, sb, done);
                self.substitute(right, env, depth, sb, done);
                // A record literal is never `missing`.
                if matches!(op, BinOp::Coalesce) && self.known(left, env).shape().is_some() {
                    let taken = std::mem::replace(&mut **left, Expr::Missing);
                    self.set(e, taken);
                }
            }
            Expr::Call { name, args, .. } => {
                for a in args.iter_mut() {
                    self.substitute(a, env, depth, sb, done);
                }
                if name == "type_of" && args.len() == 1 && self.known(&args[0], env).shape().is_some() {
                    self.set(e, Expr::Str("Record".to_string()));
                    return;
                }
                if self.funcs.contains_key(name.as_str()) {
                    let bindings: Vec<Binding> = args.iter().map(|a| self.known(a, env)).collect();
                    if let Some(n) = self.specialize(name, &bindings, depth, sb, done) {
                        *name = n;
                    }
                }
            }
            Expr::Method { recv, name, args, named, line, col, .. } => {
                self.substitute(recv, env, depth, sb, done);
                // A frame verb reads its arguments as written: a name in them stays a name.
                if crate::interp::takes_unevaluated_args(name) {
                    return;
                }
                let mark = env.mark();
                env.shadow("it");
                for a in args.iter_mut() {
                    self.substitute(a, env, depth, sb, done);
                }
                for (_, v) in named.iter_mut() {
                    self.substitute(v, env, depth, sb, done);
                }
                env.truncate(mark);
                if !named.is_empty() {
                    return;
                }
                let (line, col) = (*line, *col);
                let known = self.known(recv, env);
                if let Some(fs) = known.shape() {
                    // Only what a record ANSWERS: the methods its type owns, and the
                    // universal `is_missing`. (A record has no `count`; `keys().count()`
                    // folds on its own.)
                    let field_of = |recv: &Expr, k: &str| Expr::Field { recv: Box::new(recv.clone()), name: k.to_string(), line, col };
                    let new = match (name.as_str(), args.len()) {
                        ("keys", 0) => Some(Expr::Array(fs.iter().map(|(k, _)| Expr::Str(k.clone())).collect())),
                        ("is_missing", 0) => Some(Expr::Bool(false)),
                        ("values", 0) => Some(Expr::Array(fs.iter().map(|(k, _)| field_of(recv, k)).collect())),
                        ("items", 0) => Some(Expr::Array(
                            fs.iter().map(|(k, _)| Expr::Tuple(vec![Expr::Str(k.clone()), field_of(recv, k)])).collect(),
                        )),
                        ("has", 1) => match &args[0] {
                            Expr::Str(k) => Some(Expr::Bool(fs.iter().any(|(f, _)| f == k))),
                            _ => None,
                        },
                        ("get", 1) | ("expect", 1) => match &args[0] {
                            Expr::Str(k) => match fs.iter().find(|(f, _)| f == k) {
                                Some((_, Binding::Lit(l))) => Some(l.to_expr()),
                                Some((_, Binding::Global(g))) => Some(Expr::Ident { name: g.clone(), line, col }),
                                Some(_) => Some(field_of(recv, k)),
                                None if name == "get" => Some(Expr::Missing),
                                None => None,
                            },
                            _ => None,
                        },
                        ("get", 2) => match &args[0] {
                            Expr::Str(k) => match fs.iter().find(|(f, _)| f == k) {
                                Some((_, Binding::Lit(l))) => Some(l.to_expr()),
                                Some((_, Binding::Global(g))) => Some(Expr::Ident { name: g.clone(), line, col }),
                                Some(_) => Some(field_of(recv, k)),
                                None => Some(std::mem::replace(&mut args[1], Expr::Missing)),
                            },
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(new) = new {
                        self.set(e, new);
                    }
                } else if let Some(items) = sequence_literal(recv, env) {
                    // A tuple or array literal of safe elements — written here, or bound by
                    // a `let` — answers its length, its ends, and unrolls a `map` or a
                    // `reduce` over it: what a shape's `items()` hands to a clause builder.
                    let n = items.len();
                    let is_array = matches!(recv_kind(recv, env), Some(Seq::Array));
                    let new = match (name.as_str(), args.len()) {
                        ("count", 0) | ("length", 0) => Some(Expr::Int(n as i64)),
                        ("first", 0) if n > 0 => Some(items[0].clone()),
                        ("last", 0) if n > 0 => Some(items[n - 1].clone()),
                        ("map", 1) if is_array && n <= MAX_UNROLL => unrolled_map(self.types, &args[0], &items).map(Expr::Array),
                        ("reduce", 2) if is_array && n <= MAX_UNROLL => unrolled_reduce(self.types, &args[0], &args[1], &items),
                        _ => None,
                    };
                    if let Some(new) = new {
                        let again = matches!(name.as_str(), "map" | "reduce" | "first" | "last");
                        self.set(e, new);
                        if again {
                            self.substitute(e, env, depth, sb, done);
                        }
                        return;
                    }
                }
            }
            Expr::CallValue { callee, args, .. } => {
                self.substitute(callee, env, depth, sb, done);
                for a in args.iter_mut() {
                    self.substitute(a, env, depth, sb, done);
                }
                // A lambda applied to arguments is the `let` that binds them — when no
                // argument reads a parameter bound before it.
                if let Expr::Lambda { params, defaults, body, .. } = &**callee
                    && defaults.is_empty()
                    && params.len() == args.len()
                    && args.iter().enumerate().all(|(i, a)| !params[..i].iter().any(|p| mentions(a, p)))
                {
                    let bindings: Vec<(String, Expr)> = params.iter().cloned().zip(std::mem::take(args)).collect();
                    let body = typed_clone(self.types, body);
                    self.set(e, Expr::Let { bindings, body: Box::new(body), from_do: false });
                    self.substitute(e, env, depth, sb, done);
                    return;
                }
            }
            Expr::Index { recv, index, .. } => {
                self.substitute(recv, env, depth, sb, done);
                self.substitute(index, env, depth, sb, done);
                if let Expr::Int(i) = **index
                    && let Some(items) = sequence_literal(recv, env)
                    && i >= 0
                    && (i as usize) < items.len()
                {
                    let elem = items[i as usize].clone();
                    self.set(e, elem);
                    self.substitute(e, env, depth, sb, done);
                    return;
                }
            }
            Expr::Slice { recv, start, stop, step, .. } => {
                self.substitute(recv, env, depth, sb, done);
                for part in [start, stop, step].into_iter().flatten() {
                    self.substitute(part, env, depth, sb, done);
                }
            }
            Expr::Lambda { params, defaults, body, .. } => {
                for d in defaults.iter_mut() {
                    self.substitute(d, env, depth, sb, done);
                }
                let mark = env.mark();
                for p in params.iter() {
                    env.shadow(p);
                }
                if let Some(b) = Rc::get_mut(body) {
                    self.substitute(b, env, depth, sb, done);
                }
                env.truncate(mark);
                return;
            }
            Expr::Let { bindings, body, .. } => {
                let mark = env.mark();
                for (n, v) in bindings.iter_mut() {
                    self.substitute(v, env, depth, sb, done);
                    let b = self.known(v, env);
                    let lit = if is_safe_sequence(v, env) { Some(v.clone()) } else { None };
                    env.bind_with(n, b, lit);
                }
                self.substitute(body, env, depth, sb, done);
                env.truncate(mark);
                // A binding nothing reads, whose value has nothing to run and cannot raise,
                // is dropped — the accumulator and the pair an unrolled `reduce` bound, once
                // the body has taken what it needed from them.
                let mut i = bindings.len();
                while i > 0 {
                    i -= 1;
                    let read = mentions(body, &bindings[i].0)
                        || bindings[i + 1..].iter().any(|(_, later)| mentions(later, &bindings[i].0));
                    if !read && is_safe(&bindings[i].1, env) {
                        let (_, dropped) = bindings.remove(i);
                        forget_types(self.types, &dropped);
                    }
                }
                if bindings.is_empty() {
                    let taken = std::mem::replace(&mut **body, Expr::Missing);
                    self.set(e, taken);
                }
                return;
            }
            Expr::If { cond, then_branch, else_branch, .. } => {
                self.substitute(cond, env, depth, sb, done);
                self.substitute(then_branch, env, depth, sb, done);
                self.substitute(else_branch, env, depth, sb, done);
            }
            Expr::Try { expr, .. } => self.substitute(expr, env, depth, sb, done),
            Expr::Match { scrutinee, arms, .. } => {
                self.substitute(scrutinee, env, depth, sb, done);
                for arm in arms.iter_mut() {
                    let mark = env.mark();
                    for n in crate::interp::pattern_binding_names(&arm.pattern) {
                        env.shadow(&n);
                    }
                    if let Some(g) = &mut arm.guard {
                        self.substitute(g, env, depth, sb, done);
                    }
                    self.substitute(&mut arm.body, env, depth, sb, done);
                    env.truncate(mark);
                }
            }
        }
        // A literal condition selects its branch.
        while simplify::constant(e, self.types) {}
        // A sub-expression closed under the sandbox — a method on a literal or a held
        // value, an operator on literals, a field of a held record, an interpolation of
        // held names — is evaluated where it stands. A call to one of the program's own
        // functions is not: that is a specialization, or the fold's, later.
        let bound = env.names();
        if let Some(label) = super::candidate_label(e, sb, &bound, done)
            && (label.starts_with('#') || label.contains('.'))
            && let Ok(Outcome::Value(v)) = sb.attempt(e, done)
        {
            let mut budget = crate::fold::MAX_LITERAL_NODES;
            if let Some(lit) = crate::fold::to_expr(&v, &mut budget) {
                self.set(e, lit);
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

#[derive(PartialEq)]
enum Seq {
    Array,
    Tuple,
}

/// The elements of the tuple or array literal `e` is — written, or bound to a `let` name —
/// when every element is safe to read more than once and in another place: a scalar
/// literal, a name, a present key of a shaped name, or a tuple or array of those.
fn sequence_literal(e: &Expr, env: &Env) -> Option<Vec<Expr>> {
    let seq = match e {
        Expr::Array(xs) | Expr::Tuple(xs) => xs,
        Expr::Ident { name, .. } => match env.literal_of(name)? {
            Expr::Array(xs) | Expr::Tuple(xs) => xs,
            _ => return None,
        },
        _ => return None,
    };
    if seq.iter().all(|x| is_safe(x, env)) { Some(seq.clone()) } else { None }
}

fn recv_kind(e: &Expr, env: &Env) -> Option<Seq> {
    match e {
        Expr::Array(_) => Some(Seq::Array),
        Expr::Tuple(_) => Some(Seq::Tuple),
        Expr::Ident { name, .. } => match env.literal_of(name)? {
            Expr::Array(_) => Some(Seq::Array),
            Expr::Tuple(_) => Some(Seq::Tuple),
            _ => None,
        },
        _ => None,
    }
}

/// A tuple or array literal whose every element is safe (see `sequence_literal`).
fn is_safe_sequence(e: &Expr, env: &Env) -> bool {
    matches!(e, Expr::Array(_) | Expr::Tuple(_)) && sequence_literal(e, env).is_some()
}

/// An expression that may be read more than once, and in another place, for the one it
/// stands in: it cannot raise and has nothing to run.
fn is_safe(e: &Expr, env: &Env) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing | Expr::Ident { .. } => true,
        Expr::Field { recv, name, .. } => match &**recv {
            Expr::Ident { name: p, .. } => {
                env.lookup(p).and_then(Binding::shape).is_some_and(|fs| fs.iter().any(|(k, _)| k == name))
            }
            _ => false,
        },
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().all(|x| is_safe(x, env)),
        Expr::Record(fields) => fields.iter().all(|(_, v)| is_safe(v, env)),
        _ => false,
    }
}

/// `[e1, …].map(f)` as the array of `f` applied to each: `f` is a one-parameter lambda or an
/// `it`-body.
fn unrolled_map(types: &mut TypeMap, f: &Expr, items: &[Expr]) -> Option<Vec<Expr>> {
    let (param, body): (&str, &Expr) = match f {
        Expr::Lambda { params, defaults, body, .. } if params.len() == 1 && defaults.is_empty() => (&params[0], body),
        Expr::Lambda { .. } => return None,
        it_body => ("it", it_body),
    };
    Some(items.iter().map(|x| substituted(types, body, param, x)).collect())
}

/// `[e1, …].reduce(init, (a, c) => body)` as the `let`s that run it: `let a = init, c = e1 in
/// body`, then that as the next `a` — when no element reads the accumulator's name.
fn unrolled_reduce(types: &mut TypeMap, init: &Expr, f: &Expr, items: &[Expr]) -> Option<Expr> {
    let Expr::Lambda { params, defaults, body, .. } = f else { return None };
    if params.len() != 2 || !defaults.is_empty() || items.iter().any(|x| mentions(x, &params[0])) {
        return None;
    }
    let mut acc = init.clone();
    for x in items {
        acc = Expr::Let {
            bindings: vec![(params[0].clone(), acc), (params[1].clone(), x.clone())],
            body: Box::new(typed_clone(types, body)),
            from_do: false,
        };
    }
    Some(acc)
}

/// `body` with every free occurrence of `name` replaced by `with`, carrying `body`'s types.
fn substituted(types: &mut TypeMap, body: &Expr, name: &str, with: &Expr) -> Expr {
    let mut out = typed_clone(types, body);
    replace_idents(&mut out, &|n| (n == name).then(|| with.clone()), &mut Vec::new());
    out
}

/// A copy of `body` — a live, typed tree — that owns its lambda bodies and carries the
/// checker's types of `body`'s nodes: the compiler routes a frame verb by its receiver's
/// type, and a copy that lost it would route generically.
fn typed_clone(types: &mut TypeMap, body: &Expr) -> Expr {
    let mut out = body.clone();
    crate::fold::unshare_lambdas(&mut out);
    copy_types(types, body, &out);
    out
}

/// Whether `e` mentions the name `name` at all (a binder of it inside counts too — a safe
/// over-approximation of "reads it").
fn mentions(e: &Expr, name: &str) -> bool {
    let mut found = false;
    crate::visit::walk_expr(e, &mut |x| {
        if let Expr::Ident { name: n, .. } = x
            && n == name
        {
            found = true;
        }
    });
    found
}

/// Replace free identifiers under `e` by what `with` gives for their name; a binder of the
/// same name inside `e` shadows it.
fn replace_idents(e: &mut Expr, with: &dyn Fn(&str) -> Option<Expr>, shadow: &mut Vec<String>) {
    match e {
        Expr::Ident { name, .. } => {
            if !shadow.iter().any(|s| s == name)
                && let Some(to) = with(name)
            {
                *e = to;
            }
        }
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing | Expr::Column { .. } => {}
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    replace_idents(x, with, shadow);
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter_mut().for_each(|x| replace_idents(x, with, shadow)),
        Expr::Record(fields) => fields.iter_mut().for_each(|(_, v)| replace_idents(v, with, shadow)),
        Expr::RecordUpdate { parts, .. } => parts.iter_mut().for_each(|p| replace_idents(p.expr_mut(), with, shadow)),
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } | Expr::Unary { expr: recv, .. } | Expr::Try { expr: recv, .. } => {
            replace_idents(recv, with, shadow)
        }
        Expr::Binary { left, right, .. } => {
            replace_idents(left, with, shadow);
            replace_idents(right, with, shadow);
        }
        Expr::Call { args, .. } => args.iter_mut().for_each(|a| replace_idents(a, with, shadow)),
        Expr::Method { recv, args, named, .. } => {
            replace_idents(recv, with, shadow);
            shadow.push("it".to_string());
            args.iter_mut().for_each(|a| replace_idents(a, with, shadow));
            named.iter_mut().for_each(|(_, v)| replace_idents(v, with, shadow));
            shadow.pop();
        }
        Expr::CallValue { callee, args, .. } => {
            replace_idents(callee, with, shadow);
            args.iter_mut().for_each(|a| replace_idents(a, with, shadow));
        }
        Expr::Index { recv, index, .. } => {
            replace_idents(recv, with, shadow);
            replace_idents(index, with, shadow);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            replace_idents(recv, with, shadow);
            for part in [start, stop, step].into_iter().flatten() {
                replace_idents(part, with, shadow);
            }
        }
        Expr::Lambda { params, defaults, body, .. } => {
            defaults.iter_mut().for_each(|d| replace_idents(d, with, shadow));
            let mark = shadow.len();
            shadow.extend(params.iter().cloned());
            if let Some(b) = Rc::get_mut(body) {
                replace_idents(b, with, shadow);
            }
            shadow.truncate(mark);
        }
        Expr::Let { bindings, body, .. } => {
            let mark = shadow.len();
            for (n, v) in bindings.iter_mut() {
                replace_idents(v, with, shadow);
                shadow.push(n.clone());
            }
            replace_idents(body, with, shadow);
            shadow.truncate(mark);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            replace_idents(cond, with, shadow);
            replace_idents(then_branch, with, shadow);
            replace_idents(else_branch, with, shadow);
        }
        Expr::Match { scrutinee, arms, .. } => {
            replace_idents(scrutinee, with, shadow);
            for arm in arms.iter_mut() {
                let mark = shadow.len();
                shadow.extend(crate::interp::pattern_binding_names(&arm.pattern));
                if let Some(g) = &mut arm.guard {
                    replace_idents(g, with, shadow);
                }
                replace_idents(&mut arm.body, with, shadow);
                shadow.truncate(mark);
            }
        }
    }
}

/// The addresses of every node under `e`, preorder — the order `walk_expr` fixes, so two
/// trees of one structure zip.
pub(super) fn node_ptrs(e: &Expr) -> Vec<*const Expr> {
    let mut out = Vec::new();
    crate::visit::walk_expr(e, &mut |x| out.push(x as *const Expr));
    out
}

/// The checker's types of every node under `e`, by value, in `walk_expr`'s order — taken
/// while every node is alive, so no address in it can be a reused one.
fn snapshot_types(types: &TypeMap, e: &Expr) -> Vec<Option<Type>> {
    node_ptrs(e).iter().map(|p| types.get(p).cloned()).collect()
}

/// Give `tree`, of the structure a snapshot was taken from, the snapshot's types — all but
/// the root's: a root is inline in the statement, the box or the slot that holds it, and
/// moves with it, while every other node is where its parent's box or vector put it. A
/// root is never a receiver, so nothing reads its type.
fn restore_types(types: &mut TypeMap, snapshot: &[Option<Type>], tree: &Expr) {
    let mine = node_ptrs(tree);
    if mine.len() != snapshot.len() {
        return;
    }
    for (p, t) in mine.into_iter().zip(snapshot).skip(1) {
        if let Some(t) = t {
            types.insert(p, t.clone());
        }
    }
}

/// Give `copy`, a tree of `original`'s structure, `original`'s types — `original` alive.
fn copy_types(types: &mut TypeMap, original: &Expr, copy: &Expr) {
    let snapshot = snapshot_types(types, original);
    restore_types(types, &snapshot, copy);
}

pub(crate) fn forget_types(types: &mut TypeMap, e: &Expr) {
    crate::visit::walk_expr(e, &mut |x| {
        types.remove(&(x as *const Expr));
    });
}

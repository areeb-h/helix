//! Specialization at load time (ADR 0051): a function compiled for what its call site knows.
//!
//! A call site often knows more than "a record": it knows the record's KEYS, because it
//! wrote the literal — `sql(m, {where: {city: req.city}, limit: 10})` — even when the values
//! come from a request. A function reading such a record asks it questions the shape alone
//! answers: `spec.limit?` is `missing` when there is no `limit` key, `spec.keys()` is a
//! constant, `type_of(spec)` is `"Record"`. And a call site often passes a CONSTANT — a
//! top-level name the sandbox holds, a scalar literal — that the callee reads field by field.
//! An array literal is knowledge of the same kind: `{order: ["-age"]}` says how many
//! elements `order` has and what each one is, and a comprehension over it inside the callee
//! — `order.map(_ord1(m, it))`, `any_of.reduce(…)` — is unrolled over them, each element's
//! own knowledge (a literal, a record's shape) reaching the lambda's body and, through it,
//! the callees it calls: the same idea one level down.
//!
//! So a call whose arguments carry any of that is rewritten to call a CLONE of the callee,
//! made once per function and per what is known (`sql$3`), in which every such question is
//! answered in place: the absent key is `missing`, the constant name is the global, the
//! literal is the literal. The clone is then reduced as far as what is known reaches — this
//! is a partial evaluator over one function body. A sub-expression closed under the
//! sandbox (`"city".split_once(" ")`, `SEED.s == ""`, `m.columns.contains("city")` on the
//! held model) is evaluated where it stands; a literal condition selects its branch; a
//! `map` or a `reduce` over the small literal array a shape produces (`w.items()` on
//! `{city: …}` is one element) or a call site wrote is unrolled, a lambda applied to known
//! arguments becomes a `let`, and a `let` bound to a tuple of safe elements answers `c[0]`
//! and `c.count()`. What remains is the work the runtime values genuinely need: for the
//! field build's where clause, one branch on whether the value is `missing`, and the
//! parameter list — the text `city = $1` is a constant. Nothing about the runtime values
//! is assumed: a present key's value is still read at run time, and may be `missing`.
//!
//! A clone that reduces to one of its parameters, or to a scalar literal, is not a function
//! worth calling: the call site becomes the argument, or the literal, when its other
//! arguments have nothing to run. That is what a validating wrapper — `_wants_rec("page",
//! p, eg)`, `if type_of(v) == "Record" then v else raise(…)` — becomes for a record literal:
//! nothing, so what it wrapped is seen through.
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
//! What keeps it bounded: clones are memoized per (function, knowledge); a clone costs its
//! body's size against one budget for the program, and a clone in which nothing was reduced
//! is not kept; a body past a size is never cloned; the depth of transitive specialization
//! is capped; only a shape, a held name or an array literal earns a clone (a scalar literal
//! alone costs ten nanoseconds to pass); unrolling stops at eight elements. A frame verb
//! reads its arguments as written, so nothing inside them is rewritten.
//! `HELIX_NOSPECIALIZE=1` turns the pass off for an A/B, as `HELIX_NOFOLD=1` turns off the
//! fold it rides on; `HELIX_FOLD_DUMP` prints what the pass made.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::{BinOp, Expr, InterpPart, Stmt, TypeAnn};
use crate::types::{Type, TypeMap};
use crate::value::{FuncVal, Value};

use super::{simplify, Sandbox};

/// How far a specialization may follow calls into callees.
const MAX_DEPTH: usize = 4;
/// A body larger than this (in nodes) is not cloned.
const MAX_NODES: usize = 4_096;
/// The nodes all of a program's clones may hold together: a clone costs its body's size,
/// so a small helper's clone costs little and a large function's much — sixty-four of the
/// largest body allowed. The first cut counted clones instead, sixty-four per program and
/// eight per function, and the field build's harness — thirteen cases in one file —
/// starved its later call sites: `page offset` ran through the generic clone at 3.7 µs
/// while the same call alone ran at 1.3 µs (§1.50). A budget by size is what the cost
/// actually is.
const MAX_CLONE_NODES: usize = 64 * MAX_NODES;
/// The most clones one program may hold — a sanity bound behind the budget.
const MAX_CLONES: usize = 1_024;
/// A record with more keys than this is `Any`.
const MAX_KEYS: usize = 32;
/// A literal array longer than this is not unrolled, and is `Any` as knowledge.
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

    pub(super) fn to_expr(&self) -> Expr {
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
    /// An array literal of at most `MAX_UNROLL` elements: what is known of each.
    Seq(Vec<Binding>),
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

    fn seq(&self) -> Option<&[Binding]> {
        match self {
            Binding::Seq(xs) => Some(xs),
            _ => None,
        }
    }

    /// Whether the knowledge names `name` anywhere — a held global a rebinding retires.
    fn reads(&self, name: &str) -> bool {
        match self {
            Binding::Global(g) => g == name,
            Binding::Shape(fs) => fs.iter().any(|(_, v)| v.reads(name)),
            Binding::Seq(xs) => xs.iter().any(|x| x.reads(name)),
            _ => false,
        }
    }

    /// Whether a clone is worth making for this: a shape, a held name or an array literal
    /// answers questions; a scalar literal alone does not.
    fn earns_clone(&self) -> bool {
        matches!(self, Binding::Shape(_) | Binding::Global(_) | Binding::Seq(_))
    }

    /// Whether `part` is what one of this knowledge's parts is — a shape's field, a
    /// sequence's element, at any depth. A recursion that passes such a part down
    /// (`render(p.left, n)`) shrinks what it knows, and ends.
    fn contains(&self, part: &Binding) -> bool {
        match self {
            Binding::Shape(fs) => fs.iter().any(|(_, v)| v == part || v.contains(part)),
            Binding::Seq(xs) => xs.iter().any(|x| x == part || x.contains(part)),
            _ => false,
        }
    }
}

/// What `specialize` made of a call: a clone to call, or nothing worth calling — the
/// clone reduced to one of its parameters, or to a literal, or to a record or array
/// literal whose leaves have nothing to run (a constructor's result, a clause builder's
/// `{s, n, ps}` with the value's field read in `ps`), and the call site becomes that, with
/// the arguments in for the parameters.
#[derive(Clone, Debug)]
pub(crate) enum Made {
    Fn(String),
    Param(usize),
    Inline { params: Vec<String>, body: Expr },
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
            if l.b.reads(name) {
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
    memo: HashMap<(String, Vec<Binding>), Option<Made>>,
    total: usize,
    /// The nodes the clones made so far hold, against `budget`.
    nodes: usize,
    budget: usize,
    /// Whether the clone being made has been changed by what is known — set by every
    /// rewrite; a clone nothing changed is the function under another name, and not kept.
    changed: bool,
    /// The clones being made right now, outermost first — the functions on the
    /// specialization stack, with what each is being made for. A call to one of them is a
    /// recursion, and what it passes down is measured against this.
    building: Vec<(String, Vec<Binding>)>,
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
            total: 0,
            nodes: 0,
            budget: MAX_CLONE_NODES,
            changed: false,
            building: Vec::new(),
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

    /// The nodes all clones may hold together — the tests shrink it.
    pub(crate) fn set_budget(&mut self, nodes: usize) {
        self.budget = nodes;
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
        self.changed = true;
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
        binding_of(e, env, &self.globals)
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
            Binding::Seq(xs) => {
                for x in xs {
                    self.note_globals(x);
                }
            }
            _ => {}
        }
    }

    /// Reduce `e` — a body inlined at a call site outside any clone — by the rules a clone's
    /// body gets: a field of a record literal is its expression, a `let` alias its name, a
    /// closed sub-expression its value. `bound` are the names local to the site.
    pub(crate) fn reduce(&mut self, e: &mut Expr, bound: &[String], sb: &mut Sandbox, done: &[Stmt]) {
        let mut env = Env::new();
        for b in bound {
            env.shadow(b);
        }
        self.substitute(e, &mut env, 0, sb, done);
    }

    /// What a call site outside any clone knows about `e`: `bound` are the names local to
    /// the site, `held` says whether the sandbox holds a top-level name.
    pub(crate) fn binding_at(&mut self, e: &Expr, bound: &[String], held: &dyn Fn(&str) -> bool) -> Binding {
        let mut env = Env::new();
        for b in bound {
            env.shadow(b);
        }
        let mut globals = HashSet::new();
        crate::visit::walk_expr(e, &mut |x| {
            if let Expr::Ident { name, .. } = x
                && held(name)
            {
                globals.insert(name.clone());
            }
        });
        let b = binding_of(e, &env, &globals);
        self.note_globals(&b);
        b
    }

    /// What `fname` becomes for `args`: a clone, made if it does not exist yet, or the
    /// parameter or literal the clone reduced to; `None` when nothing is known, the
    /// function is not the program's own, or a cap is reached. `sb` and `done` are the
    /// sandbox and the program's prefix, for what the clone evaluates as it is made.
    pub(crate) fn specialize(
        &mut self,
        fname: &str,
        args: &[Binding],
        depth: usize,
        sb: &mut Sandbox,
        done: &[Stmt],
    ) -> Option<Made> {
        // A scalar literal alone earns no clone — passing it costs ten nanoseconds, and a
        // library's own internal calls carry them everywhere. A shape, a held name or an
        // array literal does.
        if !self.enabled || depth > MAX_DEPTH || !args.iter().any(Binding::earns_clone) {
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
        // A RECURSION IS SPECIALIZED ONCE PER CHAIN. A call to a function whose clone is
        // being made right now — `_tk(st, i + 1, acc.concat([tok]))` inside `_tk`'s own
        // body — carries knowledge that changed along the recursion: the index one literal
        // higher, the accumulator one element longer. Every level would earn a clone of
        // its own, a chain of them until the budget ran out (the field build's tokenizer:
        // 390 clones of `_tk`, 624 of `_scan_str`, 0.6 s to load a 100-line file — §1.60,
        // the checker's §1.48 in the load path). What the recursion passes down is measured
        // against what the ancestor was made for: knowledge that SHRINKS — a part of the
        // ancestor's shape, `render(p.left, n)` — is kept, since a finite structure ends;
        // knowledge that grows or merely changes is generalized to `Any`, and the chain
        // reaches a clone that recurses into itself.
        if let Some((_, ancestor)) = self.building.iter().rev().find(|(f, _)| f == fname) {
            // Any position that shrinks puts the whole call on a finite path: the
            // renderer's `render(p.right, l.n)` passes a subtree AND a counter one higher,
            // and the counter is exactly what its text needs; the subtree is what ends it.
            let shrinks = known.iter().zip(ancestor.iter()).any(|(now, was)| now != was && was.contains(now));
            if !shrinks {
                let general: Vec<Binding> =
                    known.iter().zip(ancestor.iter()).map(|(now, was)| if now == was { now.clone() } else { Binding::Any }).collect();
                if general != known {
                    return self.specialize(fname, &general, depth, sb, done);
                }
            }
        }
        let def = &self.funcs[fname];
        // A clone costs its body's size, against the program's budget.
        let size = def.types.len();
        if self.total >= MAX_CLONES || size > MAX_NODES || self.nodes + size > self.budget {
            self.memo.insert(key, None);
            return None;
        }
        let name = format!("{fname}${}", self.next_id);
        self.next_id += 1;
        self.total += 1;
        self.nodes += size;
        // Memoized BEFORE the body is rewritten, so a recursive call inside it reaches the
        // clone itself.
        self.memo.insert(key.clone(), Some(Made::Fn(name.clone())));
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
        let outer = std::mem::replace(&mut self.changed, false);
        self.building.push((fname.to_string(), known.clone()));
        self.substitute(&mut body, &mut env, depth + 1, sb, done);
        self.building.pop();
        let reduced = std::mem::replace(&mut self.changed, outer);
        // A body that is one of its parameters, a literal, or a record or array literal
        // whose leaves have nothing to run, is not a function worth calling: the call site
        // becomes that.
        let trivial = match &body {
            Expr::Ident { name: n, .. } => params.iter().position(|(p, _)| p == n).map(Made::Param),
            Expr::Record(_) | Expr::Array(_) | Expr::Tuple(_) if is_safe(&body, &env, &self.globals) => Some(Made::Inline {
                params: params.iter().map(|(p, _)| p.clone()).collect(),
                body: body.clone(),
            }),
            lit if simplify::is_literal(lit) => Some(Made::Inline { params: Vec::new(), body: lit.clone() }),
            _ => None,
        };
        if let Some(made) = trivial {
            forget_types(self.types, &body);
            self.memo.insert(key, Some(made.clone()));
            self.total -= 1;
            self.nodes -= size;
            return Some(made);
        }
        if !reduced {
            // Nothing in the body answered to what was known — it reads only what the
            // runtime values decide. The clone would be the function under another name:
            // not kept, and the budget it took is returned.
            forget_types(self.types, &body);
            self.memo.insert(key, None);
            self.total -= 1;
            self.nodes -= size;
            return None;
        }
        self.pending.push(Stmt::Func { name: name.clone(), params, defaults, ret, exported: false, body, line, col });
        Some(Made::Fn(name))
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
        let size = node_ptrs(&body).len();
        if size > MAX_NODES || self.nodes + size > self.budget {
            forget_types(self.types, &body);
            return None;
        }
        replace_idents(&mut body, &|n| renames.get(n).map(|to| Expr::Ident { name: to.clone(), line: 0, col: 0 }), &mut Vec::new());
        let params: Vec<(String, Option<TypeAnn>)> = fv.params.iter().map(|p| (p.clone(), None)).collect();
        self.total += 1;
        self.nodes += size;
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
                        self.changed = true;
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
                // A field of a record literal written here — what an inlined call leaves,
                // `{city: v}.city` — is that field's expression, when the fields dropped
                // with the record have nothing to run.
                if let Expr::Record(fs) = &**recv
                    && let Some(pos) = fs.iter().position(|(k, _)| k == name)
                    && fs.iter().enumerate().all(|(i, (_, v))| i == pos || is_safe(v, env, &self.globals))
                {
                    let val = fs[pos].1.clone();
                    self.set(e, val);
                    self.substitute(e, env, depth, sb, done);
                    return;
                }
                // A field of a name a `let` bound to a record literal whose leaves have
                // nothing to run — an inlined clause builder's `{s, n, ps}` — is that field's
                // expression, read where the field was; the binding goes once nothing reads it.
                if let Expr::Ident { name: rn, .. } = &**recv
                    && let Some(Expr::Record(fs)) = env.literal_of(rn)
                    && let Some((_, val)) = fs.iter().find(|(k, _)| k == name)
                {
                    let val = val.clone();
                    self.set(e, val);
                    self.substitute(e, env, depth, sb, done);
                    return;
                }
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
                // A record or array literal is never `missing`.
                if matches!(op, BinOp::Coalesce) && matches!(self.known(left, env), Binding::Shape(_) | Binding::Seq(_)) {
                    let taken = std::mem::replace(&mut **left, Expr::Missing);
                    self.set(e, taken);
                }
            }
            Expr::Call { name, args, .. } => {
                for a in args.iter_mut() {
                    self.substitute(a, env, depth, sb, done);
                }
                if name == "type_of" && args.len() == 1 {
                    let answer = match self.known(&args[0], env) {
                        Binding::Shape(_) => Some("Record"),
                        Binding::Seq(_) => Some("Array"),
                        _ => None,
                    };
                    if let Some(t) = answer {
                        self.set(e, Expr::Str(t.to_string()));
                        return;
                    }
                }
                if self.funcs.contains_key(name.as_str()) {
                    let bindings: Vec<Binding> = args.iter().map(|a| self.known(a, env)).collect();
                    match self.specialize(name, &bindings, depth, sb, done) {
                        Some(Made::Fn(n)) => {
                            *name = n;
                            self.changed = true;
                        }
                        // The clone is its parameter, or a literal: the call is that — when
                        // the arguments it drops have nothing to run.
                        Some(Made::Param(i))
                            if i < args.len() && args.iter().enumerate().all(|(j, a)| j == i || is_safe(a, env, &self.globals)) =>
                        {
                            let taken = std::mem::replace(&mut args[i], Expr::Missing);
                            self.set(e, taken);
                        }
                        Some(Made::Inline { params, body }) if args.iter().all(|a| is_safe(a, env, &self.globals)) => {
                            let mut inl = body;
                            let args: Vec<Expr> = std::mem::take(args);
                            replace_idents(&mut inl, &|n| params.iter().position(|p| p == n).map(|i| args[i].clone()), &mut Vec::new());
                            self.set(e, inl);
                            self.substitute(e, env, depth, sb, done);
                            return;
                        }
                        _ => {}
                    }
                }
            }
            Expr::Method { recv, name, args, named, line, col, .. } => {
                self.substitute(recv, env, depth, sb, done);
                // A frame verb reads its arguments as written: a name in them stays a name,
                // so nothing inside them is rewritten. The receiver's rules below still
                // apply — they fire only for a receiver known to be a record or an array
                // literal, which no frame verb reaches: `c.count()` is the count of the
                // known sequence, `ords.join(", ")` the join over the literal.
                if !crate::interp::takes_unevaluated_args(name) {
                    let mark = env.mark();
                    if binds_it(name) {
                        env.shadow("it");
                    }
                    for a in args.iter_mut() {
                        self.substitute(a, env, depth, sb, done);
                    }
                    for (_, v) in named.iter_mut() {
                        self.substitute(v, env, depth, sb, done);
                    }
                    env.truncate(mark);
                }
                if !named.is_empty() {
                    return;
                }
                let (line, col) = (*line, *col);
                let known = self.known(recv, env);
                if let Some(fs) = known.shape() {
                    // Only what a record ANSWERS: the methods its type owns, and the
                    // universal `is_missing`. (A record has no `count`; `keys().count()`
                    // folds on its own.) A value the call site wrote as a literal is the
                    // literal; any other is a field read.
                    let value_of = |recv: &Expr, k: &str, b: &Binding| match b {
                        Binding::Lit(l) => l.to_expr(),
                        _ => Expr::Field { recv: Box::new(recv.clone()), name: k.to_string(), line, col },
                    };
                    let new = match (name.as_str(), args.len()) {
                        ("keys", 0) => Some(Expr::Array(fs.iter().map(|(k, _)| Expr::Str(k.clone())).collect())),
                        ("is_missing", 0) => Some(Expr::Bool(false)),
                        ("values", 0) => Some(Expr::Array(fs.iter().map(|(k, b)| value_of(recv, k, b)).collect())),
                        ("items", 0) => Some(Expr::Array(
                            fs.iter().map(|(k, b)| Expr::Tuple(vec![Expr::Str(k.clone()), value_of(recv, k, b)])).collect(),
                        )),
                        ("has", 1) => match &args[0] {
                            Expr::Str(k) => Some(Expr::Bool(fs.iter().any(|(f, _)| f == k))),
                            _ => None,
                        },
                        ("get", 1) | ("expect", 1) => match &args[0] {
                            Expr::Str(k) => match fs.iter().find(|(f, _)| f == k) {
                                Some((_, Binding::Global(g))) => Some(Expr::Ident { name: g.clone(), line, col }),
                                Some((_, b)) => Some(value_of(recv, k, b)),
                                None if name == "get" => Some(Expr::Missing),
                                None => None,
                            },
                            _ => None,
                        },
                        ("get", 2) => match &args[0] {
                            Expr::Str(k) => match fs.iter().find(|(f, _)| f == k) {
                                Some((_, Binding::Global(g))) => Some(Expr::Ident { name: g.clone(), line, col }),
                                Some((_, b)) => Some(value_of(recv, k, b)),
                                None => Some(std::mem::replace(&mut args[1], Expr::Missing)),
                            },
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(new) = new {
                        self.set(e, new);
                        return;
                    }
                } else if let Some(items) = sequence_literal(recv, env, &self.globals) {
                    // A tuple or array literal of safe elements — written here, bound by a
                    // `let`, or known from the call site — answers its length, its ends,
                    // and unrolls a `map` or a `reduce` over it: what a shape's `items()`
                    // hands to a clause builder, what `{order: ["-age"]}` hands to `order`.
                    let n = items.len();
                    let is_array = matches!(recv_kind(recv, env, &self.globals), Some(Seq::Array));
                    let new = match (name.as_str(), args.len()) {
                        ("count", 0) | ("length", 0) => Some(Expr::Int(n as i64)),
                        ("is_missing", 0) => Some(Expr::Bool(false)),
                        ("first", 0) if n > 0 => Some(items[0].clone()),
                        ("last", 0) if n > 0 => Some(items[n - 1].clone()),
                        ("map", 1) if is_array && n <= MAX_UNROLL => unrolled_map(self.types, &args[0], &items).map(Expr::Array),
                        ("reduce", 2) if is_array && n <= MAX_UNROLL => unrolled_reduce(self.types, &args[0], &args[1], &items),
                        // Two known arrays concatenate to one — a clause builder's `l.ps.concat(r.ps)`.
                        ("concat", 1) if is_array && matches!(recv_kind(&args[0], env, &self.globals), Some(Seq::Array)) => {
                            sequence_literal(&args[0], env, &self.globals)
                                .filter(|rhs| items.len() + rhs.len() <= MAX_UNROLL)
                                .map(|rhs| Expr::Array(items.iter().chain(rhs.iter()).cloned().collect()))
                        }
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
                // A method on a name bound to a literal — `ords.join(", ")` on the array the
                // unrolled map produced — is the method on the literal, evaluated where the
                // sandbox can: the arguments closed, the receiver known here if not there.
                if let Some(lit) = literal_receiver(recv, env, &self.globals) {
                    let probe = Expr::Method { recv: Box::new(lit), name: name.clone(), args: args.clone(), named: Vec::new(), ufcs: None, line, col };
                    let bound = env.names();
                    if let Some(v) = super::closed_literal(&probe, sb, &bound, done) {
                        self.set(e, v);
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
                    && let Some(items) = sequence_literal(recv, env, &self.globals)
                    && i >= 0
                    && (i as usize) < items.len()
                {
                    // An element known from the call site is `recv[i]` already: nothing to
                    // rewrite, and the knowledge of it reads through `binding_of`.
                    if matches!(items[i as usize], Expr::Index { .. }) {
                        return;
                    }
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
                let mut i = 0;
                while i < bindings.len() {
                    self.substitute(&mut bindings[i].1, env, depth, sb, done);
                    // An alias — `let p = q` (the destructuring desugar's own `$rec0 = spec`,
                    // a validating wrapper seen through) — is the name it aliases: `q` is
                    // read where `p` was, and the binding goes. Not where a later binding
                    // or the body rebinds either name, which would change what is read.
                    if let Expr::Ident { name: q, line, col } = &bindings[i].1
                        && *q != bindings[i].0
                        && !binds(body, q)
                        && !bindings[i + 1..].iter().any(|(n2, v2)| n2 == q || n2 == &bindings[i].0 || binds(v2, q))
                    {
                        let (p, q, line, col) = (bindings[i].0.clone(), q.clone(), *line, *col);
                        let to = |n: &str| (n == p).then(|| Expr::Ident { name: q.clone(), line, col });
                        for (_, v2) in bindings[i + 1..].iter_mut() {
                            replace_idents(v2, &to, &mut Vec::new());
                        }
                        replace_idents(body, &to, &mut Vec::new());
                        let (_, dropped) = bindings.remove(i);
                        forget_types(self.types, &dropped);
                        self.changed = true;
                        continue;
                    }
                    let b = self.known(&bindings[i].1, env);
                    // A sequence or record literal whose leaves have nothing to run is kept
                    // as the literal: its length, elements and fields are read from it.
                    let v = &bindings[i].1;
                    let lit = if is_safe_sequence(v, env, &self.globals) || (matches!(v, Expr::Record(_)) && is_safe(v, env, &self.globals)) {
                        Some(v.clone())
                    } else {
                        None
                    };
                    env.bind_with(&bindings[i].0, b, lit);
                    i += 1;
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
                    if !read && is_safe(&bindings[i].1, env, &self.globals) {
                        let (_, dropped) = bindings.remove(i);
                        forget_types(self.types, &dropped);
                        self.changed = true;
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
        while simplify::constant(e, self.types) {
            self.changed = true;
        }
        // A sub-expression closed under the sandbox — a method on a literal or a held
        // value, an operator on literals, a field of a held record, an interpolation of
        // held names, a call of the program's own function with literal arguments — is
        // evaluated where it stands, so the reduction continues through its value: an
        // unrolled `order.map(ord1(it))` is the array of what `ord1` answers, and the
        // `join` over it its text.
        let bound = env.names();
        if let Some(lit) = super::closed_literal(e, sb, &bound, done) {
            self.set(e, lit);
        }
    }
}

/// What is known of `e` under `env`; a top-level name outside `env` is a `Global` when the
/// sandbox holds it (`globals`), and `Any` otherwise — a name that folds nothing keys
/// nothing.
fn binding_of(e: &Expr, env: &Env, globals: &HashSet<String>) -> Binding {
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
            Binding::Shape(fields.iter().map(|(k, v)| (k.clone(), binding_of(v, env, globals))).collect())
        }
        Expr::Array(xs) => {
            if xs.len() > MAX_UNROLL {
                return Binding::Any;
            }
            Binding::Seq(xs.iter().map(|x| binding_of(x, env, globals)).collect())
        }
        Expr::Ident { name, .. } => match env.lookup(name) {
            Some(b) => b.clone(),
            None if globals.contains(name) => Binding::Global(name.clone()),
            None => Binding::Any,
        },
        Expr::Field { recv, name, .. } => match binding_of(recv, env, globals) {
            Binding::Shape(fs) => fs.into_iter().find(|(k, _)| k == name).map(|(_, b)| b).unwrap_or(Binding::Any),
            _ => Binding::Any,
        },
        Expr::Index { recv, index, .. } => match (binding_of(recv, env, globals), &**index) {
            (Binding::Seq(xs), Expr::Int(i)) if *i >= 0 && (*i as usize) < xs.len() => xs[*i as usize].clone(),
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

/// The elements of the sequence `e` is: a tuple or array literal — written, or bound to a
/// `let` name — when every element is safe to read more than once and in another place;
/// or a name, field or element the call site is known to have written an array literal
/// for, whose elements are then `e[i]` — or the literal itself, where the call site wrote
/// a scalar.
fn sequence_literal(e: &Expr, env: &Env, globals: &HashSet<String>) -> Option<Vec<Expr>> {
    let seq = match e {
        Expr::Array(xs) | Expr::Tuple(xs) => Some(xs),
        Expr::Ident { name, .. } => match env.literal_of(name) {
            Some(Expr::Array(xs) | Expr::Tuple(xs)) => Some(xs),
            _ => None,
        },
        _ => None,
    };
    if let Some(seq) = seq {
        return if seq.iter().all(|x| is_safe(x, env, globals)) { Some(seq.clone()) } else { None };
    }
    let known = binding_of(e, env, globals);
    let elems = known.seq()?;
    if !is_safe(e, env, globals) {
        return None;
    }
    let (line, col) = crate::visit::expr_pos(e).unwrap_or((0, 0));
    Some(
        elems
            .iter()
            .enumerate()
            .map(|(i, b)| match b {
                Binding::Lit(l) => l.to_expr(),
                _ => Expr::Index { recv: Box::new(e.clone()), index: Box::new(Expr::Int(i as i64)), line, col },
            })
            .collect(),
    )
}

fn recv_kind(e: &Expr, env: &Env, globals: &HashSet<String>) -> Option<Seq> {
    match e {
        Expr::Array(_) => Some(Seq::Array),
        Expr::Tuple(_) => Some(Seq::Tuple),
        Expr::Ident { name, .. } if env.literal_of(name).is_some() => match env.literal_of(name)? {
            Expr::Array(_) => Some(Seq::Array),
            Expr::Tuple(_) => Some(Seq::Tuple),
            _ => None,
        },
        _ => binding_of(e, env, globals).seq().map(|_| Seq::Array),
    }
}

/// A tuple or array literal whose every element is safe (see `sequence_literal`).
fn is_safe_sequence(e: &Expr, env: &Env, globals: &HashSet<String>) -> bool {
    matches!(e, Expr::Array(_) | Expr::Tuple(_)) && sequence_literal(e, env, globals).is_some()
}

/// An expression that may be read more than once, and in another place, for the one it
/// stands in: it cannot raise and has nothing to run — a literal, a name, a present key
/// of a shaped value, an element the call site is known to have written, a tuple, array
/// or record of those.
fn is_safe(e: &Expr, env: &Env, globals: &HashSet<String>) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing | Expr::Ident { .. } => true,
        Expr::Field { recv, name, .. } => {
            is_safe(recv, env, globals)
                && binding_of(recv, env, globals).shape().is_some_and(|fs| fs.iter().any(|(k, _)| k == name))
        }
        Expr::Index { recv, index, .. } => {
            is_safe(recv, env, globals)
                && matches!((binding_of(recv, env, globals), &**index), (Binding::Seq(xs), Expr::Int(i)) if *i >= 0 && (*i as usize) < xs.len())
        }
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().all(|x| is_safe(x, env, globals)),
        Expr::Record(fields) => fields.iter().all(|(_, v)| is_safe(v, env, globals)),
        _ => false,
    }
}

/// `is_safe` outside any clone: a literal, a name, or a tuple, array or record of those.
pub(crate) fn is_trivially_safe(e: &Expr) -> bool {
    is_safe(e, &Env::new(), &HashSet::new())
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

/// Whether `e` binds the name `name` anywhere inside — a `let`, a lambda parameter, a match
/// pattern, or the `it` of a method's body.
fn binds(e: &Expr, name: &str) -> bool {
    let mut found = false;
    crate::visit::walk_expr(e, &mut |x| {
        let here = match x {
            Expr::Let { bindings, .. } => bindings.iter().any(|(n, _)| n == name),
            Expr::Lambda { params, .. } => params.iter().any(|p| p == name),
            Expr::Match { arms, .. } => {
                arms.iter().any(|a| crate::interp::pattern_binding_names(&a.pattern).iter().any(|n| n == name))
            }
            Expr::Method { name: verb, args, .. } => name == "it" && !args.is_empty() && binds_it(verb),
            _ => false,
        };
        if here {
            found = true;
        }
    });
    found
}

/// Whether the method `name` binds `it` for its argument — the comprehension verbs, and
/// no other: `cur.get(it)` reads the `it` of the comprehension around it.
fn binds_it(name: &str) -> bool {
    crate::parser::BOUND_FN_VERBS.contains(&name)
}

/// The literal a receiver stands for, when a name is bound to one — by a `let`, or by the
/// call site's array literal whose elements are all scalars — so a method on it can be
/// evaluated as a method on the literal.
fn literal_receiver(recv: &Expr, env: &Env, globals: &HashSet<String>) -> Option<Expr> {
    if let Expr::Ident { name, .. } = recv
        && let Some(lit) = env.literal_of(name)
        && simplify::is_literal(lit)
    {
        return Some(lit.clone());
    }
    let known = binding_of(recv, env, globals);
    let elems = known.seq()?;
    let lits: Option<Vec<Expr>> = elems.iter().map(|b| if let Binding::Lit(l) = b { Some(l.to_expr()) } else { None }).collect();
    lits.map(Expr::Array)
}

/// Whether `e` mentions the name `name` at all — as an identifier, as the callee of a
/// call by name (`f(p)` reads `f`: the field build's renderer binds `let f = it` and calls
/// it so, and the first cut dropped the binding as unread), or as the free spelling of a
/// method. A binder of it inside counts too — a safe over-approximation of "reads it".
fn mentions(e: &Expr, name: &str) -> bool {
    let mut found = false;
    crate::visit::walk_expr(e, &mut |x| {
        let here = match x {
            Expr::Ident { name: n, .. } | Expr::Call { name: n, .. } => n == name,
            Expr::Method { ufcs: Some(u), .. } => u == name,
            _ => false,
        };
        if here {
            found = true;
        }
    });
    found
}

/// Replace free identifiers under `e` by what `with` gives for their name; a binder of the
/// same name inside `e` shadows it.
pub(super) fn replace_idents(e: &mut Expr, with: &dyn Fn(&str) -> Option<Expr>, shadow: &mut Vec<String>) {
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
        Expr::Call { name, args, line, col } => {
            args.iter_mut().for_each(|a| replace_idents(a, with, shadow));
            // A call BY NAME of the replaced name — `let f = it in f(p)` under an unrolled
            // map, `(f) => f(p)` with the element in for `f` — is a call through the value.
            if !shadow.iter().any(|s| s == name)
                && let Some(to) = with(name)
            {
                let (line, col) = (*line, *col);
                let args = std::mem::take(args);
                *e = Expr::CallValue { callee: Box::new(to), args, line, col };
            }
        }
        Expr::Method { recv, name, args, named, .. } => {
            replace_idents(recv, with, shadow);
            // Only a comprehension verb binds `it` for its body; `cur.get(it)` inside a
            // `map` reads the map's `it`, and must be replaced with it.
            let binder = binds_it(name);
            if binder {
                shadow.push("it".to_string());
            }
            args.iter_mut().for_each(|a| replace_idents(a, with, shadow));
            named.iter_mut().for_each(|(_, v)| replace_idents(v, with, shadow));
            if binder {
                shadow.pop();
            }
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

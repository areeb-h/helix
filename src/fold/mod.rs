//! Constant folding of pure calls with literal arguments (ADR 0050), and the specialization
//! that rides on it (ADR 0051, `specialize`).
//!
//! A call to one of the program's own functions whose arguments are literals — or names
//! bound to literals at the top level — is evaluated ONCE, before the program runs, in a
//! sandboxed tree-walker, and replaced by its value's literal. So is a call through a
//! function-valued field of a record the sandbox already holds (`User.sql({…})`, the object
//! API a library exposes). What the sandbox REFUSES decides what folds: an impure builtin
//! (`print`, `now`, `sleep`), any authority (a file, the network, a database), a Python
//! object, a frame (no literal, and an engine of its own), a name it does not hold (a
//! mutable global, a binding it could not evaluate, a parameter), a write to a mutable
//! global, a recursion deeper than [`MAX_DEPTH`], and more work than a budget of calls and
//! elements — handed to a loop, a method or a builtin, or produced by one. A callee the
//! sandbox abandoned is not tried again in that program. A refusal abandons the fold and the
//! call stays exactly as written — the runtime runs it as it always did, so a fold can never
//! change what a program computes, only when.
//!
//! A GENUINE raise during a fold — the callee refusing its argument — is the program's own
//! error. At a position that runs unconditionally at the top level it is reported before
//! anything runs, as a type error is (`helix check` sees it); anywhere else — under `if`,
//! `match`, `try`, `and`/`or`/`??`, in a lambda or a method's body, inside a function — the
//! call stays and raises at run time as before.
//!
//! The pass runs AFTER the checker, before the receiver-directed rewrite (`ufcs`), in every
//! pipeline that runs, checks or bundles a program. The checker types what the programmer
//! wrote — a fold never adds precision a call lacked, so `launder(true)` typed `Any` stays
//! `Any`, and a type error outranks a raise, since the fold only runs on a program the
//! checker accepted — and every engine runs the one program the fold produced. The
//! checker's types are keyed by node address: a node this pass makes from a typed one
//! inherits its types, and a node it drops is forgotten.
//!
//! What folds besides a call: a method on a literal receiver (`missing.is_missing()`,
//! `["a", "b"].all(…)`), an operator on literals, a field of a record the sandbox holds, an
//! interpolation of held names — and the branch a literal condition selects (`simplify`).
//! These are what make a specialized clone collapse to the work its runtime values need.
//!
//! Two rules keep the pass cheap and honest. A top-level binding is evaluated ON DEMAND —
//! when an attempt meets its name — never eagerly, so a program whose calls fold nothing
//! pays for nothing but the walk. And a fold rewrites IN PLACE: it replaces one node by its
//! literal and never re-allocates another. A name bound locally — a parameter, a `let`, a
//! lambda's own — is never the global of that name, so a body reading its parameter `M`
//! folds nothing of a top-level `M`.

mod simplify;
mod specialize;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::{BinOp, Expr, InterpPart, Stmt};
use crate::error::HelixError;
use crate::interp::Interp;
use crate::types::TypeMap;
use crate::value::{FuncVal, Value};

use specialize::{Binding, Specializer};

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ABORTED: Cell<bool> = const { Cell::new(false) };
    static FUEL: Cell<u64> = const { Cell::new(0) };
    /// The names an attempt met and the sandbox did not hold — a pending one is bound on
    /// demand by [`Sandbox::ensure`] and the attempt retried. All of them, not the first: a
    /// frame verb probes a column's name as a variable before the pending constant comes up.
    static MISSING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The most work one attempt may do — a call, a tail hop, a comprehension element, and an
/// element handed to a loop, a method or a builtin or produced by one each cost one. The
/// field build's render of a literal spec costs on the order of a hundred; its model's
/// constructor a few hundred. The budget is small on purpose: the one cost a fold can ADD
/// is the walker running, before the program does, a call it then abandons — a benchmark's
/// `fib(30)` at the top level — and a unit is a microsecond or two at the worst, so an
/// abandoned attempt costs a millisecond at most, and its callee is not tried again.
const FUEL_PER_ATTEMPT: u64 = 1_000;
/// The most work one program's folding may spend altogether.
const FUEL_PER_PROGRAM: u64 = 10_000;
/// The deepest call chain an attempt may reach. Deeper is abandoned, so a fold never needs
/// the big stack the engines run on, and a program the VM runs on the heap is never refused
/// for the walker's depth.
pub(crate) const MAX_DEPTH: usize = 256;
/// The largest literal a fold writes back, in AST nodes; a larger value stays a call.
pub(super) const MAX_LITERAL_NODES: usize = 4_096;

/// Whether a sandboxed evaluation is under way — the hooks outside the interpreter (the
/// authority gate, the Python bridge, the array and tensor method entries) consult this.
pub(crate) fn sandbox_active() -> bool {
    ACTIVE.with(|a| a.get())
}

/// Abandon the current attempt: sticky, so a `try` inside the evaluated code cannot swallow
/// the refusal and let a half-evaluated fold succeed.
pub(crate) fn abort() {
    ABORTED.with(|a| a.set(true));
}

/// The error a hook returns after abandoning — never shown; the attempt discards it.
pub(crate) fn abort_err(line: usize, col: usize) -> HelixError {
    abort();
    HelixError::new("constant folding abandoned", line, col)
}

/// Remember a name the sandbox did not hold: a top-level binding the program made earlier
/// is evaluated on demand and the attempt retried; any other (a mutable global, a
/// parameter, a binding the sandbox could not evaluate) leaves the call as written.
pub(crate) fn missing_name(name: &str) {
    MISSING.with(|m| {
        let mut m = m.borrow_mut();
        if !m.iter().any(|n| n == name) {
            m.push(name.to_string());
        }
    });
}

/// A name the identifier evaluator did not find: remembered, and the attempt abandoned —
/// the sandbox never reports an unknown name as the program's error.
pub(crate) fn refuse_name(name: &str) {
    missing_name(name);
    abort();
}

fn take_missing() -> Vec<String> {
    MISSING.with(|m| std::mem::take(&mut *m.borrow_mut()))
}

/// Spend `n` units of the attempt's budget; `false` (and abandoned) once it is gone.
pub(crate) fn charge(n: u64) -> bool {
    FUEL.with(|f| {
        let left = f.get();
        if left < n {
            f.set(0);
            abort();
            false
        } else {
            f.set(left - n);
            true
        }
    })
}

/// What a value costs to hold, in units: an array's or a tensor's elements, a string's
/// 64-byte blocks; a scalar is free. Charged for what a builtin or a method PRODUCES, so
/// `to_array(range(0, n))` and `"x".repeat(n)` are bounded like a loop.
pub(crate) fn value_size(v: &Value) -> u64 {
    match v {
        Value::Array(a) => a.len() as u64,
        Value::Tensor(t) => t.len() as u64,
        Value::Str(s) => (s.len() / 64) as u64,
        _ => 0,
    }
}

/// A sandboxed tree-walker holding the program's functions and the top-level values it
/// has been asked for, plus the program's remaining budget.
pub(super) struct Sandbox {
    interp: Interp,
    program_fuel: u64,
    /// Top-level immutable bindings the program has made so far and the sandbox has not
    /// evaluated: name → the index of the statement that binds it, in the program's prefix
    /// (`done` below).
    pending: HashMap<String, usize>,
    /// Callees the sandbox abandoned — for impurity, a write, a frame, the depth or the
    /// budget — by the label `candidate_label` gives them: not tried again in this program,
    /// so a benchmark's `fib(30)` costs one abandoned attempt, not one per call.
    futile: HashSet<String>,
    /// The nodes of the sandbox's copies of the program's lambda bodies. A copy outlives
    /// the copying — the walker's closures hold it — so it is given the checker's types of
    /// the body it was copied from, and those are forgotten when the sandbox is.
    typed_copies: Vec<Vec<*const Expr>>,
}

/// How an attempt ended without the program's own raise.
pub(super) enum Outcome {
    Value(Value),
    /// The sandbox refused the evaluation itself — the callee is futile here.
    Abandoned,
    /// A name the sandbox does not hold and could not bind — the callee may be fine with
    /// other arguments.
    Unheld,
}

impl Sandbox {
    fn new(program: &[Stmt], sp: &mut Specializer) -> Self {
        let mut interp = Interp::sandbox();
        let muts: HashSet<String> = program
            .iter()
            .flat_map(|s| match s {
                Stmt::Assign { name, mutable: true, .. } => vec![name.clone()],
                Stmt::Destructure { names, mutable: true, .. } => names.clone(),
                _ => Vec::new(),
            })
            .collect();
        interp.set_fold_mut_names(muts);
        // Every top-level name the program binds — what a frame verb's resolver must tell
        // from a column's name when the sandbox does not hold it.
        let tops: HashSet<String> = program
            .iter()
            .flat_map(|s| match s {
                Stmt::Assign { name, .. } => vec![name.clone()],
                Stmt::Destructure { names, .. } => names.clone(),
                _ => Vec::new(),
            })
            .collect();
        interp.set_fold_top_names(tops);
        // Every top-level `fn` the runtime hoists — one shadowed by a top-level assignment
        // is not hoisted there and is not a function here (a definition evaluates nothing).
        let assigned: HashSet<&str> = program
            .iter()
            .flat_map(|s| match s {
                Stmt::Assign { name, .. } => vec![name.as_str()],
                Stmt::Destructure { names, .. } => names.iter().map(String::as_str).collect(),
                _ => Vec::new(),
            })
            .collect();
        let mut sb = Sandbox {
            interp,
            program_fuel: FUEL_PER_PROGRAM,
            pending: HashMap::new(),
            futile: HashSet::new(),
            typed_copies: Vec::new(),
        };
        for s in program {
            if matches!(s, Stmt::Func { name, .. } if !assigned.contains(name.as_str())) {
                sb.hoist_fn(s, sp);
            }
        }
        sb
    }

    /// Define a top-level function in the sandbox, from a copy that owns its lambda bodies.
    /// `fold_expr` rewrites the program's bodies in place, and a body shared with the
    /// sandbox would have to be re-allocated to be written — moving every node inside it
    /// away from the address the checker's type map knows it by. The copy's lambda bodies
    /// are what the walker's closures will hold, so they are given the types of the bodies
    /// they were copied from — a closure devirtualized later is typed through them.
    fn hoist_fn(&mut self, stmt: &Stmt, sp: &mut Specializer) {
        let mut copy = stmt.clone();
        if let (Stmt::Func { body: original, .. }, Stmt::Func { body, .. }) = (stmt, &mut copy) {
            let originals = lambda_bodies(original);
            unshare_lambdas(body);
            for (o, c) in originals.iter().zip(lambda_bodies(body)) {
                self.typed_copies.push(sp.type_copy(o, &c));
            }
        }
        let _ = self.interp.run(std::slice::from_ref(&copy));
    }

    /// Hold a value under a top-level name, as the engines will from a hoisted binding.
    fn hold(&mut self, name: String, value: Value) {
        self.interp.hold_global(name, value);
    }

    /// The closure held as field `field` of the record the top-level `name` holds.
    fn closure_field(&self, name: &str, field: &str) -> Option<Rc<FuncVal>> {
        match self.interp.global_value(name)? {
            Value::Record(fields) => match fields.iter().find(|(s, _)| s.as_str() == field) {
                Some((_, Value::Function(f))) => Some(f.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Run `f` under the sandbox's guards: `Ok(Some(_))` on a clean evaluation, `Ok(None)`
    /// when the sandbox abandoned it, `Err` for the program's own raise.
    fn guarded<T>(&mut self, f: impl FnOnce(&mut Interp) -> Result<T, HelixError>) -> Result<Option<T>, HelixError> {
        if self.program_fuel == 0 {
            return Ok(None);
        }
        let fuel = FUEL_PER_ATTEMPT.min(self.program_fuel);
        ACTIVE.with(|a| a.set(true));
        ABORTED.with(|a| a.set(false));
        FUEL.with(|c| c.set(fuel));
        let r = f(&mut self.interp);
        ACTIVE.with(|a| a.set(false));
        let spent = fuel - FUEL.with(|c| c.get());
        self.program_fuel = self.program_fuel.saturating_sub(spent);
        let aborted = ABORTED.with(|a| a.replace(false));
        match r {
            _ if aborted => Ok(None),
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(e),
        }
    }

    /// Evaluate the candidate `e`, binding on demand each top-level name it turns out to
    /// need and retrying; `done` is the program's prefix, where those names are bound.
    fn attempt(&mut self, e: &Expr, done: &[Stmt]) -> Result<Outcome, HelixError> {
        let stmt = Stmt::Expr(e.clone());
        loop {
            take_missing();
            let r = self.guarded(|i| i.exec(&stmt));
            let names = take_missing();
            match r {
                Ok(Some(out)) => return Ok(Outcome::Value(out.value)),
                Ok(None) if names.iter().any(|n| self.ensure(n, done)) => continue,
                Ok(None) => return Ok(if names.is_empty() { Outcome::Abandoned } else { Outcome::Unheld }),
                Err(e) => return Err(e),
            }
        }
    }

    /// Note a top-level immutable binding, to be evaluated if and when an attempt needs it.
    fn note_top(&mut self, idx: usize, stmt: &Stmt) {
        for n in stmt_names(stmt) {
            self.pending.insert(n, idx);
        }
    }

    /// Hold the top-level binding `name`: evaluate the statement that made it — and, on
    /// demand, the earlier bindings that one reads. `true` when the sandbox holds it after.
    /// A binding it cannot evaluate is forgotten, so anything reading it is refused too; one
    /// that RAISES is not reported here — its own fold already was, at its unconditional
    /// position. Each is tried once, whatever comes of it.
    fn ensure(&mut self, name: &str, done: &[Stmt]) -> bool {
        let Some(idx) = self.pending.get(name).copied() else {
            return false;
        };
        let stmt = &done[idx];
        for n in stmt_names(stmt) {
            self.pending.remove(&n);
        }
        loop {
            take_missing();
            let r = self.guarded(|i| i.exec(stmt));
            match (r, take_missing()) {
                (Ok(Some(_)), _) => return true,
                (Ok(None), deps) if deps.iter().any(|d| self.ensure(d, done)) => continue,
                _ => {
                    for n in stmt_names(stmt) {
                        self.interp.forget_global(&n);
                    }
                    return false;
                }
            }
        }
    }

    fn is_user_fn(&self, name: &str) -> bool {
        self.interp.is_declared_fn(name) && self.interp.global_is_function(name)
    }

    /// Whether `name` is a record the sandbox holds — binding it on demand first.
    fn holds_record(&mut self, name: &str, done: &[Stmt]) -> bool {
        self.holds_value(name, done) && self.interp.global_is_record(name)
    }

    /// Whether the sandbox holds a value under `name` — binding it on demand first.
    fn holds_value(&mut self, name: &str, done: &[Stmt]) -> bool {
        if self.pending.contains_key(name) {
            self.ensure(name, done);
        }
        self.interp.has_global(name)
    }

    /// A name the sandbox holds, or could on demand.
    fn holds(&self, name: &str) -> bool {
        self.interp.has_global(name) || self.pending.contains_key(name)
    }
}

fn stmt_names(stmt: &Stmt) -> Vec<String> {
    match stmt {
        Stmt::Assign { name, .. } => vec![name.clone()],
        Stmt::Destructure { names, .. } => names.clone(),
        _ => Vec::new(),
    }
}

/// The body of every lambda under `e`, preorder.
fn lambda_bodies(e: &Expr) -> Vec<Rc<Expr>> {
    let mut out = Vec::new();
    crate::visit::walk_expr(e, &mut |x| {
        if let Expr::Lambda { body, .. } = x {
            out.push(body.clone());
        }
    });
    out
}

/// Fold every foldable call in `stmts`, in program order, and specialize what a call site
/// knows (ADR 0051) unless `HELIX_NOSPECIALIZE` is set. `types` are the checker's, keyed by
/// node address: kept true for the nodes this pass makes and drops. `Err` is a raise the
/// program would meet unconditionally at the top level.
pub fn fold_program(stmts: &mut Vec<Stmt>, types: &mut TypeMap) -> Result<(), HelixError> {
    // `HELIX_NOFOLD=1` runs every call at run time, for an A/B — `HELIX_NOJIT`'s twin.
    if std::env::var_os("HELIX_NOFOLD").is_some() {
        return Ok(());
    }
    fold_program_with(stmts, types, std::env::var_os("HELIX_NOSPECIALIZE").is_none())
}

pub(crate) fn fold_program_with(stmts: &mut Vec<Stmt>, types: &mut TypeMap, specialize: bool) -> Result<(), HelixError> {
    if !stmts.iter().any(|s| matches!(s, Stmt::Func { .. })) {
        return Ok(());
    }
    let mut sp = Specializer::new(stmts, types, specialize);
    let mut sb = Sandbox::new(stmts, &mut sp);
    let mut i = 0;
    // Clones are appended as they are made and folded when the walk reaches them.
    while i < stmts.len() {
        {
            // The statements before this one are where a name the sandbox is asked for is
            // bound.
            let (done, rest) = stmts.split_at_mut(i);
            let stmt = &mut rest[0];
            let mut bound: Vec<String> = Vec::new();
            match stmt {
                Stmt::Func { params, body, .. } => {
                    bound.extend(params.iter().map(|(n, _)| n.clone()));
                    fold_expr(body, &mut sb, &mut sp, done, &mut bound, false)?;
                }
                Stmt::Assign { value, .. } | Stmt::Destructure { value, .. } => {
                    fold_expr(value, &mut sb, &mut sp, done, &mut bound, true)?
                }
                Stmt::Expr(e) => fold_expr(e, &mut sb, &mut sp, done, &mut bound, true)?,
                Stmt::Import { .. } => {}
            }
            if matches!(stmt, Stmt::Assign { mutable: false, .. } | Stmt::Destructure { mutable: false, .. }) {
                sb.note_top(i, stmt);
            }
        }
        let pending = sp.take_pending();
        if !pending.is_empty() {
            move_stmts(stmts, sp.types(), |v| {
                v.extend(pending);
                0
            });
        }
        i += 1;
    }
    // The sandbox's copies go with it; so do their types.
    for ptrs in std::mem::take(&mut sb.typed_copies) {
        sp.forget_ptrs(&ptrs);
    }
    // Captured values hoisted for a devirtualized closure come first: every statement that
    // may call it runs after them.
    let hoisted = sp.take_hoisted();
    if !hoisted.is_empty() {
        move_stmts(stmts, sp.types(), |v| {
            let k = hoisted.len();
            v.splice(0..0, hoisted);
            k
        });
    }
    Ok(())
}

/// The root expression of a statement — inline in the statement, so it moves with it.
fn root_of(s: &Stmt) -> Option<*const Expr> {
    match s {
        Stmt::Assign { value, .. } | Stmt::Destructure { value, .. } | Stmt::Expr(value) => Some(value as *const Expr),
        Stmt::Func { body, .. } => Some(body as *const Expr),
        Stmt::Import { .. } => None,
    }
}

/// Move the statements with `f`, carrying the checker's types of their roots to where they
/// land — the map is keyed by address, a root lives inline in its statement, and a vector
/// that grows or is spliced moves every statement in it. `f` returns the index at which
/// the first of the statements it was handed now sits. Every other node is where its
/// parent's box or vector put it, and stays there.
fn move_stmts(stmts: &mut Vec<Stmt>, types: &mut TypeMap, f: impl FnOnce(&mut Vec<Stmt>) -> usize) {
    let saved: Vec<Option<crate::types::Type>> = stmts.iter().map(|s| root_of(s).and_then(|p| types.remove(&p))).collect();
    let at = f(stmts);
    for (s, t) in stmts[at..].iter().zip(saved) {
        if let (Some(p), Some(t)) = (root_of(s), t) {
            types.insert(p, t);
        }
    }
}

/// Fold inside `e` (children first), then `e` itself where it is a candidate, then
/// specialize the call it may be. `bound`: the names local to this position, which are
/// never the globals of those names. `unconditional`: whether this position runs whenever
/// its top-level statement does.
fn fold_expr(
    e: &mut Expr,
    sb: &mut Sandbox,
    sp: &mut Specializer,
    done: &[Stmt],
    bound: &mut Vec<String>,
    unconditional: bool,
) -> Result<(), HelixError> {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing
        | Expr::Ident { .. } | Expr::Column { .. } => {}
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    fold_expr(x, sb, sp, done, bound, unconditional)?;
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                fold_expr(x, sb, sp, done, bound, unconditional)?;
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                fold_expr(v, sb, sp, done, bound, unconditional)?;
            }
        }
        Expr::RecordUpdate { parts, .. } => {
            for p in parts {
                fold_expr(p.expr_mut(), sb, sp, done, bound, unconditional)?;
            }
        }
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } => {
            fold_expr(recv, sb, sp, done, bound, unconditional)?
        }
        Expr::Unary { expr, .. } => fold_expr(expr, sb, sp, done, bound, unconditional)?,
        Expr::Binary { op, left, right, .. } => {
            fold_expr(left, sb, sp, done, bound, unconditional)?;
            let right_runs = unconditional && !matches!(op, BinOp::And | BinOp::Or | BinOp::Coalesce);
            fold_expr(right, sb, sp, done, bound, right_runs)?;
        }
        Expr::Call { args, .. } => {
            for a in args {
                fold_expr(a, sb, sp, done, bound, unconditional)?;
            }
        }
        // A method's arguments may be bodies run per element (`it`-forms), so nothing in
        // them is unconditional, and `it` is bound in them. A frame verb reads its arguments
        // as written (`takes_unevaluated_args`), so nothing inside them is rewritten.
        Expr::Method { recv, name, args, named, .. } => {
            fold_expr(recv, sb, sp, done, bound, unconditional)?;
            if !crate::interp::takes_unevaluated_args(name) {
                bound.push("it".to_string());
                for a in args {
                    fold_expr(a, sb, sp, done, bound, false)?;
                }
                for (_, v) in named {
                    fold_expr(v, sb, sp, done, bound, false)?;
                }
                bound.pop();
            }
        }
        Expr::CallValue { callee, args, .. } => {
            fold_expr(callee, sb, sp, done, bound, unconditional)?;
            for a in args {
                fold_expr(a, sb, sp, done, bound, unconditional)?;
            }
        }
        Expr::Index { recv, index, .. } => {
            fold_expr(recv, sb, sp, done, bound, unconditional)?;
            fold_expr(index, sb, sp, done, bound, unconditional)?;
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            fold_expr(recv, sb, sp, done, bound, unconditional)?;
            for part in [start, stop, step].into_iter().flatten() {
                fold_expr(part, sb, sp, done, bound, unconditional)?;
            }
        }
        Expr::Lambda { params, defaults, body, .. } => {
            for d in defaults {
                fold_expr(d, sb, sp, done, bound, false)?;
            }
            let mark = bound.len();
            bound.extend(params.iter().cloned());
            // In place or not at all: a body another owner shares (the sandbox holds a
            // closure over it) would be re-allocated by `make_mut`, and the checker's type
            // map knows every node inside by its address.
            if let Some(b) = Rc::get_mut(body) {
                fold_expr(b, sb, sp, done, bound, false)?;
            }
            bound.truncate(mark);
        }
        Expr::Let { bindings, body, .. } => {
            let mark = bound.len();
            for (n, v) in bindings {
                fold_expr(v, sb, sp, done, bound, unconditional)?;
                bound.push(n.clone());
            }
            fold_expr(body, sb, sp, done, bound, unconditional)?;
            bound.truncate(mark);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            fold_expr(cond, sb, sp, done, bound, unconditional)?;
            fold_expr(then_branch, sb, sp, done, bound, false)?;
            fold_expr(else_branch, sb, sp, done, bound, false)?;
        }
        Expr::Try { expr, .. } => fold_expr(expr, sb, sp, done, bound, false)?,
        Expr::Match { scrutinee, arms, .. } => {
            fold_expr(scrutinee, sb, sp, done, bound, unconditional)?;
            for arm in arms {
                let mark = bound.len();
                bound.extend(crate::interp::pattern_binding_names(&arm.pattern));
                if let Some(g) = &mut arm.guard {
                    fold_expr(g, sb, sp, done, bound, false)?;
                }
                fold_expr(&mut arm.body, sb, sp, done, bound, false)?;
                bound.truncate(mark);
            }
        }
    }
    // A literal that decides an `if`, an `and`, an `or`, a `??` selects its branch.
    while simplify::constant(e, sp.types()) {}
    // A call the sandbox evaluated in full whose value has no literal — a record of
    // closures — ran once here and runs once there; it earns no clone.
    let mut evaluated = false;
    if let Some(label) = candidate_label(e, sb, bound, done) {
        // The futility memo is for callees: a literal receiver or operator is its own case.
        let memo = !label.starts_with('#');
        if !(memo && sb.futile.contains(&label)) {
            match sb.attempt(e, done) {
                Ok(Outcome::Value(v)) => {
                    let mut budget = MAX_LITERAL_NODES;
                    if let Some(lit) = to_expr(&v, &mut budget) {
                        sp.set(e, lit);
                        return Ok(());
                    }
                    evaluated = true;
                }
                Ok(Outcome::Abandoned) => {
                    if memo {
                        sb.futile.insert(label);
                    }
                }
                Ok(Outcome::Unheld) => {}
                Err(raise) => {
                    if unconditional {
                        return Err(raise);
                    }
                }
            }
        }
    }
    if sp.enabled() && !evaluated {
        specialize_site(e, sb, sp, done, bound);
    }
    Ok(())
}

/// The specialization rules at one call site (ADR 0051): a method through a record the
/// sandbox holds becomes a direct call of the closure it holds there, and a call to one of
/// the program's functions that passes a literal shape, a held name or a scalar literal is
/// pointed at the clone made for exactly that.
fn specialize_site(e: &mut Expr, sb: &mut Sandbox, sp: &mut Specializer, done: &[Stmt], bound: &[String]) {
    if let Expr::Method { recv, name, args, named, line, col, .. } = e
        && named.is_empty()
        && let Expr::Ident { name: g, .. } = &**recv
        && !bound.iter().any(|b| b == g)
        && !crate::registry::type_owns_method("Record", name)
        && sb.holds_record(g, done)
        && let Some(fv) = sb.closure_field(g, name)
        && let Some(fname) = sp.devirtualize(g, name, &fv)
    {
        let (line, col) = (*line, *col);
        // A function-valued field receives the ORIGIN of a lambda the parser synthesized
        // from a bare bound name (ADR 0045) — as the walker and the VM hand it over. The
        // lambda shell around it is dropped, so its type is forgotten with the call's.
        for a in args.iter() {
            if matches!(a, Expr::Lambda { bound: Some(_), .. }) {
                sp.types().remove(&(a as *const Expr));
            }
        }
        let unwrapped: Vec<Expr> = std::mem::take(args)
            .into_iter()
            .map(|a| match a {
                Expr::Lambda { bound: Some(origin), .. } => *origin,
                other => other,
            })
            .collect();
        let call = Expr::Call { name: fname, args: unwrapped, line, col };
        sp.set(e, call);
    }
    if let Expr::Call { name, args, .. } = e
        && sp.knows(name)
    {
        let bindings: Vec<Binding> = args.iter().map(|a| sp.binding_at(a, bound, &|n| sb.holds(n))).collect();
        if let Some(n) = sp.specialize(name, &bindings, 0, sb, done) {
            *name = n;
        }
    }
    let made: Vec<Stmt> = sp.new_pending().to_vec();
    for st in &made {
        sb.hoist_fn(st, sp);
    }
    for (n, v) in sp.take_held() {
        sb.hold(n, v);
    }
}

/// What the sandbox may evaluate here, and the label the futility memo keeps for it: a
/// call to one of the program's own functions, or a method on a value the sandbox
/// holds — with arguments that mention nothing the sandbox lacks — is labelled by its
/// callee; a method on a literal, an operator on literals, a field of a held record, an
/// interpolation of held names, `type_of` of a literal are labelled `#…` and never
/// remembered as futile. A name bound locally is never the global of that name.
pub(super) fn candidate_label(e: &Expr, sb: &mut Sandbox, bound: &[String], done: &[Stmt]) -> Option<String> {
    let local = |n: &str| bound.iter().any(|b| b == n);
    let closed = |a: &Expr| {
        let mut b = bound.to_vec();
        known_closed(a, sb, &mut b)
    };
    match e {
        Expr::Call { name, args, .. } if !local(name) && sb.is_user_fn(name) && args.iter().all(closed) => Some(name.clone()),
        Expr::Call { name, args, .. } if name == "type_of" && args.len() == 1 && simplify::is_literal(&args[0]) => {
            Some("#type_of".to_string())
        }
        Expr::Method { recv, name, args, named, .. } if named.is_empty() && args.iter().all(closed) => match &**recv {
            Expr::Ident { name: r, .. } if !local(r) && sb.holds_value(r, done) => Some(format!("{r}.{name}")),
            recv if simplify::is_literal(recv) => Some("#literal".to_string()),
            _ => None,
        },
        Expr::Field { recv, .. } => match &**recv {
            Expr::Ident { name: r, .. } if !local(r) && sb.holds(r) => Some("#field".to_string()),
            _ => None,
        },
        Expr::Binary { op, left, right, .. }
            if !matches!(op, BinOp::And | BinOp::Or | BinOp::Coalesce)
                && simplify::is_literal(left)
                && simplify::is_literal(right) =>
        {
            Some("#operator".to_string())
        }
        Expr::Unary { expr, .. } if simplify::is_literal(expr) => Some("#operator".to_string()),
        Expr::Index { recv, index, .. } if simplify::is_literal(recv) && simplify::is_literal(index) => {
            Some("#operator".to_string())
        }
        Expr::Interp(parts)
            if parts.iter().any(|p| matches!(p, InterpPart::Expr(..)))
                && parts.iter().all(|p| match p {
                    InterpPart::Expr(x, _) => closed(x),
                    InterpPart::Lit(_) => true,
                }) =>
        {
            Some("#interp".to_string())
        }
        _ => None,
    }
}

/// An expression the sandbox can evaluate: no column reference, no free name it does not
/// hold — a name bound locally (`bound`) counts as free, whatever global shares it.
pub(super) fn known_closed(e: &Expr, sb: &Sandbox, bound: &mut Vec<String>) -> bool {
    match e {
        Expr::Column { .. } => false,
        Expr::Ident { name, .. } => {
            if bound.iter().any(|b| b == name) {
                // A local of this position: the sandbox holds the global of that name, if
                // any, and that is not this.
                return false;
            }
            sb.holds(name) || sb.is_user_fn(name)
        }
        Expr::Lambda { params, defaults, body, .. } => {
            let mark = bound.len();
            let ok = defaults.iter().all(|d| known_closed(d, sb, bound)) && {
                bound.extend(params.iter().cloned());
                known_closed(body, sb, bound)
            };
            bound.truncate(mark);
            ok
        }
        Expr::Let { bindings, body, .. } => {
            let mark = bound.len();
            let mut ok = true;
            for (n, v) in bindings {
                ok = ok && known_closed(v, sb, bound);
                bound.push(n.clone());
            }
            ok = ok && known_closed(body, sb, bound);
            bound.truncate(mark);
            ok
        }
        Expr::Match { scrutinee, arms, .. } => {
            if !known_closed(scrutinee, sb, bound) {
                return false;
            }
            arms.iter().all(|arm| {
                let mark = bound.len();
                bound.extend(crate::interp::pattern_binding_names(&arm.pattern));
                let ok = arm.guard.as_ref().is_none_or(|g| known_closed(g, sb, bound))
                    && known_closed(&arm.body, sb, bound);
                bound.truncate(mark);
                ok
            })
        }
        // A method argument may be an `it`-body: its binder is the method's own.
        Expr::Method { recv, args, named, .. } => {
            known_closed(recv, sb, bound)
                && {
                    let mark = bound.len();
                    bound.push("it".to_string());
                    let ok = args.iter().all(|a| known_closed(a, sb, bound))
                        && named.iter().all(|(_, v)| known_closed(v, sb, bound));
                    bound.truncate(mark);
                    ok
                }
        }
        Expr::Interp(parts) => parts.iter().all(|p| match p {
            InterpPart::Expr(x, _) => known_closed(x, sb, bound),
            _ => true,
        }),
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().all(|x| known_closed(x, sb, bound)),
        Expr::Record(fields) => fields.iter().all(|(_, v)| known_closed(v, sb, bound)),
        Expr::RecordUpdate { parts, .. } => parts.iter().all(|p| known_closed(p.expr(), sb, bound)),
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } | Expr::Unary { expr: recv, .. } | Expr::Try { expr: recv, .. } => {
            known_closed(recv, sb, bound)
        }
        Expr::Binary { left, right, .. } => known_closed(left, sb, bound) && known_closed(right, sb, bound),
        Expr::Call { args, .. } => args.iter().all(|a| known_closed(a, sb, bound)),
        Expr::CallValue { callee, args, .. } => {
            known_closed(callee, sb, bound) && args.iter().all(|a| known_closed(a, sb, bound))
        }
        Expr::Index { recv, index, .. } => known_closed(recv, sb, bound) && known_closed(index, sb, bound),
        Expr::Slice { recv, start, stop, step, .. } => {
            known_closed(recv, sb, bound)
                && [start, stop, step].into_iter().flatten().all(|x| known_closed(x, sb, bound))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            known_closed(cond, sb, bound) && known_closed(then_branch, sb, bound) && known_closed(else_branch, sb, bound)
        }
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => true,
    }
}

/// Give every lambda under `e` a body of its own — a copy of a function must not share the
/// program's nodes (see [`Sandbox::hoist_fn`]).
pub(super) fn unshare_lambdas(e: &mut Expr) {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing
        | Expr::Ident { .. } | Expr::Column { .. } => {}
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    unshare_lambdas(x);
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter_mut().for_each(unshare_lambdas),
        Expr::Record(fields) => fields.iter_mut().for_each(|(_, v)| unshare_lambdas(v)),
        Expr::RecordUpdate { parts, .. } => parts.iter_mut().for_each(|p| unshare_lambdas(p.expr_mut())),
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } | Expr::Unary { expr: recv, .. } | Expr::Try { expr: recv, .. } => {
            unshare_lambdas(recv)
        }
        Expr::Binary { left, right, .. } => {
            unshare_lambdas(left);
            unshare_lambdas(right);
        }
        Expr::Call { args, .. } => args.iter_mut().for_each(unshare_lambdas),
        Expr::Method { recv, args, named, .. } => {
            unshare_lambdas(recv);
            args.iter_mut().for_each(unshare_lambdas);
            named.iter_mut().for_each(|(_, v)| unshare_lambdas(v));
        }
        Expr::CallValue { callee, args, .. } => {
            unshare_lambdas(callee);
            args.iter_mut().for_each(unshare_lambdas);
        }
        Expr::Index { recv, index, .. } => {
            unshare_lambdas(recv);
            unshare_lambdas(index);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            unshare_lambdas(recv);
            for part in [start, stop, step].into_iter().flatten() {
                unshare_lambdas(part);
            }
        }
        Expr::Lambda { defaults, body, .. } => {
            defaults.iter_mut().for_each(unshare_lambdas);
            let mut owned: Expr = (**body).clone();
            unshare_lambdas(&mut owned);
            *body = Rc::new(owned);
        }
        Expr::Let { bindings, body, .. } => {
            bindings.iter_mut().for_each(|(_, v)| unshare_lambdas(v));
            unshare_lambdas(body);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            unshare_lambdas(cond);
            unshare_lambdas(then_branch);
            unshare_lambdas(else_branch);
        }
        Expr::Match { scrutinee, arms, .. } => {
            unshare_lambdas(scrutinee);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    unshare_lambdas(g);
                }
                unshare_lambdas(&mut arm.body);
            }
        }
    }
}

/// The literal for a value, when it has one: numbers, strings, booleans, `missing`, arrays,
/// tuples and records of those, and a dict as the pairs that rebuild it — within `budget`
/// nodes. A function, a frame, a tensor, a rational, bytes: no literal, no fold.
pub(super) fn to_expr(v: &Value, budget: &mut usize) -> Option<Expr> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    Some(match v {
        Value::Int(i) => Expr::Int(*i),
        Value::Float(f) => Expr::Float(*f),
        Value::Str(s) => Expr::Str((**s).clone()),
        Value::Bool(b) => Expr::Bool(*b),
        Value::Missing => Expr::Missing,
        Value::Array(a) => Expr::Array(a.iter_values().map(|x| to_expr(&x, budget)).collect::<Option<Vec<_>>>()?),
        Value::Tuple(t) => Expr::Tuple(t.iter().map(|x| to_expr(x, budget)).collect::<Option<Vec<_>>>()?),
        Value::Record(fields) => Expr::Record(
            fields
                .iter()
                .map(|(s, x)| Some((s.as_str().to_string(), to_expr(x, budget)?)))
                .collect::<Option<Vec<_>>>()?,
        ),
        // A dict has no literal of its own; the pairs that rebuild it, in its own key order,
        // evaluate to an equal dict wherever the fold writes them.
        Value::Dict(d) => {
            use crate::value::DictKey;
            let map = d.map();
            let mut pairs = Vec::with_capacity(map.len());
            for (k, x) in map.iter() {
                let key = match k {
                    DictKey::Bool(b) => Expr::Bool(*b),
                    DictKey::Int(i) => Expr::Int(*i),
                    DictKey::Str(s) => Expr::Str((**s).clone()),
                    DictKey::Dna(_) => return None,
                };
                pairs.push(Expr::Tuple(vec![key, to_expr(x, budget)?]));
            }
            Expr::Method {
                recv: Box::new(Expr::Array(pairs)),
                name: "to_dict".to_string(),
                args: Vec::new(),
                named: Vec::new(),
                ufcs: None,
                line: 0,
                col: 0,
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(src: &str) -> Vec<Stmt> {
        let toks = crate::lexer::lex(src).unwrap_or_else(|e| panic!("{}", e.message));
        crate::parser::parse(toks).unwrap_or_else(|e| panic!("{}", e.message))
    }
    fn folded_with(src: &str, specialize: bool) -> Vec<Stmt> {
        let mut stmts = parsed(src);
        let mut types = TypeMap::default();
        fold_program_with(&mut stmts, &mut types, specialize).unwrap_or_else(|e| panic!("{}", e.message));
        stmts
    }
    fn folded(src: &str) -> Vec<Stmt> {
        folded_with(src, false)
    }
    fn fold_err(src: &str) -> String {
        let mut stmts = parsed(src);
        let mut types = TypeMap::default();
        match fold_program_with(&mut stmts, &mut types, false) {
            Ok(()) => panic!("expected a fold-time raise for {src:?}"),
            Err(e) => e.message,
        }
    }
    fn value_of(stmt: &Stmt) -> &Expr {
        match stmt {
            Stmt::Assign { value, .. } => value,
            Stmt::Expr(e) => e,
            other => panic!("not a value statement: {other:?}"),
        }
    }
    fn func<'a>(stmts: &'a [Stmt], name: &str) -> &'a Expr {
        stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Func { name: n, body, .. } if n == name => Some(body),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no function `{name}` in {stmts:?}"))
    }
    fn count_nodes(e: &Expr, pred: impl Fn(&Expr) -> bool) -> usize {
        let mut n = 0;
        crate::visit::walk_expr(e, &mut |x| {
            if pred(x) {
                n += 1;
            }
        });
        n
    }

    /// A pure call with literal arguments becomes its value; through a record the sandbox
    /// holds — the object API — too; a record's own method as well.
    #[test]
    fn a_pure_call_with_literal_arguments_folds() {
        let s = folded("fn double(x) = x * 2\ny = double(21)\nz = double(y)");
        assert!(matches!(value_of(&s[1]), Expr::Int(42)), "{:?}", s[1]);
        assert!(matches!(value_of(&s[2]), Expr::Int(84)), "{:?}", s[2]);
        let s = folded(
            "sep = \" and \"\nfn mk(spec) = {c: spec.columns, sql: (conds) => \"select * from {spec.table} where {conds.join(sep)}\"}\nM = mk({table: \"t\", columns: {id: 1}})\nq = M.sql([\"a = 1\", \"b = 2\"])\nk = M.keys()",
        );
        assert!(matches!(value_of(&s[3]), Expr::Str(q) if q == "select * from t where a = 1 and b = 2"), "{:?}", s[3]);
        assert!(matches!(value_of(&s[2]), Expr::Call { .. }), "a record holding a lambda has no literal: {:?}", s[2]);
        assert!(matches!(value_of(&s[4]), Expr::Array(_)), "{:?}", s[4]);
        // Inside a body, a call with literal arguments folds; one with a parameter cannot.
        let s = folded("fn g(y) = y + 1\nfn f(x) = g(1) + g(x)");
        assert!(matches!(func(&s, "f"), Expr::Binary { left, right, .. } if matches!(**left, Expr::Int(2)) && matches!(**right, Expr::Call { .. })));
        // A fn shadowed by a top-level assignment is not a function at run time, so not here.
        let s = folded("fn f(x) = x + 1\nf = 5\ny = f(1)");
        assert!(matches!(value_of(&s[2]), Expr::Call { .. }), "{:?}", s[2]);
    }

    /// What the sandbox refuses stays a call: an impure builtin, an authority, a mutable
    /// global read or written, a name it does not hold, a Python object, too much work, too
    /// deep a recursion, a value with no literal, a literal too large, a builtin that answers
    /// from context, a loop over more elements than the budget (`reduce` charges up front,
    /// as every comprehension does), a frame, a value too big to hold — and a method on a
    /// binding that is not a record.
    #[test]
    fn what_the_sandbox_refuses_stays_a_call() {
        for src in [
            "fn loud(x) = do {\n  print(x)\n  x\n}\ny = loud(1)",
            "fn rd() = read_text(\"x\")\ny = rd()",
            "mut n = 0\nfn bump() = n + 1\ny = bump()",
            "fn later() = LATE\ny = later()\nLATE = 1",
            "fn py() = python.import(\"os\")\ny = py()",
            "fn spin(i) = if i > 10000 then i else spin(i + 1)\ny = spin(0)",
            "fn deep(i) = if i == 0 then 0 else 1 + deep(i - 1)\ny = deep(1000)",
            "fn mk() = (x) => x\ny = mk()",
            "fn big() = range(0, 5000).map(it)\ny = big()",
            "fn now_ish() = now()\ny = now_ish()",
            "fn here() = source_path()\ny = here()",
            "fn big() = range(0, 5000).reduce(0, (s, x) => s + x)\ny = big()",
            "fn n() = dataframe({a: [1, 2, 3]}).count()\ny = n()",
            "fn s() = \"x\".repeat(100000)\ny = s()",
        ] {
            let s = folded(src);
            let last = s.iter().rev().find(|st| matches!(st, Stmt::Assign { name, .. } if name == "y")).unwrap();
            assert!(matches!(value_of(last), Expr::Call { .. }), "{src}: {:?}", value_of(last));
        }
        let s = folded("xs = [1, 2]\ny = xs.map(it * 2)");
        assert!(matches!(value_of(&s[1]), Expr::Method { .. }), "{:?}", s[1]);
        // A binding the sandbox could not evaluate is not held, so a call reading it stays.
        let s = folded("fn rd() = read_text(\"x\")\ncfg = rd()\nfn use_cfg() = cfg\ny = use_cfg()");
        assert!(matches!(value_of(&s[3]), Expr::Call { .. }), "{:?}", s[3]);
    }

    /// A top-level binding is evaluated when an attempt needs it — through the bindings it
    /// reads in turn, and through a frame verb's own resolver, which is not the identifier
    /// evaluator — and a call reading one made LATER stays. A mutable global a frame verb
    /// reads is a refusal, never the program's error.
    #[test]
    fn a_top_level_binding_is_held_on_demand() {
        let s = folded("base = 20\nfn g(y) = y * 2\nsep = base + 1\nz = g(sep)");
        assert!(matches!(value_of(&s[3]), Expr::Int(42)), "{:?}", s[3]);
        let s = folded("fn g() = later\ny = g()\nlater = 1");
        assert!(matches!(value_of(&s[1]), Expr::Call { .. }), "{:?}", s[1]);
        let frame = "D = dataframe({ts: [1, 5, 9]})\nfn f(d) = ((x) => x.where(@ts > LO))(d)\nn = f(D).count()\n";
        for lo in ["LO = 4\n", "mut LO = 4\n"] {
            let s = folded(&format!("{lo}{frame}"));
            assert!(matches!(value_of(&s[3]), Expr::Method { .. }), "a frame has no literal: {:?}", s[3]);
        }
    }

    /// A name bound locally is never the global of that name: a body reading its own
    /// parameter `M` folds nothing of a top-level `M`, and a body reading the global does.
    #[test]
    fn a_local_shadowing_a_held_global_is_never_folded() {
        let s = folded("M = {a: 1}\nfn f(M) = M.keys().count()\nfn g(x) = x + M.keys().count()\nfn h(x) = let M = x in M.keys().count()\nfn k(xs) = xs.map(M.keys().count())");
        assert!(matches!(func(&s, "f"), Expr::Method { name, .. } if name == "count"), "{:?}", func(&s, "f"));
        assert!(matches!(func(&s, "g"), Expr::Binary { right, .. } if matches!(**right, Expr::Int(1))), "{:?}", func(&s, "g"));
        assert!(matches!(func(&s, "h"), Expr::Let { body, .. } if matches!(**body, Expr::Method { .. })), "{:?}", func(&s, "h"));
        // `M` in a method body is the global — `it` is the binder there, not `M`.
        assert_eq!(count_nodes(func(&s, "k"), |e| matches!(e, Expr::Int(1))), 1, "{:?}", func(&s, "k"));
    }

    /// A literal receiver, an operator on literals, a field of a held record, an
    /// interpolation of held names, and the branch a literal condition selects all fold.
    #[test]
    fn a_literal_receiver_and_a_constant_branch_fold() {
        let s = folded(
            "fn f() = 1\nM = {t: \"people\", n: 3}\nx = if missing.is_missing() then [1, 2].count() else 0\ny = M.t\nz = \"from {M.t} limit {M.n}\"\nw = 2 * 3 + M.n\nv = missing ?? 7\nu = true or f()",
        );
        assert!(matches!(value_of(&s[2]), Expr::Int(2)), "{:?}", s[2]);
        assert!(matches!(value_of(&s[3]), Expr::Str(t) if t == "people"), "{:?}", s[3]);
        assert!(matches!(value_of(&s[4]), Expr::Str(t) if t == "from people limit 3"), "{:?}", s[4]);
        assert!(matches!(value_of(&s[5]), Expr::Int(9)), "{:?}", s[5]);
        assert!(matches!(value_of(&s[6]), Expr::Int(7)), "{:?}", s[6]);
        assert!(matches!(value_of(&s[7]), Expr::Bool(true)), "{:?}", s[7]);
    }

    /// A genuine raise at an unconditional top-level position is the program's own error,
    /// reported before anything runs; under `if`, `try`, a lambda, a method body or inside
    /// a function it stays a call.
    #[test]
    fn a_raise_is_reported_only_where_the_program_runs_it_unconditionally() {
        let chk = "fn chk(s) = if s.limit == \"bad\" then raise(\"refused\") else s.limit\n";
        assert_eq!(fold_err(&format!("{chk}y = chk({{limit: \"bad\"}})")), "refused");
        assert_eq!(fold_err(&format!("{chk}print(chk({{limit: \"bad\"}}))")), "refused");
        assert_eq!(fold_err(&format!("{chk}y = [chk({{limit: \"bad\"}})]")), "refused");
        // A literal receiver runs its body for each of its elements — `[1].map(…)` meets the
        // raise as surely as `[chk(…)]` does.
        assert_eq!(fold_err(&format!("{chk}y = [1].map(chk({{limit: \"bad\"}}))")), "refused");
        for src in [
            "y = try chk({limit: \"bad\"})",
            "y = if false then chk({limit: \"bad\"}) else 1",
            "y = false and chk({limit: \"bad\"}) == 1",
            "y = 1 ?? chk({limit: \"bad\"})",
            "f = () => chk({limit: \"bad\"})",
            "fn g() = chk({limit: \"bad\"})",
            "y = match 1 { 2 => chk({limit: \"bad\"}), _ => 0 }",
        ] {
            let s = folded(&format!("{chk}{src}"));
            assert!(s.len() == 2, "{src}");
        }
        // A receiver the sandbox does not hold may be empty: the body's raise is the run's.
        let s = folded(&format!("{chk}mut xs = [1]\ny = xs.map(chk({{limit: \"bad\"}}))"));
        assert!(matches!(value_of(&s[2]), Expr::Method { .. }), "{:?}", s[2]);
        // The good spelling folds.
        let s = folded(&format!("{chk}y = chk({{limit: 10}})"));
        assert!(matches!(value_of(&s[1]), Expr::Int(10)), "{:?}", s[1]);
    }

    /// The checker's type map names nodes by address and the compiler reads receiver types
    /// from it after the fold: a fold rewrites in place and never re-allocates a lambda body.
    /// (The first cut did, through `Rc::make_mut` on a body the sandbox's copy of the
    /// function shared — and a DataFrame verb inside a lambda inside a function lost its
    /// receiver type on the VM, which then had no `join` to offer.)
    #[test]
    fn a_fold_leaves_every_other_node_where_the_checker_typed_it() {
        fn lambda_body(stmt: &Stmt) -> &Rc<Expr> {
            if let Stmt::Func { body: Expr::CallValue { callee, .. }, .. } = stmt
                && let Expr::Lambda { body, .. } = &**callee
            {
                return body;
            }
            panic!("not the shape this test builds: {stmt:?}");
        }
        let mut stmts = parsed("fn g(y) = y + 1\nfn on(l, k) = ((x) => g(1) + x + k)(l)\nprint(on(2, 3))");
        let before = Rc::as_ptr(lambda_body(&stmts[1]));
        let mut types = TypeMap::default();
        fold_program_with(&mut stmts, &mut types, false).unwrap_or_else(|e| panic!("{}", e.message));
        let body = lambda_body(&stmts[1]);
        assert_eq!(before, Rc::as_ptr(body), "the lambda body moved");
        // …and the call inside it still folded: `g(1) + x + k` is `(2 + x) + k`.
        assert!(
            matches!(&**body, Expr::Binary { left, .. } if matches!(&**left, Expr::Binary { left: l2, .. } if matches!(**l2, Expr::Int(2)))),
            "{body:?}"
        );
    }

    /// A call handed a record literal is pointed at a clone made for that shape, in which
    /// the questions the shape answers are answered: an absent key is `missing`, so the
    /// branch on it is gone; `keys()` is a literal; a present key is a plain field read.
    #[test]
    fn a_call_is_specialized_for_the_shape_it_is_handed() {
        let s = folded_with(
            "mut RT = 5\nfn f(s) = let {a, limit} = s in if limit.is_missing() then a else a + limit\nfn n(s) = s.keys().count() * (if s.has(\"zz\") then 10 else 2)\ny = f({a: RT})\nz = n({a: RT, b: RT})",
            true,
        );
        assert!(matches!(value_of(&s[3]), Expr::Call { name, .. } if name == "f$1"), "{:?}", s[3]);
        let clone = func(&s, "f$1");
        assert_eq!(count_nodes(clone, |e| matches!(e, Expr::FieldOrMissing { .. } | Expr::If { .. })), 0, "{clone:?}");
        assert_eq!(count_nodes(clone, |e| matches!(e, Expr::Field { name, .. } if name == "a")), 1, "{clone:?}");
        assert!(matches!(value_of(&s[4]), Expr::Call { name, .. } if name == "n$2"), "{:?}", s[4]);
        assert!(matches!(func(&s, "n$2"), Expr::Int(4)), "{:?}", func(&s, "n$2"));
        // The generic function is untouched, for every other caller.
        assert_eq!(count_nodes(func(&s, "f"), |e| matches!(e, Expr::If { .. })), 1);
    }

    /// A method through a record the sandbox holds — the object API — becomes a direct call
    /// of the closure held there, its captured values hoisted to top-level bindings placed
    /// first, and that call is then specialized for the shape it is handed.
    #[test]
    fn a_method_through_a_held_record_is_seen_through() {
        // `t` is baked into the clone `mk` is specialized to for `"z"`; `d`, a dict computed
        // in the body, is what the closure captures — hoisted as the pairs that rebuild it.
        let s = folded_with(
            "fn mk(t) = let d = [[t, 1]].to_dict() in {t: t, go: (s) => \"{d.get(t)}:{s.get(\"x\")}:{s.get(\"y\")}\"}\nM = mk(\"z\")\nmut RT = 1\ny = M.go({x: RT})",
            true,
        );
        assert!(
            matches!(&s[0], Stmt::Assign { name, value: Expr::Method { name: m, .. }, .. } if name == "M$go$d" && m == "to_dict"),
            "the captured value comes first: {:?}",
            s[0]
        );
        let last = s.iter().rev().find(|st| matches!(st, Stmt::Assign { name, .. } if name == "y")).unwrap();
        assert!(matches!(value_of(last), Expr::Call { name, .. } if name.starts_with("M$go$")), "{:?}", value_of(last));
        let name = match value_of(last) {
            Expr::Call { name, .. } => name.clone(),
            _ => unreachable!(),
        };
        let clone = func(&s, &name);
        // `s.get("y")` on the shape `{x}` is `missing`, `s.get("x")` a field read, and
        // `d.get("z")` — the hoisted dict, a literal key — folded to its value.
        assert_eq!(count_nodes(clone, |e| matches!(e, Expr::Method { name, .. } if name == "get")), 0, "{clone:?}");
        assert_eq!(count_nodes(clone, |e| matches!(e, Expr::Int(1))), 1, "{clone:?}");
        // A field the record's own type owns is not a closure call: `keys` stays a method.
        let s = folded_with("fn mk(t) = {t: t}\nM = mk(\"z\")\nmut RT = 1\ny = M.keys().map(it + RT)", true);
        let last = s.iter().rev().find(|st| matches!(st, Stmt::Assign { name, .. } if name == "y")).unwrap();
        assert!(matches!(value_of(last), Expr::Method { .. }), "{:?}", value_of(last));
    }

    /// Specialization follows a known argument into the callees a clone calls, and stops
    /// at its caps: eight clones per function, and no more.
    #[test]
    fn specialization_is_transitive_and_capped() {
        let s = folded_with("fn g(s) = s.get(\"b\")\nfn f(s) = g(s)\nmut RT = 1\ny = f({a: RT})", true);
        let clone = func(&s, "f$1");
        assert!(matches!(clone, Expr::Call { name, .. } if name == "g$2"), "{clone:?}");
        assert!(matches!(func(&s, "g$2"), Expr::Missing), "{:?}", func(&s, "g$2"));
        let mut src = String::from("fn f(s) = s.get(\"k\")\nmut RT = 1\n");
        for i in 0..9 {
            src.push_str(&format!("y{i} = f({{k{i}: RT}})\n"));
        }
        let s = folded_with(&src, true);
        let clones = s.iter().filter(|st| matches!(st, Stmt::Func { name, .. } if name.starts_with("f$"))).count();
        assert_eq!(clones, 8);
        let ninth = s.iter().find(|st| matches!(st, Stmt::Assign { name, .. } if name == "y8")).unwrap();
        assert!(matches!(value_of(ninth), Expr::Call { name, .. } if name == "f"), "{:?}", value_of(ninth));
    }

    /// A clause builder — `items()` of a shaped record, reduced with a lambda that reads
    /// each pair — becomes its text: the `reduce` over the one element the shape produces is
    /// unrolled to a `let`, the pair answers `c[0]` and `c[1]`, the accumulator's literal
    /// answers `a.s` and `a.n`, and the interpolation of literals folds; only the value's
    /// field read remains.
    #[test]
    fn a_clause_over_a_shape_becomes_its_text() {
        let s = folded_with(
            "mut RT = 1\nfn clause(w) = w.items().reduce({s: \"\", n: 1}, (a, c) => {s: \"{a.s}{c[0]} = ${a.n}\", n: a.n + 1, v: c[1]})\ny = clause({city: RT})",
            true,
        );
        let clone = func(&s, "clause$1");
        assert_eq!(count_nodes(clone, |e| matches!(e, Expr::Method { .. })), 0, "{clone:?}");
        assert_eq!(count_nodes(clone, |e| matches!(e, Expr::Str(t) if t == "city = $1")), 1, "{clone:?}");
        assert_eq!(count_nodes(clone, |e| matches!(e, Expr::Int(2))), 1, "{clone:?}");
        assert_eq!(count_nodes(clone, |e| matches!(e, Expr::Field { name, .. } if name == "city")), 1, "{clone:?}");
    }

    /// A program with no function of its own is untouched, cheaply.
    #[test]
    fn nothing_to_fold_is_nothing_done() {
        let s = folded("x = 1 + 2\nprint(x)");
        assert!(matches!(value_of(&s[0]), Expr::Binary { .. }));
    }

    /// Every entry the fold leaves in the checker's type map names a node alive in the
    /// program. The map is keyed by address, the compiler routes a method call by its
    /// receiver's entry, and an entry for a freed node answers for whatever is allocated
    /// there next. (The field build's §1.51: a clone took its types through the original
    /// body's addresses, one of which a fold had freed and a typed node reused, so the
    /// clone's `c` took that node's type and `c.count()` became the module's three-argument `count`
    /// — for one program, and for no smaller one.) So a function's types are snapshotted
    /// by value when it is recorded, a clone takes them from the snapshot, and every node a
    /// rewrite drops or moves — the slot included, and a statement's root when the program
    /// grows — is forgotten or re-keyed.
    #[test]
    fn every_type_the_fold_leaves_names_a_live_node() {
        // The object API over a model, five distinct closures reaching one higher-order
        // function, and a clause builder's `c.count()` beside a module `count` of three
        // parameters — the reproducer's shape, without its ORM.
        let orm = "fn count(m, spec, target) = \"select count(*) from {m.table}\"\n\
            fn clauses(w) = w.items().map((p) => \"{p[0]} = ?\")\n\
            fn sql(m, spec) = let {where} = spec in let w = where ?? {} in let c = clauses(w) in let two = c.count() == 2 in \
            if c.count() == 0 then {sql: \"select * from {m.table}\", two: two} \
            else {sql: \"select * from {m.table} where {c.join(\\\" and \\\")}\", two: two}\n\
            fn define(spec) = let m = {table: spec.table, columns: spec.columns} in \
            {table: m.table, columns: m.columns, by_key: \"select * from {m.table} where id = $1\", \
            sql: (spec) => sql(m, spec), prepare: (spec) => {sql: sql(m, spec).sql}, count: (spec) => count(m, spec, \"n\")}\n\
            M = define({table: \"people\", columns: [\"id\", \"name\", \"age\", \"city\"]})\n\
            mut RC = \"x\"\n\
            EMPTY = {}\n\
            fn t(label, f) = do {\n  _ = range(0, 3).map(f()).last()\n  \
            ms = range(0, 2).map(let k = it in do {\n    _ = range(0, 3).map(f()).last()\n    1.0\n  })\n  \
            print(\"{label} {ms.count()} {f()}\")\n}\n\
            fn main() = do {\n  _ = t(\"by_key\", () => M.by_key)\n  \
            _ = t(\"prepare\", () => M.prepare({where: {city: \"x\"}}).sql)\n  \
            _ = t(\"empty\", () => M.sql(EMPTY).sql)\n  \
            _ = t(\"1 clause\", () => M.sql({where: {city: RC}}).sql)\n  \
            _ = t(\"count\", () => M.count({where: {city: RC}}))\n  \
            t(\"2 clauses\", () => M.sql({where: {city: RC, \"age >\": 30}}).sql)\n}";
        for src in [
            "mut RT = 5\nfn f(s) = let {a, limit} = s in if limit.is_missing() then a else a + limit\nfn n(s) = s.keys().count() * (if s.has(\"zz\") then 10 else 2)\ny = f({a: RT})\nz = n({a: RT, b: RT})",
            "fn mk(t) = let d = [[t, 1]].to_dict() in {t: t, go: (s) => \"{d.get(t)}:{s.get(\"x\")}:{s.get(\"y\")}\"}\nM = mk(\"z\")\nmut RT = 1\ny = M.go({x: RT})",
            "mut RT = 1\nfn clause(w) = w.items().reduce({s: \"\", n: 1}, (a, c) => {s: \"{a.s}{c[0]} = ${a.n}\", n: a.n + 1, v: c[1]})\ny = clause({city: RT})",
            orm,
        ] {
            let mut stmts = parsed(src);
            let mut types = crate::types::check(&stmts).unwrap_or_else(|e| panic!("{src}: {}", e.message));
            let before = types.len();
            assert!(before > 0, "the checker typed nothing in {src}");
            fold_program_with(&mut stmts, &mut types, true).unwrap_or_else(|e| panic!("{}", e.message));
            let mut live = std::collections::HashSet::new();
            for s in &stmts {
                crate::visit::walk_stmt(s, &mut |x| {
                    live.insert(x as *const Expr);
                });
            }
            let stale = types.keys().filter(|k| !live.contains(*k)).count();
            assert_eq!(stale, 0, "{src}: {stale} of {} entries name a freed node ({before} before the fold)", types.len());
        }
    }
}

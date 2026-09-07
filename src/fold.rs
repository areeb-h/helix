//! Constant folding of pure calls with literal arguments (ADR 0050).
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
//! sandbox abandoned is not tried again in that program. A refusal abandons the fold and the call stays
//! exactly as written — the runtime runs it as it always did, so a fold can never change what
//! a program computes, only when.
//!
//! A GENUINE raise during a fold — the callee refusing its argument — is the program's own
//! error. At a position that runs unconditionally at the top level it is reported before
//! anything runs, as a type error is (`helix check` sees it); anywhere else — under `if`,
//! `match`, `try`, `and`/`or`/`??`, in a lambda or a method's body, inside a function — the
//! call stays and raises at run time as before.
//!
//! The pass runs AFTER the checker and the receiver-directed rewrite (`ufcs`), in every
//! pipeline that runs, checks or bundles a program (`main::run_program`, the compile paths,
//! `check_file_structured`/`check_file_capture`, `bundle::build`). The checker types what the
//! programmer wrote — a fold never adds precision a call lacked, so `launder(true)` typed
//! `Any` stays `Any`, and a type error outranks a raise, since the fold only runs on a
//! program the checker accepted — and every engine runs the one program the fold produced.
//!
//! Two rules keep the pass cheap and honest. A top-level binding is evaluated ON DEMAND —
//! when an attempt meets its name — never eagerly, so a program whose calls fold nothing
//! pays for nothing but the walk. And a fold rewrites IN PLACE: it replaces one node by its
//! literal and never re-allocates another, because the checker's type map names nodes by
//! address and the compiler reads receiver types from it after the fold.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::{BinOp, Expr, InterpPart, Stmt};
use crate::error::HelixError;
use crate::interp::Interp;
use crate::value::Value;

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
const MAX_LITERAL_NODES: usize = 4_096;

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
struct Sandbox {
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
}

/// How an attempt ended without the program's own raise.
enum Outcome {
    Value(Value),
    /// The sandbox refused the evaluation itself — the callee is futile here.
    Abandoned,
    /// A name the sandbox does not hold and could not bind — the callee may be fine with
    /// other arguments.
    Unheld,
}

impl Sandbox {
    fn new(program: &[Stmt]) -> Self {
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
        let mut funcs: Vec<Stmt> = program
            .iter()
            .filter(|s| matches!(s, Stmt::Func { name, .. } if !assigned.contains(name.as_str())))
            .cloned()
            .collect();
        // The sandbox's copies own their lambda bodies. `fold_expr` rewrites the program's
        // bodies in place, and a body shared with the sandbox would have to be re-allocated
        // to be written — moving every node inside it away from the address the checker's
        // type map knows it by.
        for f in &mut funcs {
            if let Stmt::Func { body, .. } = f {
                unshare_lambdas(body);
            }
        }
        let _ = interp.run(&funcs);
        Sandbox { interp, program_fuel: FUEL_PER_PROGRAM, pending: HashMap::new(), futile: HashSet::new() }
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
        if self.pending.contains_key(name) {
            self.ensure(name, done);
        }
        self.interp.global_is_record(name)
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

/// Fold every foldable call in `stmts`, in program order. `Err` is a raise the program would
/// meet unconditionally at the top level.
pub fn fold_program(stmts: &mut [Stmt]) -> Result<(), HelixError> {
    // `HELIX_NOFOLD=1` runs every call at run time, for an A/B — `HELIX_NOJIT`'s twin.
    if std::env::var_os("HELIX_NOFOLD").is_some() || !stmts.iter().any(|s| matches!(s, Stmt::Func { .. })) {
        return Ok(());
    }
    let mut sb = Sandbox::new(stmts);
    for i in 0..stmts.len() {
        // The statements before this one are where a name the sandbox is asked for is bound.
        let (done, rest) = stmts.split_at_mut(i);
        let stmt = &mut rest[0];
        match stmt {
            Stmt::Func { body, .. } => fold_expr(body, &mut sb, done, false)?,
            Stmt::Assign { value, .. } | Stmt::Destructure { value, .. } => fold_expr(value, &mut sb, done, true)?,
            Stmt::Expr(e) => fold_expr(e, &mut sb, done, true)?,
            Stmt::Import { .. } => {}
        }
        if matches!(stmt, Stmt::Assign { mutable: false, .. } | Stmt::Destructure { mutable: false, .. }) {
            sb.note_top(i, stmt);
        }
    }
    Ok(())
}

/// Fold inside `e` (children first), then `e` itself where it is a candidate.
/// `unconditional`: whether this position runs whenever its top-level statement does.
fn fold_expr(e: &mut Expr, sb: &mut Sandbox, done: &[Stmt], unconditional: bool) -> Result<(), HelixError> {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing
        | Expr::Ident { .. } | Expr::Column { .. } => {}
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    fold_expr(x, sb, done, unconditional)?;
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                fold_expr(x, sb, done, unconditional)?;
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                fold_expr(v, sb, done, unconditional)?;
            }
        }
        Expr::RecordUpdate { parts, .. } => {
            for p in parts {
                fold_expr(p.expr_mut(), sb, done, unconditional)?;
            }
        }
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } => fold_expr(recv, sb, done, unconditional)?,
        Expr::Unary { expr, .. } => fold_expr(expr, sb, done, unconditional)?,
        Expr::Binary { op, left, right, .. } => {
            fold_expr(left, sb, done, unconditional)?;
            let right_runs = unconditional && !matches!(op, BinOp::And | BinOp::Or | BinOp::Coalesce);
            fold_expr(right, sb, done, right_runs)?;
        }
        Expr::Call { args, .. } => {
            for a in args {
                fold_expr(a, sb, done, unconditional)?;
            }
        }
        // A method's arguments may be bodies run per element (`it`-forms), so nothing in
        // them is unconditional.
        Expr::Method { recv, args, named, .. } => {
            fold_expr(recv, sb, done, unconditional)?;
            for a in args {
                fold_expr(a, sb, done, false)?;
            }
            for (_, v) in named {
                fold_expr(v, sb, done, false)?;
            }
        }
        Expr::CallValue { callee, args, .. } => {
            fold_expr(callee, sb, done, unconditional)?;
            for a in args {
                fold_expr(a, sb, done, unconditional)?;
            }
        }
        Expr::Index { recv, index, .. } => {
            fold_expr(recv, sb, done, unconditional)?;
            fold_expr(index, sb, done, unconditional)?;
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            fold_expr(recv, sb, done, unconditional)?;
            for part in [start, stop, step].into_iter().flatten() {
                fold_expr(part, sb, done, unconditional)?;
            }
        }
        Expr::Lambda { defaults, body, .. } => {
            for d in defaults {
                fold_expr(d, sb, done, false)?;
            }
            // In place or not at all: a body another owner shares (the sandbox holds a
            // closure over it) would be re-allocated by `make_mut`, and the checker's type
            // map knows every node inside by its address.
            if let Some(b) = Rc::get_mut(body) {
                fold_expr(b, sb, done, false)?;
            }
        }
        Expr::Let { bindings, body, .. } => {
            for (_, v) in bindings {
                fold_expr(v, sb, done, unconditional)?;
            }
            fold_expr(body, sb, done, unconditional)?;
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            fold_expr(cond, sb, done, unconditional)?;
            fold_expr(then_branch, sb, done, false)?;
            fold_expr(else_branch, sb, done, false)?;
        }
        Expr::Try { expr, .. } => fold_expr(expr, sb, done, false)?,
        Expr::Match { scrutinee, arms, .. } => {
            fold_expr(scrutinee, sb, done, unconditional)?;
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    fold_expr(g, sb, done, false)?;
                }
                fold_expr(&mut arm.body, sb, done, false)?;
            }
        }
    }
    let Some(label) = candidate_label(e, sb, done) else {
        return Ok(());
    };
    if sb.futile.contains(&label) {
        return Ok(());
    }
    match sb.attempt(e, done) {
        Ok(Outcome::Value(v)) => {
            let mut budget = MAX_LITERAL_NODES;
            if let Some(lit) = to_expr(&v, &mut budget) {
                *e = lit;
            }
        }
        Ok(Outcome::Abandoned) => {
            sb.futile.insert(label);
        }
        Ok(Outcome::Unheld) => {}
        Err(raise) => {
            if unconditional {
                return Err(raise);
            }
        }
    }
    Ok(())
}

/// A call to one of the program's own functions, or a call through a record the sandbox
/// holds — with arguments that mention nothing the sandbox lacks — and its label for the
/// futility memo: the function's name, or `record.method`.
fn candidate_label(e: &Expr, sb: &mut Sandbox, done: &[Stmt]) -> Option<String> {
    match e {
        Expr::Call { name, args, .. } if sb.is_user_fn(name) && args.iter().all(|a| foldable_arg(a, sb)) => {
            Some(name.clone())
        }
        Expr::Method { recv, name, args, named, .. }
            if named.is_empty() && args.iter().all(|a| foldable_arg(a, sb)) =>
        {
            match &**recv {
                Expr::Ident { name: r, .. } if sb.holds_record(r, done) => Some(format!("{r}.{name}")),
                _ => None,
            }
        }
        _ => None,
    }
}

/// An argument the sandbox can evaluate: no column reference, no free name it does not hold.
fn foldable_arg(a: &Expr, sb: &Sandbox) -> bool {
    let mut bound: Vec<String> = Vec::new();
    known_closed(a, sb, &mut bound)
}

fn known_closed(e: &Expr, sb: &Sandbox, bound: &mut Vec<String>) -> bool {
    match e {
        Expr::Column { .. } => false,
        Expr::Ident { name, .. } => bound.iter().any(|b| b == name) || sb.holds(name) || sb.is_user_fn(name),
        Expr::Lambda { params, defaults, body, .. } => {
            let mark = bound.len();
            bound.extend(params.iter().cloned());
            let ok = defaults.iter().all(|d| known_closed(d, sb, bound)) && known_closed(body, sb, bound);
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

/// Give every lambda under `e` a body of its own — the sandbox's copy of a function must
/// not share the program's nodes (see [`Sandbox::new`]).
fn unshare_lambdas(e: &mut Expr) {
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

/// The literal for a value, when it has one: numbers, strings, booleans, `missing`, and
/// arrays, tuples and records of those — within `budget` nodes. A function, a frame, a
/// tensor, a dict, a rational, bytes: no literal, no fold.
fn to_expr(v: &Value, budget: &mut usize) -> Option<Expr> {
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
    fn folded(src: &str) -> Vec<Stmt> {
        let mut stmts = parsed(src);
        fold_program(&mut stmts).unwrap_or_else(|e| panic!("{}", e.message));
        stmts
    }
    fn fold_err(src: &str) -> String {
        let mut stmts = parsed(src);
        match fold_program(&mut stmts) {
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
        if let Stmt::Func { body, .. } = &s[1] {
            assert!(matches!(body, Expr::Binary { left, right, .. } if matches!(**left, Expr::Int(2)) && matches!(**right, Expr::Call { .. })), "{body:?}");
        } else {
            panic!();
        }
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

    /// A genuine raise at an unconditional top-level position is the program's own error,
    /// reported before anything runs; under `if`, `try`, a lambda, a method body or inside
    /// a function it stays a call.
    #[test]
    fn a_raise_is_reported_only_where_the_program_runs_it_unconditionally() {
        let chk = "fn chk(s) = if s.limit == \"bad\" then raise(\"refused\") else s.limit\n";
        assert_eq!(fold_err(&format!("{chk}y = chk({{limit: \"bad\"}})")), "refused");
        assert_eq!(fold_err(&format!("{chk}print(chk({{limit: \"bad\"}}))")), "refused");
        assert_eq!(fold_err(&format!("{chk}y = [chk({{limit: \"bad\"}})]")), "refused");
        for src in [
            "y = try chk({limit: \"bad\"})",
            "y = if false then chk({limit: \"bad\"}) else 1",
            "y = false and chk({limit: \"bad\"}) == 1",
            "y = 1 ?? chk({limit: \"bad\"})",
            "f = () => chk({limit: \"bad\"})",
            "y = [1].map(chk({limit: \"bad\"}))",
            "fn g() = chk({limit: \"bad\"})",
            "y = match 1 { 2 => chk({limit: \"bad\"}), _ => 0 }",
        ] {
            let s = folded(&format!("{chk}{src}"));
            assert!(s.len() == 2, "{src}");
        }
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
        fold_program(&mut stmts).unwrap_or_else(|e| panic!("{}", e.message));
        let body = lambda_body(&stmts[1]);
        assert_eq!(before, Rc::as_ptr(body), "the lambda body moved");
        // …and the call inside it still folded: `g(1) + x + k` is `(2 + x) + k`.
        assert!(
            matches!(&**body, Expr::Binary { left, .. } if matches!(&**left, Expr::Binary { left: l2, .. } if matches!(**l2, Expr::Int(2)))),
            "{body:?}"
        );
    }

    /// A program with no function of its own is untouched, cheaply.
    #[test]
    fn nothing_to_fold_is_nothing_done() {
        let s = folded("x = 1 + 2\nprint(x)");
        assert!(matches!(value_of(&s[0]), Expr::Binary { .. }));
    }
}

//! Tree-walking interpreter.

use std::rc::Rc;

use rustc_hash::FxHashMap;

use crate::ast::{BinOp, Expr, Stmt, UnOp};
use crate::backend::Df;
use crate::dataframe;
use crate::error::{suggest, HelixError};
use crate::symbol::Symbol;
use crate::tensor;
use crate::value::Value;

struct Binding {
    value: Value,
    mutable: bool,
}

/// Guards against runaway recursion with a graceful error well before the
/// dedicated 2 GiB eval thread's stack overflows. Calibrated conservatively: a
/// debug build costs ~25 KB of native stack per Helix call, so even a complex
/// function body stays comfortably inside the stack at this depth. See `main.rs`.
const MAX_CALL_DEPTH: usize = 20_000;

/// Ceiling on a single interpolated string's byte length. Interpolation nests, so
/// a doubling loop could otherwise grow a string until the allocator aborts; 1 GiB
/// is far past any legitimate message and leaves headroom under typical limits.
pub(crate) const MAX_STRING_LEN: usize = 1 << 30;

pub struct Interp {
    env: FxHashMap<String, Binding>,
    depth: usize,
    /// Names bound at the top level (plus the seeded constants). A lambda captures
    /// only its *non-global* free variables — globals persist in `env` and resolve
    /// at call time, so capturing them would be wasteful and (for `mut` globals)
    /// wrong. Populated by `bind` while `depth == 0`.
    globals: std::collections::HashSet<String>,
}

/// Result of running a statement: the value (for REPL auto-printing) and
/// whether it was a bare expression worth echoing.
pub struct StmtOutcome {
    pub value: Value,
    pub is_expr: bool,
}

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}

impl Interp {
    pub fn new() -> Self {
        let mut env = FxHashMap::default();
        // Math constants are predefined immutable bindings.
        env.insert(
            "pi".to_string(),
            Binding { value: Value::Float(std::f64::consts::PI), mutable: false },
        );
        env.insert(
            "e".to_string(),
            Binding { value: Value::Float(std::f64::consts::E), mutable: false },
        );
        env.insert(
            "inf".to_string(),
            Binding { value: Value::Float(f64::INFINITY), mutable: false },
        );
        // The `python` interop entry point — an opaque namespace handle. Always
        // present; without the `python` build feature its methods return a clean
        // "rebuild with --features python" error (see `crate::python`).
        env.insert(
            "python".to_string(),
            Binding {
                value: Value::PyObject(std::rc::Rc::new(crate::python::PyHandle::namespace())),
                mutable: false,
            },
        );
        let globals = env.keys().cloned().collect();
        Interp { env, depth: 0, globals }
    }

    pub fn run(&mut self, program: &[Stmt]) -> Result<(), HelixError> {
        for stmt in program {
            self.exec(stmt)?;
        }
        Ok(())
    }

    pub fn exec(&mut self, stmt: &Stmt) -> Result<StmtOutcome, HelixError> {
        match stmt {
            Stmt::Assign {
                name,
                mutable,
                value,
                line,
                col,
                ..
            } => {
                let v = self.eval(value)?;
                self.bind(name, v.clone(), *mutable, *line, *col)?;
                Ok(StmtOutcome {
                    value: Value::Unit,
                    is_expr: false,
                })
            }
            Stmt::Destructure {
                names,
                mutable,
                value,
                line,
                col,
                ..
            } => {
                let v = self.eval(value)?;
                let parts = destructure_parts(&v, names.len(), *line, *col)?;
                for (n, val) in names.iter().zip(parts) {
                    self.bind(n, val, *mutable, *line, *col)?;
                }
                Ok(StmtOutcome {
                    value: Value::Unit,
                    is_expr: false,
                })
            }
            Stmt::Func {
                name,
                params,
                body,
                line,
                col,
                ..
            } => {
                // Annotations are checker-only; the interpreter needs just names.
                let param_names: Vec<String> = params.iter().map(|(n, _)| n.clone()).collect();
                let f = Value::Function(Rc::new(crate::value::FuncVal {
                    params: Rc::new(param_names),
                    body: Rc::new(body.clone()),
                    captured: Rc::new(Vec::new()), // top-level fn: free names are globals
                }));
                self.bind(name, f, false, *line, *col)?;
                Ok(StmtOutcome {
                    value: Value::Unit,
                    is_expr: false,
                })
            }
            // Imports are resolved and stripped by the module loader before
            // execution; reaching here means one was used outside a file (e.g. the
            // REPL), which isn't supported.
            Stmt::Import { line, col, .. } => Err(HelixError::new(
                "`import` is only allowed at the top level of a file",
                *line,
                *col,
            )
            .hint("run a multi-module program from a file, not the REPL.")),
            Stmt::Expr(e) => {
                let v = self.eval(e)?;
                Ok(StmtOutcome {
                    value: v,
                    is_expr: true,
                })
            }
        }
    }

    fn bind(
        &mut self,
        name: &str,
        v: Value,
        mutable: bool,
        line: usize,
        col: usize,
    ) -> Result<(), HelixError> {
        // Top-level bindings are globals (a lambda never captures these).
        if self.depth == 0 {
            self.globals.insert(name.to_string());
        }
        match self.env.get(name) {
            None => {
                self.env.insert(
                    name.to_string(),
                    Binding {
                        value: v,
                        mutable,
                    },
                );
                Ok(())
            }
            Some(existing) => {
                if mutable {
                    // `mut x = ...` on an existing name re-declares it as mutable.
                    self.env.insert(
                        name.to_string(),
                        Binding { value: v, mutable: true },
                    );
                    Ok(())
                } else if existing.mutable {
                    // plain reassignment to a mutable binding
                    self.env.get_mut(name).unwrap().value = v;
                    Ok(())
                } else {
                    Err(HelixError::new(
                        format!("`{}` is immutable and cannot be reassigned", name),
                        line,
                        col,
                    )
                    .hint(format!(
                        "declare it as mutable up front with `mut {} = ...` if it needs to change.",
                        name
                    )))
                }
            }
        }
    }

    fn eval(&mut self, e: &Expr) -> Result<Value, HelixError> {
        match e {
            Expr::Int(v) => Ok(Value::Int(*v)),
            Expr::Float(v) => Ok(Value::Float(*v)),
            Expr::Str(s) => Ok(Value::Str(Rc::new(s.clone()))),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Missing => Ok(Value::Missing),
            Expr::Column { name, line, col } => Err(HelixError::new(
                format!("`@{name}` is a column reference, only valid inside a DataFrame operation"),
                *line,
                *col,
            )
            .hint("use `@column` inside a verb like `df.where(...)`, `df.select(...)`, or `df.group(...)`.")),
            Expr::Interp(parts) => {
                let mut s = String::new();
                for part in parts {
                    match part {
                        crate::ast::InterpPart::Lit(t) => s.push_str(t),
                        crate::ast::InterpPart::Expr(e, spec) => {
                            let v = self.eval(e)?;
                            let (l, c) = e.position();
                            match spec {
                                Some(fs) => s.push_str(
                                    &fs.apply(&v).map_err(|m| HelixError::new(m, l, c))?,
                                ),
                                // Hot path: format scalars straight into `s`, no throwaway String.
                                None => crate::value::write_value(&mut s, &v, l, c)?,
                            }
                        }
                    }
                    // Interpolation can nest (a value's display may itself be an
                    // interpolated string), so a `s = "{s}{s}"` loop doubles the
                    // string each frame — bounded only by recursion depth. Cap the
                    // accumulated length so runaway growth errors cleanly instead of
                    // aborting the allocator.
                    if s.len() > MAX_STRING_LEN {
                        let (l, c) = e.position();
                        return Err(HelixError::new(
                            format!("interpolated string exceeds {MAX_STRING_LEN} bytes"),
                            l,
                            c,
                        )
                        .hint("build large text incrementally or write it to a file instead."));
                    }
                }
                Ok(Value::Str(Rc::new(s)))
            }
            Expr::Ident { name, line, col } => match self.env.get(name) {
                Some(b) => Ok(b.value.clone()),
                None => {
                    let names: Vec<&str> = self.env.keys().map(|s| s.as_str()).collect();
                    let mut err = HelixError::new(
                        format!("`{}` is not defined", name),
                        *line,
                        *col,
                    );
                    if let Some(s) = suggest(name, &names) {
                        err = err.hint(format!("did you mean `{}`?", s));
                    } else {
                        err = err.hint(format!("assign it first, e.g. `{} = ...`.", name));
                    }
                    Err(err)
                }
            },
            Expr::Array(items) => {
                let mut vals = Vec::with_capacity(items.len());
                for it in items {
                    vals.push(self.eval(it)?);
                }
                Ok(Value::array_sniff(vals))
            }
            Expr::Tuple(items) => {
                let mut vals = Vec::with_capacity(items.len());
                for it in items {
                    vals.push(self.eval(it)?);
                }
                Ok(Value::Tuple(Rc::new(vals)))
            }
            Expr::Record(fields) => {
                let mut vals = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    vals.push((Symbol::intern(k), self.eval(v)?));
                }
                Ok(Value::Record(Rc::new(vals)))
            }
            Expr::RecordUpdate { base, fields, line, col } => {
                let base_v = self.eval(base)?;
                let Value::Record(base_fields) = base_v else {
                    return Err(HelixError::new(
                        format!("`...` record update needs a record, got a {}", base_v.type_name()),
                        *line,
                        *col,
                    )
                    .hint("the spread base must be a record, e.g. `{ ...resp, status: 500 }`."));
                };
                // Clone the base fields, then set (override) or append each update field, in
                // order — a later field wins over a same-named base field or earlier update.
                let mut out: Vec<(Symbol, Value)> = (*base_fields).clone();
                for (k, ve) in fields {
                    let sym = Symbol::intern(k);
                    let val = self.eval(ve)?;
                    match out.iter_mut().find(|(s, _)| *s == sym) {
                        Some(slot) => slot.1 = val,
                        None => out.push((sym, val)),
                    }
                }
                Ok(Value::Record(Rc::new(out)))
            }
            Expr::Field {
                recv,
                name,
                line,
                col,
            } => {
                let r = self.eval(recv)?;
                eval_field(&r, Symbol::intern(name), *line, *col)
            }
            Expr::Unary { op, expr, line, col } => {
                let v = self.eval(expr)?;
                self.eval_unary(op, v, *line, *col)
            }
            Expr::Binary {
                op,
                left,
                right,
                line,
                col,
            } => {
                // Boolean ops short-circuit and use three-valued logic so that
                // `missing` propagates only when the result isn't already
                // determined (`true or missing` -> true; `false or missing` ->
                // missing). See ADR 0001.
                match op {
                    BinOp::And => {
                        let l = tri(&self.eval(left)?, *line, *col)?;
                        if l == Some(false) {
                            return Ok(Value::Bool(false)); // determined; skip right
                        }
                        let r = tri(&self.eval(right)?, *line, *col)?;
                        Ok(match (l, r) {
                            (_, Some(false)) => Value::Bool(false),
                            (Some(true), Some(true)) => Value::Bool(true),
                            _ => Value::Missing,
                        })
                    }
                    BinOp::Or => {
                        let l = tri(&self.eval(left)?, *line, *col)?;
                        if l == Some(true) {
                            return Ok(Value::Bool(true)); // determined; skip right
                        }
                        let r = tri(&self.eval(right)?, *line, *col)?;
                        Ok(match (l, r) {
                            (_, Some(true)) => Value::Bool(true),
                            (Some(false), Some(false)) => Value::Bool(false),
                            _ => Value::Missing,
                        })
                    }
                    // `a ?? b`: evaluate `b` only when `a` is missing.
                    BinOp::Coalesce => {
                        let l = self.eval(left)?;
                        if matches!(l, Value::Missing) {
                            self.eval(right)
                        } else {
                            Ok(l)
                        }
                    }
                    _ => {
                        let l = self.eval(left)?;
                        let r = self.eval(right)?;
                        eval_binary(op, l, r, *line, *col)
                    }
                }
            }
            Expr::Call {
                name,
                args,
                line,
                col,
            } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a)?);
                }
                // A user binding of this name shadows a builtin of the same name —
                // defining `fn sign(..)` calls *your* function, not the math builtin.
                if let Some(b) = self.env.get(name) {
                    return match &b.value {
                        Value::Function(g) => {
                            let g = g.clone();
                            self.call_function(name, &g, vals, *line, *col)
                        }
                        // The name is bound, but not to something callable.
                        other => Err(HelixError::new(
                            format!("`{}` is a {}, not a function", name, other.type_name()),
                            *line,
                            *col,
                        )
                        .hint("only functions and the built-ins `print`/`dna`/`range` can be called.")),
                    };
                }
                // No user binding → the builtin.
                if crate::registry::lookup(name).is_some() {
                    return self.call_builtin(name, vals, *line, *col);
                }
                // Unknown — suggest the closest known function name.
                let mut cands: Vec<String> = crate::registry::names().map(|s| s.to_string()).collect();
                cands.extend(
                    self.env
                        .iter()
                        .filter(|(_, b)| matches!(b.value, Value::Function(_)))
                        .map(|(k, _)| k.clone()),
                );
                let cand_refs: Vec<&str> = cands.iter().map(|s| s.as_str()).collect();
                let mut err =
                    HelixError::new(format!("`{}` is not a known function", name), *line, *col);
                if let Some(s) = suggest(name, &cand_refs) {
                    err = err.hint(format!("did you mean `{}`?", s));
                }
                Err(err)
            }
            Expr::CallValue {
                callee,
                args,
                line,
                col,
            } => {
                let callee_v = self.eval(callee)?;
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a)?);
                }
                let label = callee.call_label();
                match callee_v {
                    Value::Function(g) => self.call_function(&label, &g, vals, *line, *col),
                    // The expression evaluated to something that isn't callable.
                    other => Err(HelixError::new(
                        format!("`{}` is a {}, not a function", label, other.type_name()),
                        *line,
                        *col,
                    )
                    .hint("only functions can be called this way, e.g. `(rec.handler)(x)`.")),
                }
            }
            Expr::Method {
                recv,
                name,
                args,
                line,
                col,
                ..
            } => {
                let recv_v = self.eval(recv)?;
                // `is_missing` is universal — every value answers it. DataFrame and
                // GroupBy receivers are routed to their verb dispatch below, which
                // never reaches the universal handler in `call_method`, so intercept
                // it here; a frame/group is never `missing`, so the answer is `false`.
                if name == "is_missing"
                    && matches!(recv_v, Value::DataFrame(_) | Value::GroupBy(_))
                {
                    return df_is_missing(args.is_empty(), *line, *col);
                }
                // DataFrame / GroupBy verbs take their column arguments
                // *unevaluated* (column names and predicates), so they're routed
                // before the array comprehension path.
                match &recv_v {
                    Value::DataFrame(lf) => {
                        return self.eval_df_method((**lf).clone(), name, args, *line, *col);
                    }
                    Value::GroupBy(g) => {
                        return self.eval_groupby_method(
                            g.handle.clone(),
                            g.keys.clone(),
                            name,
                            args,
                            *line,
                            *col,
                        );
                    }
                    _ => {}
                }
                // Comprehension-style methods take an *unevaluated* expression
                // that is run once per element with `it` bound to the element.
                if matches!(
                    name.as_str(),
                    "map" | "filter" | "where" | "reduce" | "scan" | "any" | "all"
                ) {
                    return self.eval_comprehension(&recv_v, name, args, *line, *col);
                }
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a)?);
                }
                call_method(&recv_v, name, vals, *line, *col)
            }
            Expr::Index {
                recv,
                index,
                line,
                col,
            } => {
                let recv_v = self.eval(recv)?;
                let idx_v = self.eval(index)?;
                eval_index(&recv_v, &idx_v, *line, *col)
            }
            Expr::Slice {
                recv,
                start,
                stop,
                step,
                line,
                col,
            } => {
                let recv_v = self.eval(recv)?;
                // Evaluate each present bound; a missing bound propagates.
                let part = |this: &mut Self, e: &Option<Box<Expr>>| -> Result<Option<i64>, HelixError> {
                    match e {
                        None => Ok(None),
                        Some(e) => match this.eval(e)? {
                            Value::Int(i) => Ok(Some(i)),
                            Value::Missing => Ok(None), // treat missing bound as omitted
                            other => Err(type_err("slice bound", "an integer", &other, *line, *col)),
                        },
                    }
                };
                let s = part(self, start)?;
                let e = part(self, stop)?;
                let st = part(self, step)?.unwrap_or(1);
                if st == 0 {
                    return Err(HelixError::new("slice step cannot be zero", *line, *col));
                }
                eval_slice(&recv_v, s, e, st, *line, *col)
            }
            Expr::Lambda { params, body, .. } => {
                // Capture the lambda's free, non-global variables by value — its
                // lexical environment — so a returned or stored closure still sees
                // them after the defining call has returned.
                let captured: Vec<(String, Value)> = crate::bytecode::free_names(params, body)
                    .into_iter()
                    .filter(|n| !self.globals.contains(n))
                    .filter_map(|n| self.env.get(&n).map(|b| (n.clone(), b.value.clone())))
                    .collect();
                Ok(Value::Function(Rc::new(crate::value::FuncVal {
                    params: Rc::new(params.clone()),
                    body: Rc::new((**body).clone()),
                    captured: Rc::new(captured),
                })))
            }
            Expr::Let { bindings, body } => {
                // Bind sequentially (later bindings see earlier ones), evaluate
                // the body, then restore the outer scope.
                let mut saved: Vec<(String, Option<Binding>)> = Vec::with_capacity(bindings.len());
                for (name, expr) in bindings {
                    let v = self.eval(expr)?;
                    let prev = self
                        .env
                        .insert(name.clone(), Binding { value: v, mutable: false });
                    saved.push((name.clone(), prev));
                }
                let result = self.eval(body);
                for (name, prev) in saved.into_iter().rev() {
                    match prev {
                        Some(b) => {
                            self.env.insert(name, b);
                        }
                        None => {
                            self.env.remove(&name);
                        }
                    }
                }
                result
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                line,
                col,
            } => {
                let c = self.eval(cond)?;
                if matches!(c, Value::Missing) {
                    return Err(HelixError::new(
                        "`if` condition is `missing` — cannot choose a branch",
                        *line,
                        *col,
                    )
                    .hint("handle the missing case first, e.g. `if x.is_missing() then ... else ...`."));
                }
                let taken = as_bool(&c, *line, *col).map_err(|e| {
                    e.hint("an `if` condition must be a boolean, e.g. `if x > 0 then ... else ...`.")
                })?;
                if taken {
                    self.eval(then_branch)
                } else {
                    self.eval(else_branch)
                }
            }
            // `try EXPR` — evaluate `EXPR`, catching any runtime error into a record.
            Expr::Try { expr, .. } => Ok(match self.eval(expr) {
                Ok(v) => try_ok(v),
                Err(e) => try_err(e.message),
            }),
            Expr::Match { scrutinee, arms, line, col } => {
                let v = self.eval(scrutinee)?;
                for arm in arms {
                    if let Some(binds) = pattern_match(&arm.pattern, &v) {
                        // Install every binding (save/restore); the guard and body see them.
                        let mut saved: Vec<(String, Option<Binding>)> =
                            Vec::with_capacity(binds.len());
                        for (name, val) in binds {
                            let prev = self
                                .env
                                .insert(name.clone(), Binding { value: val, mutable: false });
                            saved.push((name, prev));
                        }
                        // `Some(result)` takes this arm; `None` means the guard failed,
                        // so fall through to the next arm. A guard error propagates.
                        let outcome: Option<Result<Value, HelixError>> = match &arm.guard {
                            None => Some(self.eval(&arm.body)),
                            Some(g) => match self.eval(g).and_then(|gv| as_bool(&gv, *line, *col)) {
                                Ok(true) => Some(self.eval(&arm.body)),
                                Ok(false) => None,
                                Err(e) => Some(Err(e)),
                            },
                        };
                        for (name, prev) in saved.into_iter().rev() {
                            match prev {
                                Some(b) => {
                                    self.env.insert(name, b);
                                }
                                None => {
                                    self.env.remove(&name);
                                }
                            }
                        }
                        if let Some(r) = outcome {
                            return r;
                        }
                    }
                }
                Err(HelixError::new("no `match` arm matched the value", *line, *col)
                    .hint("add a `_ => ...` arm to handle any remaining case."))
            }
        }
    }

    /// Apply a function: bind its parameters over the current scope, evaluate
    /// the body, then restore. Because the function's own name stays bound
    /// throughout, recursion works.
    fn call_function(
        &mut self,
        name: &str,
        f: &crate::value::FuncVal,
        args: Vec<Value>,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        let params = &f.params;
        let body = &f.body;
        let captured = &f.captured;
        if params.len() != args.len() {
            return Err(HelixError::new(
                format!(
                    "`{}` expects {} argument{}, got {}",
                    name,
                    params.len(),
                    if params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                line,
                col,
            ));
        }
        self.depth += 1;
        if self.depth > MAX_CALL_DEPTH {
            self.depth -= 1;
            return Err(HelixError::new(
                format!("maximum recursion depth ({}) exceeded", MAX_CALL_DEPTH),
                line,
                col,
            )
            .hint("is the recursion missing a base case, or should this be a loop/comprehension?"));
        }
        // `saved` holds a *borrowed* name (from the function value's params/captured,
        // which outlive this call) alongside the binding it shadowed. The insert key must
        // still be owned (the env map owns its keys), but save/restore no longer clones
        // each name a second time, and the common no-shadow restore is a plain
        // `remove(&str)` with zero allocation — so a hot recursive call (e.g. `fib`) stops
        // churning a fresh copy of each parameter name per call.
        let mut saved: Vec<(&str, Option<Binding>)> =
            Vec::with_capacity(captured.len() + params.len());
        // Install the captured lexical environment (a closure's free variables)
        // first, then the parameters on top so they shadow on any name clash.
        for (n, v) in captured.iter() {
            let prev = self
                .env
                .insert(n.clone(), Binding { value: v.clone(), mutable: false });
            saved.push((n.as_str(), prev));
        }
        for (p, a) in params.iter().zip(args) {
            let prev = self
                .env
                .insert(p.clone(), Binding { value: a, mutable: false });
            saved.push((p.as_str(), prev));
        }
        let result = self.eval(body);
        // Restore in reverse so shadowed names come back correctly.
        for (n, prev) in saved.into_iter().rev() {
            match prev {
                Some(b) => {
                    self.env.insert(n.to_string(), b);
                }
                None => {
                    self.env.remove(n);
                }
            }
        }
        self.depth -= 1;
        result
    }

    /// Unary negation / logical-not. `pub(crate)` so the bytecode VM can reuse
    /// the exact same semantics via its builtin-host interpreter.
    pub(crate) fn eval_unary(
        &self,
        op: &UnOp,
        v: Value,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match op {
            UnOp::Neg => match v {
                // wrapping so `-(i64::MIN)` doesn't panic in debug
                Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
                Value::Float(f) => Ok(Value::Float(-f)),
                Value::Missing => Ok(Value::Missing), // negation propagates
                other => Err(HelixError::new(
                    format!("cannot negate a value of type {}", other.type_name()),
                    line,
                    col,
                )),
            },
            UnOp::Not => match v {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                Value::Missing => Ok(Value::Missing), // not missing -> missing
                other => Err(HelixError::new(
                    format!("expected a boolean, found a value of type {}", other.type_name()),
                    line,
                    col,
                )),
            },
        }
    }

}

// ---------- free helpers ----------

pub(crate) fn as_bool(v: &Value, line: usize, col: usize) -> Result<bool, HelixError> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(HelixError::new(
            format!("expected a boolean, found a value of type {}", other.type_name()),
            line,
            col,
        )
        .hint("Helix has no \"truthiness\" — use an explicit comparison like `x > 0`.")),
    }
}

/// Three-valued view of a value for boolean logic: `Some(bool)` for a real
/// boolean, `None` for `missing`, error otherwise.
pub(crate) fn tri(v: &Value, line: usize, col: usize) -> Result<Option<bool>, HelixError> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Missing => Ok(None),
        other => Err(HelixError::new(
            format!("expected a boolean, found a value of type {}", other.type_name()),
            line,
            col,
        )
        .hint("Helix has no \"truthiness\" — use an explicit comparison like `x > 0`.")),
    }
}

use num_traits::ToPrimitive as _AsIntToPrim;

pub(crate) fn as_int(v: &Value, who: &str, line: usize, col: usize) -> Result<i64, HelixError> {
    match v {
        Value::Int(i) => Ok(*i),
        // An integer-valued rational (denominator 1) is that integer.
        Value::Rational(r) if r.is_integer() => r.to_i64().ok_or_else(|| {
            type_err(who, "an integer in range", v, line, col)
        }),
        // An *integer-valued* float (e.g. `1.0` from `least_squares`/`lll`, a lattice
        // coefficient, or arithmetic that stayed whole) counts as that integer — so
        // `gcd`/`range`/`//`/… accept it directly. A fractional or out-of-range float
        // is a clear error (use `round`/`floor`/`trunc` to choose how to convert).
        Value::Float(f)
            if f.fract() == 0.0 && *f >= -9.223_372_036_854_776e18 && *f < 9.223_372_036_854_776e18 =>
        {
            Ok(*f as i64)
        }
        other => Err(type_err(who, "an integer", other, line, col)),
    }
}

fn type_err(who: &str, want: &str, got: &Value, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!("`{}` expected {}, found a value of type {}", who, want, got.type_name()),
        line,
        col,
    )
}

fn arity(name: &str, args: &[Value], want: usize, line: usize, col: usize) -> Result<(), HelixError> {
    if args.len() == want {
        Ok(())
    } else {
        Err(HelixError::new(
            format!(
                "`{}` takes {} argument{}, got {}",
                name,
                want,
                if want == 1 { "" } else { "s" },
                args.len()
            ),
            line,
            col,
        ))
    }
}

/// The binder parameter name(s) and body for a comprehension. `x => ...` names
/// one binder; `(a, b) => ...` destructures each element into two; a bare
/// expression uses the implicit `it`.
pub(crate) fn comprehension_params(arg: &Expr) -> (Vec<String>, &Expr) {
    match arg {
        Expr::Lambda { params, body, .. } => (params.clone(), body.as_ref()),
        other => (vec!["it".to_string()], other),
    }
}


mod dataframe_ops;
pub(crate) use dataframe_ops::*;

fn comp_arity(name: &str, example: &str, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!("`{}` takes exactly one expression", name),
        line,
        col,
    )
    .hint(format!("e.g. `xs.{}{}`.", name, example))
}

/// Match `pat` against `v`. `None` means no match; `Some(binds)` means it matched,
/// binding the listed names (left-to-right; empty for a pure literal or `_`).
/// Recursive for tuple/record patterns. Shared by the tree-walker AND the VM (whose
/// `MatchArm` op calls this), so the two engines match identically.
pub(crate) fn pattern_match(pat: &crate::ast::Pattern, v: &Value) -> Option<Vec<(String, Value)>> {
    use crate::ast::Pattern;
    let lit = |matched: bool| if matched { Some(Vec::new()) } else { None };
    match pat {
        Pattern::Wildcard => Some(Vec::new()),
        Pattern::Bind(name) => Some(vec![(name.clone(), v.clone())]),
        Pattern::Int(i) => lit(matches!(v, Value::Int(x) if x == i)),
        Pattern::Float(f) => lit(matches!(v, Value::Float(x) if x == f)),
        Pattern::Str(s) => lit(matches!(v, Value::Str(x) if x.as_str() == s.as_str())),
        Pattern::Bool(b) => lit(matches!(v, Value::Bool(x) if x == b)),
        Pattern::Missing => lit(matches!(v, Value::Missing)),
        Pattern::Tuple(pats) => {
            let items = match v {
                Value::Tuple(items) => items,
                _ => return None,
            };
            if items.len() != pats.len() {
                return None;
            }
            let mut binds = Vec::new();
            for (p, item) in pats.iter().zip(items.iter()) {
                binds.extend(pattern_match(p, item)?);
            }
            Some(binds)
        }
        Pattern::Record(fields) => {
            let rec = match v {
                Value::Record(rec) => rec,
                _ => return None,
            };
            let mut binds = Vec::new();
            for (key, subpat) in fields {
                // Only the listed fields are required (a partial match).
                let fv = rec.iter().find(|(k, _)| k.as_str() == key.as_str()).map(|(_, val)| val)?;
                binds.extend(pattern_match(subpat, fv)?);
            }
            Some(binds)
        }
        // Alternatives are bindingless (enforced at parse time), so any match yields
        // no bindings — keeping the two engines' stack/slot layout consistent.
        Pattern::Or(pats) => {
            if pats.iter().any(|alt| pattern_match(alt, v).is_some()) {
                Some(Vec::new())
            } else {
                None
            }
        }
    }
}

/// The names a pattern binds, left-to-right — the static counterpart of the values
/// `pattern_match` returns (same order). Used by the compiler (to declare an arm's
/// locals) and by the free-variable and module-rewrite passes.
pub(crate) fn pattern_binding_names(pat: &crate::ast::Pattern) -> Vec<String> {
    use crate::ast::Pattern;
    fn go(pat: &Pattern, out: &mut Vec<String>) {
        match pat {
            Pattern::Bind(name) => out.push(name.clone()),
            Pattern::Tuple(pats) => pats.iter().for_each(|p| go(p, out)),
            Pattern::Record(fields) => fields.iter().for_each(|(_, p)| go(p, out)),
            // Recurse alternatives so the "or-patterns can't bind" check sees any
            // (even nested) binding; a valid bindingless `Or` contributes nothing.
            Pattern::Or(alts) => alts.iter().for_each(|p| go(p, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    go(pat, &mut out);
    out
}

/// The interned field names of a `try` result record (`ok`/`value`/`error`),
/// resolved once rather than per `try` — a `try` inside a `map` builds one of
/// these per element, so the keys must not re-hit the interner each time.
struct TryKeys {
    ok: Symbol,
    value: Symbol,
    error: Symbol,
}
static TRY_KEYS: std::sync::LazyLock<TryKeys> = std::sync::LazyLock::new(|| TryKeys {
    ok: Symbol::intern("ok"),
    value: Symbol::intern("value"),
    error: Symbol::intern("error"),
});

/// The result record of `try EXPR` on success: `{ok: true, value: v, error: missing}`.
/// Shared by both engines so the record shape is identical.
pub(crate) fn try_ok(v: Value) -> Value {
    let k = &*TRY_KEYS;
    Value::Record(Rc::new(vec![
        (k.ok, Value::Bool(true)),
        (k.value, v),
        (k.error, Value::Missing),
    ]))
}

/// The result record of `try EXPR` on a runtime error:
/// `{ok: false, value: missing, error: <message>}`.
pub(crate) fn try_err(message: String) -> Value {
    let k = &*TRY_KEYS;
    Value::Record(Rc::new(vec![
        (k.ok, Value::Bool(false)),
        (k.value, Value::Missing),
        (k.error, Value::Str(Rc::new(message))),
    ]))
}


/// Apply a scalar operation across a value, broadcasting over arrays and
/// propagating `missing` — the spine of the math standard library.
fn broadcast_unary(
    v: &Value,
    scalar: &dyn Fn(&Value) -> Result<Value, HelixError>,
) -> Result<Value, HelixError> {
    match v {
        Value::Array(items) => {
            let out: Result<Vec<Value>, HelixError> = items
                .iter_values()
                .map(|e| broadcast_unary(&e, scalar))
                .collect();
            Ok(Value::array(out?))
        }
        // Apply the scalar op to every tensor element (math fns yield Float).
        Value::Tensor(t) => {
            let mut data = Vec::with_capacity(t.len());
            for &x in t.iter() {
                match scalar(&Value::Float(x))? {
                    Value::Float(f) => data.push(f),
                    Value::Int(i) => data.push(i as f64),
                    other => {
                        return Err(HelixError::new(
                            format!("cannot apply this to a tensor (produced a {})", other.type_name()),
                            0,
                            0,
                        ))
                    }
                }
            }
            let out = ndarray::ArrayD::from_shape_vec(t.raw_dim(), data)
                .expect("same length as source tensor");
            Ok(Value::Tensor(Rc::new(out)))
        }
        Value::Missing => Ok(Value::Missing),
        other => scalar(other),
    }
}

/// Arrays/tensors at or above this many elements map in parallel (rayon); below it
/// the thread hand-off costs more than it saves, so the small-array hot path stays
/// sequential. The mapped result is order-preserving, hence byte-identical either way.
/// `pub(crate)` so the JIT map kernels (`jit::ffi::run_map_chunked`) share the exact
/// same cutoff — one documented threshold across both engines.
pub(crate) const PAR_MATH_THRESHOLD: usize = 1 << 15;

/// Map a monomorphized `f64 -> f64` over a packed buffer, in parallel past the
/// threshold. Order-preserving, so the output is identical to the sequential map.
fn map_f64_buf(xs: &[f64], f: fn(f64) -> f64) -> Vec<f64> {
    if xs.len() >= PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        xs.par_iter().map(|&x| f(x)).collect()
    } else {
        xs.iter().map(|&x| f(x)).collect()
    }
}

/// Map a closure over a packed buffer into a new typed buffer — the generic form of
/// [`map_f64_buf`] for `i64→i64` (`abs`/`sign` of ints) and `f64→i64` (`floor`/`sign` of
/// floats). Order-preserving and parallel past the threshold, so the result is identical
/// to the per-element path — just without materializing a `Vec<Value>` for every element.
fn map_buf<T, U>(xs: &[T], f: impl Fn(T) -> U + Sync + Send) -> Vec<U>
where
    T: Copy + Sync,
    U: Send,
{
    if xs.len() >= PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        xs.par_iter().map(|&x| f(x)).collect()
    } else {
        xs.iter().map(|&x| f(x)).collect()
    }
}

/// Map a same-type function over a packed buffer **in place** (a uniquely-owned array
/// reused instead of allocating a fresh one). Order-preserving and parallel past the
/// threshold, so the result is byte-identical to the allocating `map_buf`.
fn map_buf_inplace<T: Copy + Send>(a: &mut [T], f: impl Fn(T) -> T + Sync + Send) {
    if a.len() >= PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        a.par_iter_mut().for_each(|x| *x = f(*x));
    } else {
        a.iter_mut().for_each(|x| *x = f(*x));
    }
}

/// A float→float math function (sqrt, sin, exp, …) lifted to Helix values.
///
/// Packed numeric arrays and tensors take a fast path that maps straight over the
/// `f64`/`i64` buffer into a new buffer — no per-element `Value` boxing (the old path
/// materialized a `Vec<Value>` for *every* element) and, past a size threshold, in
/// parallel across cores. Heterogeneous (`Values`) arrays, scalars, and `missing`
/// keep the general `broadcast_unary` path, so results are unchanged.
fn apply_float_fn(
    name: &str,
    f: fn(f64) -> f64,
    v: Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    use crate::value::ArrayData;
    let scalar_fallback = |v: &Value| {
        broadcast_unary(v, &|s| match s.as_f64() {
            Some(x) => Ok(Value::Float(f(x))),
            None => Err(type_err(name, "a number or array of numbers", s, line, col)),
        })
    };
    match v {
        Value::Array(mut a) => {
            // A uniquely-owned `Floats` buffer is mapped IN PLACE (f64→f64 preserves type),
            // so chains like `sqrt(abs(xs))` reuse the intermediate instead of allocating.
            if let Some(ArrayData::Floats(buf)) = Rc::get_mut(&mut a) {
                map_buf_inplace(buf, f);
                return Ok(Value::Array(a));
            }
            match &*a {
                ArrayData::Floats(xs) => Ok(Value::float_array(map_f64_buf(xs, f))),
                // `i64 → f64` changes the element type, so it allocates a fresh buffer.
                ArrayData::Ints(xs) => Ok(Value::float_array(map_buf(xs, move |x: i64| f(x as f64)))),
                // A range maps its (materialized) i64 elements to `f64` — same as the `Ints` path.
                ArrayData::Range { .. } => Ok(Value::float_array(
                    a.to_ints().unwrap().iter().map(|&x| f(x as f64)).collect(),
                )),
                ArrayData::Values(_) | ArrayData::Enumerate { .. } => scalar_fallback(&Value::Array(a)),
            }
        }
        Value::Tensor(t) => {
            // Contiguous tensors map over the slice (parallel past the threshold);
            // a non-contiguous view (e.g. a transpose) falls back to ndarray's mapv.
            match t.as_slice() {
                Some(xs) => {
                    let out = map_f64_buf(xs, f);
                    let arr = ndarray::ArrayD::from_shape_vec(t.raw_dim(), out)
                        .expect("same length as source tensor");
                    Ok(Value::Tensor(Rc::new(arr)))
                }
                None => Ok(Value::Tensor(Rc::new(t.mapv(f)))),
            }
        }
        other => scalar_fallback(&other),
    }
}

/// A rounding function (floor/ceil/round/trunc) that yields whole `Int`s. A packed
/// `Int`/`Float` array maps straight over its buffer (an `Int` array is returned
/// unchanged — rounding an integer is a no-op); heterogeneous/scalar inputs keep the
/// general path. Output identical to the per-element map, no per-element boxing.
fn apply_round_fn(
    name: &str,
    f: fn(f64) -> f64,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    use crate::value::ArrayData;
    match v {
        Value::Array(ad) => match &**ad {
            // Checked per element (same rule as `round_to_i64`): an out-of-range / non-finite
            // element ERRORS rather than silently saturating the whole packed conversion.
            ArrayData::Floats(xs) => xs
                .iter()
                .map(|&x| round_to_i64(name, f(x), line, col))
                .collect::<Result<Vec<i64>, _>>()
                .map(Value::int_array),
            ArrayData::Ints(xs) => Ok(Value::int_array(xs.clone())),
            // Rounding whole numbers is a no-op — return the range unchanged (it is already `Int`).
            ArrayData::Range { .. } => Ok(Value::Array(ad.clone())),
            ArrayData::Values(_) | ArrayData::Enumerate { .. } => round_box(name, f, v, line, col),
        },
        // Tensors and scalars keep the exact general path (a tensor stays a whole-valued
        // `Float` tensor, with the same `as i64` saturation for out-of-range values).
        _ => round_box(name, f, v, line, col),
    }
}

/// The per-element rounding closure for heterogeneous arrays, scalars, and `missing`.
/// Convert a rounded float to `i64`, ERRORING (instead of silently saturating, which is what
/// `as i64` does — `1e30 as i64` is `i64::MAX`, `NaN as i64` is `0`) when the result is not
/// finite or lies outside the i64 range. `round`/`floor`/`ceil`/`trunc` all return `Int`, so a
/// magnitude a 64-bit integer cannot hold has no honest answer — raising beats handing back
/// `i64::MAX`/`i64::MIN`/`0` as if it were real data. The bounds are the exactly-representable
/// f64 values `-(2^63)` (= `i64::MIN`) and `2^63` (= `i64::MAX + 1`); the upper bound is strict
/// because `2^63` is not a valid `i64`, and every whole f64 below it casts without saturating.
fn round_to_i64(name: &str, x: f64, line: usize, col: usize) -> Result<i64, HelixError> {
    const MIN: f64 = -9_223_372_036_854_775_808.0; // i64::MIN, exact in f64
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2^63 = i64::MAX + 1, exact in f64
    // The half-open range also rejects NaN and ±inf (all fall outside), so no separate
    // `is_finite` check is needed.
    if (MIN..LIMIT).contains(&x) {
        Ok(x as i64)
    } else {
        Err(HelixError::new(
            format!("`{name}` cannot produce an integer from {x}: the result is out of the 64-bit integer range"),
            line,
            col,
        )
        .hint("`round`/`floor`/`ceil`/`trunc` return an Int — keep the value within ±9.2e18, or use `round(x, digits)` to stay a Float."))
    }
}

fn round_box(name: &str, f: fn(f64) -> f64, v: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    broadcast_unary(v, &|s| match s {
        Value::Int(i) => Ok(Value::Int(*i)),
        Value::Float(x) => Ok(Value::Int(round_to_i64(name, f(*x), line, col)?)),
        other => Err(type_err(name, "a number or array of numbers", other, line, col)),
    })
}

/// `abs` — preserves `Int` (`wrapping_abs`, matching the arithmetic ops) and `Float`. A
/// packed `Int`/`Float` array maps over its buffer (no boxing); everything else keeps the
/// exact general path. Output identical to the per-element map.
pub(crate) fn apply_abs(v: Value, line: usize, col: usize) -> Result<Value, HelixError> {
    use crate::value::ArrayData;
    let boxed = |v: &Value| {
        broadcast_unary(v, &|s| match s {
            Value::Int(i) => Ok(Value::Int(i.wrapping_abs())),
            Value::Float(x) => Ok(Value::Float(x.abs())),
            other => Err(type_err("abs", "a number or array of numbers", other, line, col)),
        })
    };
    match v {
        Value::Array(mut a) => {
            // `abs` preserves the element type, so a uniquely-owned Int/Float buffer is
            // mapped IN PLACE; a shared one (or a `Values` array) takes the copy path.
            match Rc::get_mut(&mut a) {
                Some(ArrayData::Floats(buf)) => {
                    map_buf_inplace(buf, |x: f64| x.abs());
                    return Ok(Value::Array(a));
                }
                Some(ArrayData::Ints(buf)) => {
                    map_buf_inplace(buf, |x: i64| x.wrapping_abs());
                    return Ok(Value::Array(a));
                }
                _ => {}
            }
            match &*a {
                ArrayData::Floats(xs) => Ok(Value::float_array(map_buf(xs, |x: f64| x.abs()))),
                ArrayData::Ints(xs) => Ok(Value::int_array(map_buf(xs, |x: i64| x.wrapping_abs()))),
                ArrayData::Range { .. } => Ok(Value::int_array(
                    a.to_ints().unwrap().iter().map(|&x| x.wrapping_abs()).collect(),
                )),
                ArrayData::Values(_) | ArrayData::Enumerate { .. } => boxed(&Value::Array(a)),
            }
        }
        other => boxed(&other),
    }
}

/// `sign` → `Int` (`1`/`-1`/`0`). Packed arrays map over the buffer; everything else keeps
/// the exact general path.
pub(crate) fn apply_sign(v: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    use crate::value::ArrayData;
    fn fsign(x: f64) -> i64 {
        if x > 0.0 {
            1
        } else if x < 0.0 {
            -1
        } else {
            0
        }
    }
    if let Value::Array(ad) = v {
        match &**ad {
            ArrayData::Floats(xs) => return Ok(Value::int_array(map_buf(xs, fsign))),
            ArrayData::Ints(xs) => return Ok(Value::int_array(map_buf(xs, |x: i64| x.signum()))),
            ArrayData::Range { .. } => {
                return Ok(Value::int_array(
                    ad.to_ints().unwrap().iter().map(|&x| x.signum()).collect(),
                ));
            }
            ArrayData::Values(_) | ArrayData::Enumerate { .. } => {}
        }
    }
    broadcast_unary(v, &|s| match s {
        Value::Int(i) => Ok(Value::Int(i.signum())),
        Value::Float(x) => Ok(Value::Int(fsign(*x))),
        other => Err(type_err("sign", "a number or array of numbers", other, line, col)),
    })
}

/// Two scalar numbers (or `missing`) for two-argument math functions.
fn two_nums(
    name: &str,
    a: &Value,
    b: &Value,
    line: usize,
    col: usize,
) -> Result<Option<(f64, f64)>, HelixError> {
    if matches!(a, Value::Missing) || matches!(b, Value::Missing) {
        return Ok(None); // signal missing-propagation
    }
    let x = a
        .as_f64()
        .ok_or_else(|| type_err(name, "a number", a, line, col))?;
    let y = b
        .as_f64()
        .ok_or_else(|| type_err(name, "a number", b, line, col))?;
    Ok(Some((x, y)))
}

/// A tensor shape argument: a Helix array of non-negative integers, `[2, 3]`.
fn tensor_shape_arg(v: &Value, line: usize, col: usize) -> Result<Vec<usize>, HelixError> {
    match v {
        Value::Array(items) => items
            .to_values()
            .iter()
            .map(|x| match x {
                Value::Int(i) if *i >= 0 => Ok(*i as usize),
                _ => Err(HelixError::new(
                    "shape entries must be non-negative integers",
                    line,
                    col,
                )),
            })
            .collect(),
        other => Err(type_err("shape", "an array like `[2, 3]`", other, line, col)),
    }
}

/// Eager integer range, capped so a typo like `range(10000000000)` raises a
/// clean error instead of exhausting memory and aborting.
fn int_range(a: i64, b: i64, line: usize, col: usize) -> Result<Value, HelixError> {
    const MAX_RANGE: i128 = 100_000_000;
    let len = (b as i128) - (a as i128);
    if len > MAX_RANGE {
        return Err(HelixError::new(
            format!("`range` would build {} elements, which is too large", len),
            line,
            col,
        )
        .hint("keep ranges under 100 million elements."));
    }
    // LAZY: return an `ArrayData::Range` (O(1), no allocation) — consumers materialize on demand
    // (see `ArrayData::to_ints`/`densify`), so `range(N).first()/.count()/.take()` never build the
    // `Vec<i64>`. Behaviour is bit-identical to the materialized `Int` array.
    Ok(Value::lazy_range(a, 1, len.max(0) as usize))
}

/// `range(start, stop, step)` — half-open, step may be negative for a descending
/// range. A zero step is an error (it would never terminate).
fn int_range_step(a: i64, b: i64, step: i64, line: usize, col: usize) -> Result<Value, HelixError> {
    if step == 0 {
        return Err(HelixError::new("`range` step must not be zero".to_string(), line, col)
            .hint("use a positive step to count up or a negative step to count down."));
    }
    const MAX_RANGE: i128 = 100_000_000;
    // Number of elements: ceil(|stop - start| / |step|), clamped at 0 when the
    // direction of `step` points away from `stop`.
    let span = (b as i128) - (a as i128);
    let stride = step as i128;
    let len = if (span > 0) == (stride > 0) && span != 0 {
        (span.abs() + stride.abs() - 1) / stride.abs()
    } else {
        0
    };
    if len > MAX_RANGE {
        return Err(HelixError::new(
            format!("`range` would build {} elements, which is too large", len),
            line,
            col,
        )
        .hint("ranges are materialized eagerly — keep them under 100 million elements."));
    }
    // LAZY: `len` is exactly the element count computed above, and each element `a + step*i`
    // (for `i` in `0..len`) is in `[a, b)` so it fits `i64` — the `ArrayData::Range` invariant.
    // No `Vec` is built; consumers materialize on demand. Bit-identical to the eager array.
    Ok(Value::lazy_range(a, step, len.max(0) as usize))
}

fn make_dna(s: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let mut out = String::with_capacity(s.len());
    for (i, ch) in s.chars().enumerate() {
        let up = ch.to_ascii_uppercase();
        if crate::interp::methods::is_iupac_dna(up) {
            out.push(up);
        } else {
            return Err(HelixError::new(
                format!("`{}` is not a valid DNA base (at position {})", ch, i),
                line,
                col,
            )
            .hint("DNA may contain A, C, G, T, N, or an IUPAC ambiguity code (R Y S W K M B D H V)."));
        }
    }
    Ok(Value::Dna(Rc::new(out)))
}


mod access;
pub(crate) use access::*;


mod ops;
pub(crate) use ops::*;


mod methods;
pub(crate) use methods::*;

#[cfg(test)]
mod tests;

mod builtins;

mod comprehensions;

//! Tree-walking interpreter.

use std::rc::Rc;

use rustc_hash::FxHashMap;

use crate::ast::{BinOp, Expr, Stmt, UnOp};
use crate::backend::Df;
use crate::dataframe;
use crate::error::{suggest, HelixError};
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

pub struct Interp {
    env: FxHashMap<String, Binding>,
    depth: usize,
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
        Interp { env, depth: 0 }
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
            Expr::Interp(parts) => {
                let mut s = String::new();
                for part in parts {
                    match part {
                        crate::ast::InterpPart::Lit(t) => s.push_str(t),
                        crate::ast::InterpPart::Expr(e) => {
                            let v = self.eval(e)?;
                            let (l, c) = e.position();
                            s.push_str(&crate::value::display_value(&v, l, c)?);
                        }
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
                Ok(Value::Array(Rc::new(vals)))
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
                    vals.push((k.clone(), self.eval(v)?));
                }
                Ok(Value::Record(Rc::new(vals)))
            }
            Expr::Field {
                recv,
                name,
                line,
                col,
            } => {
                let r = self.eval(recv)?;
                eval_field(&r, name, *line, *col)
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
                if crate::registry::lookup(name).is_some() {
                    return self.call_builtin(name, vals, *line, *col);
                }
                // A user-defined (or anonymous, stored-in-a-variable) function?
                let func = self.env.get(name).and_then(|b| match &b.value {
                    Value::Function(g) => Some((g.params.clone(), g.body.clone())),
                    _ => None,
                });
                if let Some((params, body)) = func {
                    return self.call_function(name, &params, &body, vals, *line, *col);
                }
                // The name exists but isn't callable.
                if let Some(b) = self.env.get(name) {
                    return Err(HelixError::new(
                        format!("`{}` is a {}, not a function", name, b.value.type_name()),
                        *line,
                        *col,
                    )
                    .hint("only functions and the built-ins `print`/`dna`/`range` can be called."));
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
            Expr::Method {
                recv,
                name,
                args,
                line,
                col,
            } => {
                let recv_v = self.eval(recv)?;
                // `is_missing` is universal — every value answers it. DataFrame and
                // GroupBy receivers are routed to their verb dispatch below, which
                // never reaches the universal handler in `call_method`, so intercept
                // it here; a frame/group is never `missing`, so the answer is `false`.
                if name == "is_missing"
                    && matches!(recv_v, Value::DataFrame(_) | Value::GroupBy(_))
                {
                    if !args.is_empty() {
                        return Err(HelixError::new(
                            "`is_missing` takes no arguments",
                            *line,
                            *col,
                        ));
                    }
                    return Ok(Value::Bool(false));
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
                    "map" | "filter" | "where" | "reduce" | "any" | "all"
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
            Expr::Lambda { params, body, .. } => Ok(Value::Function(Rc::new(crate::value::FuncVal {
                params: Rc::new(params.clone()),
                body: Rc::new((**body).clone()),
            }))),
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
        }
    }

    /// Apply a function: bind its parameters over the current scope, evaluate
    /// the body, then restore. Because the function's own name stays bound
    /// throughout, recursion works.
    fn call_function(
        &mut self,
        name: &str,
        params: &[String],
        body: &Expr,
        args: Vec<Value>,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
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
        let saved: Vec<(String, Option<Binding>)> = params
            .iter()
            .map(|p| (p.clone(), self.env.remove(p)))
            .collect();
        for (p, a) in params.iter().zip(args) {
            self.env
                .insert(p.clone(), Binding { value: a, mutable: false });
        }
        let result = self.eval(body);
        for (p, old) in saved {
            self.env.remove(&p);
            if let Some(b) = old {
                self.env.insert(p, b);
            }
        }
        self.depth -= 1;
        result
    }

    fn eval_with_two(
        &mut self,
        na: &str,
        va: Value,
        nb: &str,
        vb: Value,
        body: &Expr,
    ) -> Result<Value, HelixError> {
        let saved_a = self.env.remove(na);
        let saved_b = self.env.remove(nb);
        self.env
            .insert(na.to_string(), Binding { value: va, mutable: false });
        self.env
            .insert(nb.to_string(), Binding { value: vb, mutable: false });
        let result = self.eval(body);
        self.env.remove(na);
        self.env.remove(nb);
        if let Some(b) = saved_a {
            self.env.insert(na.to_string(), b);
        }
        if let Some(b) = saved_b {
            self.env.insert(nb.to_string(), b);
        }
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

pub(crate) fn as_int(v: &Value, who: &str, line: usize, col: usize) -> Result<i64, HelixError> {
    match v {
        Value::Int(i) => Ok(*i),
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

/// The result record of `try EXPR` on success: `{ok: true, value: v, error: missing}`.
/// Shared by both engines so the record shape is identical.
pub(crate) fn try_ok(v: Value) -> Value {
    Value::Record(Rc::new(vec![
        ("ok".to_string(), Value::Bool(true)),
        ("value".to_string(), v),
        ("error".to_string(), Value::Missing),
    ]))
}

/// The result record of `try EXPR` on a runtime error:
/// `{ok: false, value: missing, error: <message>}`.
pub(crate) fn try_err(message: String) -> Value {
    Value::Record(Rc::new(vec![
        ("ok".to_string(), Value::Bool(false)),
        ("value".to_string(), Value::Missing),
        ("error".to_string(), Value::Str(Rc::new(message))),
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
            let out: Result<Vec<Value>, HelixError> =
                items.iter().map(|e| broadcast_unary(e, scalar)).collect();
            Ok(Value::Array(Rc::new(out?)))
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

/// A float→float math function (sqrt, sin, exp, …) lifted to Helix values.
fn apply_float_fn(
    name: &str,
    f: fn(f64) -> f64,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    broadcast_unary(v, &|s| match s.as_f64() {
        Some(x) => Ok(Value::Float(f(x))),
        None => Err(type_err(name, "a number or array of numbers", s, line, col)),
    })
}

/// A rounding function (floor/ceil/round/trunc) that yields whole `Int`s.
fn apply_round_fn(
    name: &str,
    f: fn(f64) -> f64,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    broadcast_unary(v, &|s| match s {
        Value::Int(i) => Ok(Value::Int(*i)),
        Value::Float(x) => Ok(Value::Int(f(*x) as i64)),
        other => Err(type_err(name, "a number or array of numbers", other, line, col)),
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
        .hint("ranges are materialized eagerly — keep them under 100 million elements."));
    }
    let mut v = Vec::with_capacity(len.max(0) as usize);
    let mut x = a;
    while x < b {
        v.push(Value::Int(x));
        x += 1;
    }
    Ok(Value::Array(Rc::new(v)))
}

fn make_dna(s: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let mut out = String::with_capacity(s.len());
    for (i, ch) in s.chars().enumerate() {
        let up = ch.to_ascii_uppercase();
        match up {
            'A' | 'C' | 'G' | 'T' => out.push(up),
            _ => {
                return Err(HelixError::new(
                    format!("`{}` is not a valid DNA base (at position {})", ch, i),
                    line,
                    col,
                )
                .hint("DNA sequences may only contain A, C, G, and T."));
            }
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

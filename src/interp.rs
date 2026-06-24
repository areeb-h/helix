//! Tree-walking interpreter.

use std::rc::Rc;

use polars::prelude::LazyFrame;
use rustc_hash::FxHashMap;

use crate::ast::{BinOp, Expr, Stmt, UnOp};
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
                for (n, val) in names.iter().zip(parts.into_iter()) {
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
                let f = Value::Function {
                    params: Rc::new(param_names),
                    body: Rc::new(body.clone()),
                };
                self.bind(name, f, false, *line, *col)?;
                Ok(StmtOutcome {
                    value: Value::Unit,
                    is_expr: false,
                })
            }
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
                            s.push_str(&v.to_string());
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
                if BUILTIN_FNS.contains(&name.as_str()) {
                    return self.call_builtin(name, vals, *line, *col);
                }
                // A user-defined (or anonymous, stored-in-a-variable) function?
                let func = self.env.get(name).and_then(|b| match &b.value {
                    Value::Function { params, body } => Some((params.clone(), body.clone())),
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
                let mut cands: Vec<String> = BUILTIN_FNS.iter().map(|s| s.to_string()).collect();
                cands.extend(
                    self.env
                        .iter()
                        .filter(|(_, b)| matches!(b.value, Value::Function { .. }))
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
                // DataFrame / GroupBy verbs take their column arguments
                // *unevaluated* (column names and predicates), so they're routed
                // before the array comprehension path.
                match &recv_v {
                    Value::DataFrame(lf) => {
                        return self.eval_df_method(lf.clone(), name, args, *line, *col);
                    }
                    Value::GroupBy { lf, keys } => {
                        return self
                            .eval_groupby_method(lf.clone(), keys.clone(), name, args, *line, *col);
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
            Expr::Lambda { params, body, .. } => Ok(Value::Function {
                params: Rc::new(params.clone()),
                body: Rc::new((**body).clone()),
            }),
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
        }
    }

    /// Evaluate `map`/`filter`/`where`/`reduce`. The argument expression is run
    /// once per element with `it` (and, for `reduce`, `acc`) bound in scope.
    fn eval_comprehension(
        &mut self,
        recv: &Value,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        let items = match recv {
            Value::Array(items) => items.clone(),
            // `missing.map(...)` etc. propagate rather than erroring (ADR 0001).
            Value::Missing => return Ok(Value::Missing),
            other => {
                return Err(HelixError::new(
                    format!("type {} has no method `{}`", other.type_name(), name),
                    line,
                    col,
                )
                .hint("`map`, `filter`, `where`, and `reduce` work on arrays."))
            }
        };

        match name {
            "map" => {
                if args.len() != 1 {
                    return Err(comp_arity(name, "(it * 2)", line, col));
                }
                let (params, body) = comprehension_params(&args[0]);
                let mut out = Vec::with_capacity(items.len());
                for el in items.iter() {
                    out.push(self.eval_with_pattern(&params, el.clone(), body, line, col)?);
                }
                Ok(Value::Array(Rc::new(out)))
            }
            "filter" | "where" => {
                if args.len() != 1 {
                    return Err(comp_arity(name, "(it > 0)", line, col));
                }
                let (params, body) = comprehension_params(&args[0]);
                let mut out = Vec::new();
                for el in items.iter() {
                    let keep = self.eval_with_pattern(&params, el.clone(), body, line, col)?;
                    match keep {
                        Value::Bool(true) => out.push(el.clone()),
                        Value::Bool(false) => {}
                        other => {
                            return Err(HelixError::new(
                                format!(
                                    "`{}` expects a yes/no test, but the expression produced a {}",
                                    name,
                                    other.type_name()
                                ),
                                line,
                                col,
                            )
                            .hint("write a comparison, e.g. `xs.filter(it > 50)`."))
                        }
                    }
                }
                Ok(Value::Array(Rc::new(out)))
            }
            "any" | "all" => {
                if args.len() != 1 {
                    return Err(comp_arity(name, "(it > 0)", line, col));
                }
                let (params, body) = comprehension_params(&args[0]);
                let mut seen_missing = false;
                for el in items.iter() {
                    match self.eval_with_pattern(&params, el.clone(), body, line, col)? {
                        Value::Bool(b) => {
                            if name == "any" && b {
                                return Ok(Value::Bool(true));
                            }
                            if name == "all" && !b {
                                return Ok(Value::Bool(false));
                            }
                        }
                        Value::Missing => seen_missing = true,
                        other => {
                            return Err(HelixError::new(
                                format!(
                                    "`{}` expects a yes/no test, but the expression produced a {}",
                                    name,
                                    other.type_name()
                                ),
                                line,
                                col,
                            )
                            .hint("write a comparison, e.g. `xs.any(it > 0)`."))
                        }
                    }
                }
                // `any`: nothing was true; `all`: nothing was false. A missing
                // in the undetermined position makes the answer missing.
                if seen_missing {
                    Ok(Value::Missing)
                } else {
                    Ok(Value::Bool(name == "all"))
                }
            }
            "reduce" => {
                if args.len() != 2 {
                    return Err(HelixError::new(
                        "`reduce` takes a starting value and an accumulator function",
                        line,
                        col,
                    )
                    .hint("e.g. `xs.reduce(0, (acc, x) => acc + x)` to sum."));
                }
                // The folding function must name both binders explicitly — there
                // is no implicit `it`/`acc` here, by design (more than one binder
                // means you name them).
                let (pa, pb, body) = match &args[1] {
                    Expr::Lambda { params, body, .. } if params.len() == 2 => {
                        (params[0].as_str(), params[1].as_str(), body.as_ref())
                    }
                    Expr::Lambda { params, .. } => {
                        return Err(HelixError::new(
                            format!(
                                "`reduce`'s function needs exactly two parameters, but got {}",
                                params.len()
                            ),
                            line,
                            col,
                        )
                        .hint("the first is the running accumulator, e.g. `(acc, x) => acc + x`."))
                    }
                    _ => {
                        return Err(HelixError::new(
                            "`reduce` needs an explicit accumulator function",
                            line,
                            col,
                        )
                        .hint("name both binders: `xs.reduce(0, (acc, x) => acc + x)`."))
                    }
                };
                let mut acc = self.eval(&args[0])?;
                for el in items.iter() {
                    acc = self.eval_with_two(pa, acc, pb, el.clone(), body)?;
                }
                Ok(acc)
            }
            _ => unreachable!(),
        }
    }

    /// Bind one element to one name, or destructure it across several names
    /// (`(a, b) => ...`), then evaluate the body.
    fn eval_with_pattern(
        &mut self,
        names: &[String],
        el: Value,
        body: &Expr,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        if names.len() == 1 {
            return self.eval_with_binder(&names[0], el, body);
        }
        let parts = pattern_parts(&el, names.len(), line, col)?;
        let saved: Vec<(String, Option<Binding>)> = names
            .iter()
            .map(|n| (n.clone(), self.env.remove(n)))
            .collect();
        for (n, v) in names.iter().zip(parts.into_iter()) {
            self.env
                .insert(n.clone(), Binding { value: v, mutable: false });
        }
        let result = self.eval(body);
        for (n, old) in saved {
            self.env.remove(&n);
            if let Some(b) = old {
                self.env.insert(n, b);
            }
        }
        result
    }

    /// Evaluate `body` with `name` temporarily bound to `el`, restoring any
    /// shadowed binding afterward (so nested comprehensions work).
    fn eval_with_binder(
        &mut self,
        name: &str,
        el: Value,
        body: &Expr,
    ) -> Result<Value, HelixError> {
        let saved = self.env.remove(name);
        self.env.insert(
            name.to_string(),
            Binding {
                value: el,
                mutable: false,
            },
        );
        let result = self.eval(body);
        self.env.remove(name);
        if let Some(b) = saved {
            self.env.insert(name.to_string(), b);
        }
        result
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
        for (p, a) in params.iter().zip(args.into_iter()) {
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

    /// Dispatch a verb on a DataFrame. Column arguments arrive unevaluated so
    /// that `where(age > 40)` and `select(name, age)` can read column names
    /// directly. Predicates are lowered to Polars expressions.
    fn eval_df_method(
        &mut self,
        lf: Rc<LazyFrame>,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match name {
            "where" | "filter" | "select" | "sort" | "group" => {
                let env = &self.env;
                let resolve = |n: &str| env.get(n).map(|b| b.value.clone());
                df_column_verb(&lf, name, args, &resolve, line, col)
            }
            "head" => {
                if args.len() != 1 {
                    return Err(HelixError::new("`head` takes a row count", line, col)
                        .hint("e.g. `df.head(5)`."));
                }
                let v = self.eval(&args[0])?;
                let n = as_int(&v, "head", line, col)?.max(0) as usize;
                Ok(Value::DataFrame(Rc::new(dataframe::head(&lf, n))))
            }
            "count" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`count` takes no arguments", line, col));
                }
                Ok(Value::Int(dataframe::row_count(&lf, line, col)? as i64))
            }
            "cache" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`cache` takes no arguments", line, col)
                        .hint("e.g. `big = read_csv(\"x.csv\").cache()` to reuse without re-scanning."));
                }
                Ok(Value::DataFrame(Rc::new(dataframe::cache(&lf, line, col)?)))
            }
            "columns" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`columns` takes no arguments", line, col));
                }
                let names: Vec<Value> = dataframe::column_names(&lf, line, col)?
                    .into_iter()
                    .map(|c| Value::Str(Rc::new(c)))
                    .collect();
                Ok(Value::Array(Rc::new(names)))
            }
            _ => {
                const DF_METHODS: &[&str] = &[
                    "where", "select", "sort", "group", "head", "count", "columns", "cache",
                ];
                let mut err = HelixError::new(
                    format!("a DataFrame has no method `{}`", name),
                    line,
                    col,
                );
                if let Some(s) = suggest(name, DF_METHODS) {
                    err = err.hint(format!("did you mean `{}`?", s));
                } else {
                    err = err.hint(format!("DataFrame methods: {}", DF_METHODS.join(", ")));
                }
                Err(err)
            }
        }
    }

    fn eval_groupby_method(
        &mut self,
        lf: Rc<LazyFrame>,
        keys: Rc<Vec<String>>,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        groupby_agg(&lf, &keys, name, args, line, col)
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

    pub(crate) fn call_builtin(
        &mut self,
        name: &str,
        args: Vec<Value>,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match name {
            "print" => {
                let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect();
                println!("{}", parts.join(" "));
                Ok(Value::Unit)
            }
            "dna" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => make_dna(s, line, col),
                    other => Err(type_err("dna", "a string", other, line, col)),
                }
            }
            "read_csv" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let lf = dataframe::read_csv(s, line, col)?;
                        Ok(Value::DataFrame(Rc::new(lf)))
                    }
                    other => Err(type_err("read_csv", "a string path", other, line, col)),
                }
            }
            "range" => match args.len() {
                1 => {
                    let n = as_int(&args[0], "range", line, col)?;
                    int_range(0, n, line, col)
                }
                2 => {
                    let a = as_int(&args[0], "range", line, col)?;
                    let b = as_int(&args[1], "range", line, col)?;
                    int_range(a, b, line, col)
                }
                _ => Err(HelixError::new(
                    format!("`range` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("use `range(n)` or `range(start, stop)`.")),
            },
            "read_parquet" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let lf = dataframe::read_parquet(s, line, col)?;
                        Ok(Value::DataFrame(Rc::new(lf)))
                    }
                    other => Err(type_err("read_parquet", "a string path", other, line, col)),
                }
            }
            "read_fasta" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => crate::bio::read_fasta(s, line, col),
                    other => Err(type_err("read_fasta", "a string path", other, line, col)),
                }
            }
            "write_parquet" => {
                arity(name, &args, 2, line, col)?;
                match (&args[0], &args[1]) {
                    (Value::DataFrame(lf), Value::Str(p)) => {
                        dataframe::write_parquet(lf, p, line, col)?;
                        Ok(Value::Unit)
                    }
                    (Value::DataFrame(_), other) => {
                        Err(type_err("write_parquet", "a string path", other, line, col))
                    }
                    (other, _) => Err(type_err("write_parquet", "a DataFrame", other, line, col)),
                }
            }
            // ---- tensor constructors ----
            "tensor" => {
                arity(name, &args, 1, line, col)?;
                Ok(Value::Tensor(Rc::new(tensor::from_value(&args[0], line, col)?)))
            }
            "zeros" | "ones" => {
                arity(name, &args, 1, line, col)?;
                let shape = tensor_shape_arg(&args[0], line, col)?;
                // Guard the element count (checked, so the product can't overflow
                // and ask ndarray for an absurd allocation that aborts).
                const MAX_ELEMS: usize = 1_000_000_000; // ~8 GB of f64
                let count = shape.iter().try_fold(1usize, |acc, &d| acc.checked_mul(d));
                if !matches!(count, Some(c) if c <= MAX_ELEMS) {
                    return Err(HelixError::new(
                        format!("tensor shape {:?} is too large to allocate", shape),
                        line,
                        col,
                    )
                    .hint("the total element count must stay under 1 billion."));
                }
                let t = if name == "zeros" {
                    tensor::zeros(&shape)
                } else {
                    tensor::ones(&shape)
                };
                Ok(Value::Tensor(Rc::new(t)))
            }
            "eye" => {
                arity(name, &args, 1, line, col)?;
                let n = as_int(&args[0], "eye", line, col)?;
                if n < 0 {
                    return Err(HelixError::new("`eye` needs a non-negative size", line, col));
                }
                Ok(Value::Tensor(Rc::new(tensor::eye(n as usize))))
            }
            // ---- math standard library (broadcasts over arrays, propagates missing) ----
            "sqrt" | "cbrt" | "exp" | "ln" | "log10" | "log2" | "sin" | "cos" | "tan" | "asin"
            | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "degrees" | "radians" => {
                arity(name, &args, 1, line, col)?;
                let f: fn(f64) -> f64 = match name {
                    "sqrt" => f64::sqrt,
                    "cbrt" => f64::cbrt,
                    "exp" => f64::exp,
                    "ln" => f64::ln,
                    "log10" => f64::log10,
                    "log2" => f64::log2,
                    "sin" => f64::sin,
                    "cos" => f64::cos,
                    "tan" => f64::tan,
                    "asin" => f64::asin,
                    "acos" => f64::acos,
                    "atan" => f64::atan,
                    "sinh" => f64::sinh,
                    "cosh" => f64::cosh,
                    "tanh" => f64::tanh,
                    "degrees" => f64::to_degrees,
                    "radians" => f64::to_radians,
                    _ => unreachable!(),
                };
                apply_float_fn(name, f, &args[0], line, col)
            }
            "floor" | "ceil" | "round" | "trunc" => {
                arity(name, &args, 1, line, col)?;
                let f: fn(f64) -> f64 = match name {
                    "floor" => f64::floor,
                    "ceil" => f64::ceil,
                    "round" => f64::round,
                    "trunc" => f64::trunc,
                    _ => unreachable!(),
                };
                apply_round_fn(name, f, &args[0], line, col)
            }
            "abs" => {
                arity(name, &args, 1, line, col)?;
                broadcast_unary(&args[0], &|s| match s {
                    Value::Int(i) => Ok(Value::Int(i.abs())),
                    Value::Float(x) => Ok(Value::Float(x.abs())),
                    other => Err(type_err("abs", "a number or array of numbers", other, line, col)),
                })
            }
            "sign" => {
                arity(name, &args, 1, line, col)?;
                broadcast_unary(&args[0], &|s| match s {
                    Value::Int(i) => Ok(Value::Int(i.signum())),
                    Value::Float(x) => Ok(Value::Int(if *x > 0.0 {
                        1
                    } else if *x < 0.0 {
                        -1
                    } else {
                        0
                    })),
                    other => Err(type_err("sign", "a number or array of numbers", other, line, col)),
                })
            }
            "log" => {
                arity(name, &args, 2, line, col)?;
                match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((x, base)) => Ok(Value::Float(x.log(base))),
                }
            }
            "atan2" => {
                arity(name, &args, 2, line, col)?;
                match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((y, x)) => Ok(Value::Float(y.atan2(x))),
                }
            }
            "hypot" => {
                arity(name, &args, 2, line, col)?;
                match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((a, b)) => Ok(Value::Float(a.hypot(b))),
                }
            }
            "min" | "max" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let a = args[0]
                    .as_f64()
                    .ok_or_else(|| type_err(name, "a number", &args[0], line, col))?;
                let b = args[1]
                    .as_f64()
                    .ok_or_else(|| type_err(name, "a number", &args[1], line, col))?;
                let pick_first = if name == "min" { a <= b } else { a >= b };
                Ok(if pick_first { args[0].clone() } else { args[1].clone() })
            }
            _ => {
                let mut err =
                    HelixError::new(format!("`{}` is not a known function", name), line, col);
                if let Some(s) = suggest(name, BUILTIN_FNS) {
                    err = err.hint(format!("did you mean `{}`?", s));
                }
                Err(err)
            }
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

/// A DataFrame column argument is a bare identifier (`select(name, age)`).
fn arg_as_column_name(e: &Expr, line: usize, col: usize) -> Result<String, HelixError> {
    match e {
        Expr::Ident { name, .. } => Ok(name.clone()),
        _ => Err(
            HelixError::new("expected a column name", line, col)
                .hint("write a bare column name, e.g. `df.select(name, age)`."),
        ),
    }
}

/// Column-name arguments for `select`/`sort`/`group` (each must be a bare ident).
pub(crate) fn column_name_args(
    args: &[Expr],
    line: usize,
    col: usize,
) -> Result<Vec<String>, HelixError> {
    if args.is_empty() {
        return Err(HelixError::new("expected at least one column name", line, col)
            .hint("e.g. `df.select(name, age)`."));
    }
    args.iter().map(|a| arg_as_column_name(a, line, col)).collect()
}

/// A DataFrame column-verb that takes *unevaluated* column/predicate ASTs:
/// `where`/`filter`/`select`/`sort`/`group`. The single source of truth for both
/// the tree-walker ([`Interp::eval_df_method`]) and the VM (`Op::DfDispatch`).
/// `resolve_var` resolves a bare name that is *not* a column to a Helix variable's
/// value (for predicates like `where(age > threshold)`).
pub(crate) fn df_column_verb(
    lf: &Rc<LazyFrame>,
    name: &str,
    args: &[Expr],
    resolve_var: &dyn Fn(&str) -> Option<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "where" | "filter" => {
            if args.len() != 1 {
                return Err(HelixError::new(format!("`{}` takes one predicate", name), line, col)
                    .hint("e.g. `patients.where(age > 40)`."));
            }
            let columns = dataframe::column_names(lf, line, col)?;
            let pred = dataframe::to_polars(&args[0], &columns, resolve_var)?;
            Ok(Value::DataFrame(Rc::new(dataframe::filter(lf, pred))))
        }
        "select" => {
            let names = column_name_args(args, line, col)?;
            Ok(Value::DataFrame(Rc::new(dataframe::select(lf, &names))))
        }
        "sort" => {
            let names = column_name_args(args, line, col)?;
            Ok(Value::DataFrame(Rc::new(dataframe::sort(lf, &names))))
        }
        "group" => {
            let names = column_name_args(args, line, col)?;
            Ok(Value::GroupBy { lf: lf.clone(), keys: Rc::new(names) })
        }
        _ => unreachable!("df_column_verb only handles where/filter/select/sort/group"),
    }
}

/// A grouped-DataFrame aggregation over one column: `mean`/`sum`/`min`/`max`/
/// `count`/`std`. Shared by the tree-walker and the VM (`Op::DfDispatch`).
pub(crate) fn groupby_agg(
    lf: &Rc<LazyFrame>,
    keys: &Rc<Vec<String>>,
    name: &str,
    args: &[Expr],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "mean" | "sum" | "min" | "max" | "count" | "std" => {
            if args.len() != 1 {
                return Err(HelixError::new(format!("grouped `{}` takes one column", name), line, col)
                    .hint("e.g. `genes.group(species).mean(expression)`."));
            }
            let value_col = arg_as_column_name(&args[0], line, col)?;
            let out = dataframe::group_agg(lf, keys, name, &value_col, line, col)?;
            Ok(Value::DataFrame(Rc::new(out)))
        }
        _ => Err(HelixError::new(
            format!("a grouped DataFrame has no aggregation `{}`", name),
            line,
            col,
        )
        .hint("try mean, sum, min, max, count, or std.")),
    }
}

fn comp_arity(name: &str, example: &str, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!("`{}` takes exactly one expression", name),
        line,
        col,
    )
    .hint(format!("e.g. `xs.{}{}`.", name, example))
}

/// Every built-in function name, used both for routing and for "did you mean".
pub(crate) const BUILTIN_FNS: &[&str] = &[
    "print", "dna", "range", "read_csv", "read_parquet", "read_fasta", "write_parquet", "tensor",
    "zeros",
    "ones", "eye", "sqrt", "cbrt", "abs", "exp", "ln", "log10", "log2", "log", "sin", "cos", "tan",
    "asin", "acos", "atan", "atan2", "sinh", "cosh", "tanh", "floor", "ceil", "round", "trunc",
    "sign", "degrees", "radians", "hypot", "min", "max",
];

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

/// Resolve a Python-style slice into the concrete element indices to take.
fn slice_indices(len: i64, start: Option<i64>, stop: Option<i64>, step: i64) -> Vec<usize> {
    let (lower, upper) = if step < 0 { (-1i64, len - 1) } else { (0i64, len) };
    let clamp = |x: i64| -> i64 {
        let mut v = x;
        if v < 0 {
            // saturating so an extreme negative bound can't overflow i64
            v = v.saturating_add(len);
            if v < lower {
                v = lower;
            }
        } else if v > upper {
            v = upper;
        }
        v
    };
    let start = match start {
        Some(s) => clamp(s),
        None => if step < 0 { upper } else { lower },
    };
    let stop = match stop {
        Some(s) => clamp(s),
        None => if step < 0 { lower } else { upper },
    };
    let mut out = Vec::new();
    let mut i = start;
    if step > 0 {
        while i < stop {
            out.push(i as usize);
            i += step;
        }
    } else {
        while i > stop {
            out.push(i as usize);
            i += step;
        }
    }
    out
}

pub(crate) fn eval_slice(
    recv: &Value,
    start: Option<i64>,
    stop: Option<i64>,
    step: i64,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match recv {
        Value::Array(items) => {
            let idxs = slice_indices(items.len() as i64, start, stop, step);
            let out: Vec<Value> = idxs.iter().map(|&i| items[i].clone()).collect();
            Ok(Value::Array(Rc::new(out)))
        }
        Value::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            let idxs = slice_indices(chars.len() as i64, start, stop, step);
            let out: String = idxs.iter().map(|&i| chars[i]).collect();
            Ok(Value::Str(Rc::new(out)))
        }
        Value::Dna(s) => {
            let chars: Vec<char> = s.chars().collect();
            let idxs = slice_indices(chars.len() as i64, start, stop, step);
            let out: String = idxs.iter().map(|&i| chars[i]).collect();
            Ok(Value::Dna(Rc::new(out)))
        }
        Value::Tensor(t) => {
            if t.ndim() == 0 {
                return Err(HelixError::new("cannot slice a 0-D (scalar) tensor", line, col));
            }
            let idxs = slice_indices(t.shape()[0] as i64, start, stop, step);
            Ok(tensor::slice_first(t, &idxs))
        }
        Value::Missing => Ok(Value::Missing),
        other => Err(HelixError::new(
            format!("a value of type {} cannot be sliced", other.type_name()),
            line,
            col,
        )
        .hint("slicing works on arrays, strings, DNA, and tensors (first axis).")),
    }
}

/// Unpack a tuple/array into exactly `n` values for destructuring (shared by
/// both engines). Errors if the value isn't a tuple/array, or the arity is wrong.
pub(crate) fn destructure_parts(
    v: &Value,
    n: usize,
    line: usize,
    col: usize,
) -> Result<Vec<Value>, HelixError> {
    let parts = match v {
        Value::Tuple(t) => (**t).clone(),
        Value::Array(a) => (**a).clone(),
        other => {
            return Err(HelixError::new(
                format!(
                    "cannot destructure a value of type {} into {} names",
                    other.type_name(),
                    n
                ),
                line,
                col,
            )
            .hint("the right-hand side must be a tuple or array, e.g. `a, b = (1, 2)`."))
        }
    };
    if parts.len() != n {
        return Err(HelixError::new(
            format!("cannot destructure {} values into {} names", parts.len(), n),
            line,
            col,
        ));
    }
    Ok(parts)
}

/// Split a comprehension element into `n` parts for a multi-binder pattern
/// (`xs.map((a, b) => ...)`). Distinct from [`destructure_parts`] (the `a, b = …`
/// statement form) in its wording — "parameters" / "lambda expects N values". The
/// single source of truth for both the tree-walker ([`Interp::eval_with_pattern`])
/// and the VM (`Op::DestructureBind`), so the two engines never diverge here.
pub(crate) fn pattern_parts(
    v: &Value,
    n: usize,
    line: usize,
    col: usize,
) -> Result<Vec<Value>, HelixError> {
    let parts = match v {
        Value::Tuple(t) => (**t).clone(),
        Value::Array(a) => (**a).clone(),
        other => {
            return Err(HelixError::new(
                format!(
                    "cannot destructure a value of type {} into {} parameters",
                    other.type_name(),
                    n
                ),
                line,
                col,
            )
            .hint("the element must be a tuple or array (e.g. from `zip`/`enumerate`)."))
        }
    };
    if parts.len() != n {
        return Err(HelixError::new(
            format!("lambda expects {} values, but the element has {}", n, parts.len()),
            line,
            col,
        ));
    }
    Ok(parts)
}

/// DataFrame methods whose arguments are plain *values* (not column refs), so
/// the VM can dispatch them after evaluating args. The column-argument verbs
/// (`where`/`select`/`sort`/`group`) are not here — they take unevaluated ASTs
/// and remain on the tree-walker. Mirrors the matching arms of `eval_df_method`.
pub(crate) fn df_value_method(
    lf: &Rc<LazyFrame>,
    name: &str,
    args: Vec<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "count" => {
            if !args.is_empty() {
                return Err(HelixError::new("`count` takes no arguments", line, col));
            }
            Ok(Value::Int(dataframe::row_count(lf, line, col)? as i64))
        }
        "columns" => {
            if !args.is_empty() {
                return Err(HelixError::new("`columns` takes no arguments", line, col));
            }
            let names: Vec<Value> = dataframe::column_names(lf, line, col)?
                .into_iter()
                .map(|c| Value::Str(Rc::new(c)))
                .collect();
            Ok(Value::Array(Rc::new(names)))
        }
        "cache" => {
            if !args.is_empty() {
                return Err(HelixError::new("`cache` takes no arguments", line, col)
                    .hint("e.g. `big = read_csv(\"x.csv\").cache()` to reuse without re-scanning."));
            }
            Ok(Value::DataFrame(Rc::new(dataframe::cache(lf, line, col)?)))
        }
        "head" => {
            if args.len() != 1 {
                return Err(HelixError::new("`head` takes a row count", line, col)
                    .hint("e.g. `df.head(5)`."));
            }
            let n = as_int(&args[0], "head", line, col)?.max(0) as usize;
            Ok(Value::DataFrame(Rc::new(dataframe::head(lf, n))))
        }
        _ => {
            const DF_METHODS: &[&str] =
                &["where", "select", "sort", "group", "head", "count", "columns", "cache"];
            let mut err =
                HelixError::new(format!("a DataFrame has no method `{}`", name), line, col);
            if let Some(s) = suggest(name, DF_METHODS) {
                err = err.hint(format!("did you mean `{}`?", s));
            } else {
                err = err.hint(format!("DataFrame methods: {}", DF_METHODS.join(", ")));
            }
            Err(err)
        }
    }
}

/// Record field access `r.name`. Shared by the tree-walker and the VM.
pub(crate) fn eval_field(r: &Value, name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    match r {
        Value::Record(fields) => fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| {
                let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
                let mut err =
                    HelixError::new(format!("record has no field `{}`", name), line, col);
                if let Some(s) = suggest(name, &keys) {
                    err = err.hint(format!("did you mean `{}`?", s));
                } else {
                    err = err.hint(format!("fields: {}", keys.join(", ")));
                }
                err
            }),
        Value::Missing => Ok(Value::Missing), // propagate
        other => Err(HelixError::new(
            format!("a value of type {} has no field `{}`", other.type_name(), name),
            line,
            col,
        )
        .hint("field access `x.name` works on records; methods need `()`.")),
    }
}

/// Resolve one slice bound value to an optional index (shared by both engines):
/// an `Int` is the bound, `missing` means omitted, anything else is an error.
pub(crate) fn slice_bound(v: &Value, line: usize, col: usize) -> Result<Option<i64>, HelixError> {
    match v {
        Value::Int(i) => Ok(Some(*i)),
        Value::Missing => Ok(None),
        other => Err(type_err("slice bound", "an integer", other, line, col)),
    }
}

pub(crate) fn eval_index(recv: &Value, idx: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let i = match idx {
        Value::Int(i) => *i,
        other => return Err(type_err("index", "an integer", other, line, col)),
    };
    match recv {
        Value::Array(items) | Value::Tuple(items) => {
            let n = items.len() as i64;
            let real = if i < 0 { n + i } else { i };
            if real < 0 || real >= n {
                return Err(HelixError::new(
                    format!("index {} is out of bounds for length {}", i, n),
                    line,
                    col,
                )
                .hint("valid indices run from 0 to length-1; negative indices count from the end."));
            }
            Ok(items[real as usize].clone())
        }
        Value::Str(s) | Value::Dna(s) => {
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let real = if i < 0 { n + i } else { i };
            if real < 0 || real >= n {
                return Err(HelixError::new(
                    format!("index {} is out of bounds for length {}", i, n),
                    line,
                    col,
                ));
            }
            Ok(Value::Str(Rc::new(chars[real as usize].to_string())))
        }
        Value::Tensor(t) => tensor::index_first(t, i, line, col),
        other => Err(HelixError::new(
            format!("a value of type {} cannot be indexed", other.type_name()),
            line,
            col,
        )),
    }
}

// ---------- binary operators ----------

pub(crate) fn eval_binary(
    op: &BinOp,
    l: Value,
    r: Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    use BinOp::*;
    // Missing propagates through every arithmetic, comparison, and equality
    // operator — including `missing == missing` -> missing, so equality can
    // never be used to test for missingness. Use `.is_missing()` instead.
    if matches!(l, Value::Missing) || matches!(r, Value::Missing) {
        return Ok(Value::Missing);
    }

    // Elementwise broadcasting for arithmetic: array⊕scalar, scalar⊕array, and
    // array⊕array (same length). Comparison/equality deliberately do NOT
    // broadcast — `==` is whole-value, avoiding NumPy's "ambiguous truth value"
    // trap; use `.map`/`.where` for elementwise predicates.
    if matches!(op, Add | Sub | Mul | Div | Mod | Pow) {
        match (&l, &r) {
            (Value::Array(a), Value::Array(b)) => {
                if a.len() != b.len() {
                    return Err(HelixError::new(
                        format!(
                            "cannot `{}` arrays of different lengths ({} and {})",
                            op.symbol(),
                            a.len(),
                            b.len()
                        ),
                        line,
                        col,
                    )
                    .hint("elementwise operations need matching lengths."));
                }
                let mut out = Vec::with_capacity(a.len());
                for (x, y) in a.iter().zip(b.iter()) {
                    out.push(eval_binary(op, x.clone(), y.clone(), line, col)?);
                }
                return Ok(Value::Array(Rc::new(out)));
            }
            (Value::Array(a), scalar) => {
                let mut out = Vec::with_capacity(a.len());
                for x in a.iter() {
                    out.push(eval_binary(op, x.clone(), scalar.clone(), line, col)?);
                }
                return Ok(Value::Array(Rc::new(out)));
            }
            (scalar, Value::Array(b)) => {
                let mut out = Vec::with_capacity(b.len());
                for y in b.iter() {
                    out.push(eval_binary(op, scalar.clone(), y.clone(), line, col)?);
                }
                return Ok(Value::Array(Rc::new(out)));
            }
            // Tensor arithmetic: tensor⊕tensor (NumPy broadcasting), tensor⊕scalar.
            (Value::Tensor(a), Value::Tensor(b)) => {
                return Ok(Value::Tensor(Rc::new(tensor::elementwise(op, a, b, line, col)?)));
            }
            (Value::Tensor(a), s) if s.as_f64().is_some() => {
                return Ok(Value::Tensor(Rc::new(tensor::scalar_op(
                    op,
                    a,
                    s.as_f64().unwrap(),
                    true,
                ))));
            }
            (s, Value::Tensor(b)) if s.as_f64().is_some() => {
                return Ok(Value::Tensor(Rc::new(tensor::scalar_op(
                    op,
                    b,
                    s.as_f64().unwrap(),
                    false,
                ))));
            }
            _ => {}
        }
    }

    match op {
        Add | Sub | Mul => arith(op, &l, &r, line, col),
        Div => {
            let a = num_operand(op, &l, line, col)?;
            let b = num_operand(op, &r, line, col)?;
            if b == 0.0 {
                return Err(HelixError::new("division by zero", line, col)
                    .hint("guard the denominator, e.g. `if d != 0` (coming soon) or check your data."));
            }
            Ok(Value::Float(a / b))
        }
        Mod => match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => {
                if *b == 0 {
                    Err(HelixError::new("modulo by zero", line, col))
                } else {
                    Ok(Value::Int(a.rem_euclid(*b)))
                }
            }
            _ => {
                let a = num_operand(op, &l, line, col)?;
                let b = num_operand(op, &r, line, col)?;
                Ok(Value::Float(a.rem_euclid(b)))
            }
        },
        Pow => match (&l, &r) {
            // Integer power stays Int when the exponent is a non-negative,
            // in-range integer and the result doesn't overflow.
            (Value::Int(a), Value::Int(b)) if *b >= 0 && *b <= u32::MAX as i64 => {
                match a.checked_pow(*b as u32) {
                    Some(v) => Ok(Value::Int(v)),
                    None => Ok(Value::Float((*a as f64).powf(*b as f64))),
                }
            }
            _ => {
                let a = num_operand(op, &l, line, col)?;
                let b = num_operand(op, &r, line, col)?;
                Ok(Value::Float(a.powf(b)))
            }
        },
        Eq => Ok(Value::Bool(values_equal(&l, &r))),
        Ne => Ok(Value::Bool(!values_equal(&l, &r))),
        Lt | Gt | Le | Ge => compare(op, &l, &r, line, col),
        And | Or | Coalesce => unreachable!("handled with short-circuit in eval"),
    }
}

fn arith(op: &BinOp, l: &Value, r: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    if let (Value::Int(a), Value::Int(b)) = (l, r) {
        // Integer overflow wraps (two's complement), matching the JIT and Rust
        // release / Go / Java semantics — never a debug-build panic. Values beyond
        // the i64 range should use floats.
        let v = match op {
            BinOp::Add => a.wrapping_add(*b),
            BinOp::Sub => a.wrapping_sub(*b),
            BinOp::Mul => a.wrapping_mul(*b),
            _ => unreachable!(),
        };
        return Ok(Value::Int(v));
    }
    let a = num_operand(op, l, line, col)?;
    let b = num_operand(op, r, line, col)?;
    let v = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        _ => unreachable!(),
    };
    Ok(Value::Float(v))
}

fn num_operand(op: &BinOp, v: &Value, line: usize, col: usize) -> Result<f64, HelixError> {
    v.as_f64().ok_or_else(|| {
        HelixError::new(
            format!(
                "operator `{}` needs numbers, but got a {}",
                op.symbol(),
                v.type_name()
            ),
            line,
            col,
        )
    })
}

fn values_equal(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => (*a as f64) == *b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Dna(a), Value::Dna(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| values_equal(x, y))
        }
        _ => false,
    }
}

fn compare(op: &BinOp, l: &Value, r: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let ord = match (l, r) {
        (Value::Str(a), Value::Str(b)) => a.cmp(b),
        // Compare integers exactly as i64 — a prior `as f64` cast lost precision
        // above 2^53 and disagreed with the JIT. Now all engines agree.
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        _ => {
            let a = num_operand(op, l, line, col)?;
            let b = num_operand(op, r, line, col)?;
            a.partial_cmp(&b).ok_or_else(|| {
                HelixError::new("cannot compare these values (NaN?)", line, col)
            })?
        }
    };
    use std::cmp::Ordering::*;
    let res = match op {
        BinOp::Lt => ord == Less,
        BinOp::Gt => ord == Greater,
        BinOp::Le => ord != Greater,
        BinOp::Ge => ord != Less,
        _ => unreachable!(),
    };
    Ok(Value::Bool(res))
}

// ---------- methods ----------

const ARRAY_METHODS: &[&str] = &[
    "mean", "std", "sum", "min", "max", "count", "normalize", "sort", "reverse", "first", "last",
    "map", "filter", "where", "reduce", "any", "all", "take", "drop", "zip", "enumerate", "top",
    "drop_missing", "is_missing",
];
const STRING_METHODS: &[&str] = &["upper", "lower", "count", "reverse"];
const DNA_METHODS: &[&str] = &[
    "gc_content",
    "reverse_complement",
    "complement",
    "kmers",
    "find",
    "length",
];

pub(crate) fn call_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // `is_missing` is universal: true only for the `missing` value itself.
    if name == "is_missing" {
        if !args.is_empty() {
            return Err(HelixError::new("`is_missing` takes no arguments", line, col));
        }
        return Ok(Value::Bool(matches!(recv, Value::Missing)));
    }
    match recv {
        Value::Array(items) => array_method(items, name, &args, line, col),
        Value::Str(s) => string_method(s, name, &args, line, col),
        Value::Dna(s) => dna_method(s, name, &args, line, col),
        Value::Tensor(t) => crate::tensor::method(t, name, &args, line, col),
        other => Err(HelixError::new(
            format!("a {} has no method `{}`", other.type_name(), name),
            line,
            col,
        )),
    }
}

fn numeric_vec(items: &[Value], who: &str, line: usize, col: usize) -> Result<Vec<f64>, HelixError> {
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.iter().enumerate() {
        match v.as_f64() {
            Some(x) => out.push(x),
            None => {
                return Err(HelixError::new(
                    format!(
                        "`{}` needs an array of numbers, but element {} is a {}",
                        who,
                        i,
                        v.type_name()
                    ),
                    line,
                    col,
                ))
            }
        }
    }
    Ok(out)
}

/// True if any element is `missing` — every numeric aggregation propagates it
/// (ADR-0001), returning `missing` rather than a number.
fn has_missing(items: &[Value]) -> bool {
    items.iter().any(|v| matches!(v, Value::Missing))
}

fn array_method(
    items: &Rc<Vec<Value>>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let no_args = |n: &str| {
        if args.is_empty() {
            Ok(())
        } else {
            Err(HelixError::new(
                format!("`{}` takes no arguments, got {}", n, args.len()),
                line,
                col,
            ))
        }
    };

    match name {
        "count" => {
            no_args(name)?;
            // Counts every slot, including `missing` holes.
            Ok(Value::Int(items.len() as i64))
        }
        "mean" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "mean", line, col)?;
            empty_guard(&xs, "mean", line, col)?;
            Ok(Value::Float(neumaier_sum(&xs) / xs.len() as f64))
        }
        "std" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "std", line, col)?;
            empty_guard(&xs, "std", line, col)?;
            Ok(Value::Float(population_std(&xs)))
        }
        "sum" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            // Keep Int if every element is an Int; otherwise compensated float sum.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let s: i64 = items
                    .iter()
                    .map(|v| if let Value::Int(i) = v { *i } else { 0 })
                    .sum();
                Ok(Value::Int(s))
            } else {
                let xs = numeric_vec(items, "sum", line, col)?;
                Ok(Value::Float(neumaier_sum(&xs)))
            }
        }
        "min" | "max" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, name, line, col)?;
            empty_guard(&xs, name, line, col)?;
            let mut best_idx = 0;
            for (i, &x) in xs.iter().enumerate() {
                let better = if name == "min" { x < xs[best_idx] } else { x > xs[best_idx] };
                if better {
                    best_idx = i;
                }
            }
            Ok(items[best_idx].clone())
        }
        "normalize" => {
            no_args(name)?;
            if has_missing(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "normalize", line, col)?;
            empty_guard(&xs, "normalize", line, col)?;
            let mean = neumaier_sum(&xs) / xs.len() as f64;
            let sd = population_std(&xs);
            if sd == 0.0 {
                return Err(HelixError::new(
                    "cannot normalize: all values are identical (standard deviation is 0)",
                    line,
                    col,
                )
                .hint("normalize rescales by spread; a constant column has no spread."));
            }
            let out: Vec<Value> = xs.iter().map(|x| Value::Float((x - mean) / sd)).collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "drop_missing" => {
            no_args(name)?;
            let out: Vec<Value> = items
                .iter()
                .filter(|v| !matches!(v, Value::Missing))
                .cloned()
                .collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "sort" => {
            no_args(name)?;
            let mut sorted: Vec<Value> = (**items).clone();
            // numeric sort if all numeric, else lexical if all strings
            if items.iter().all(|v| v.as_f64().is_some()) {
                sorted.sort_by(|a, b| {
                    a.as_f64()
                        .unwrap()
                        .partial_cmp(&b.as_f64().unwrap())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                return Err(HelixError::new(
                    "`sort` needs an array of all numbers or all strings",
                    line,
                    col,
                ));
            }
            Ok(Value::Array(Rc::new(sorted)))
        }
        "reverse" => {
            no_args(name)?;
            let mut v: Vec<Value> = (**items).clone();
            v.reverse();
            Ok(Value::Array(Rc::new(v)))
        }
        "first" | "last" => {
            no_args(name)?;
            if items.is_empty() {
                return Err(HelixError::new(
                    format!("cannot take `{}` of an empty array", name),
                    line,
                    col,
                ));
            }
            let idx = if name == "first" { 0 } else { items.len() - 1 };
            Ok(items[idx].clone())
        }
        "take" => {
            arity("take", args, 1, line, col)?;
            let n = as_int(&args[0], "take", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().take(n).cloned().collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "drop" => {
            arity("drop", args, 1, line, col)?;
            let n = as_int(&args[0], "drop", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().skip(n).cloned().collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "zip" => {
            arity("zip", args, 1, line, col)?;
            let other = match &args[0] {
                Value::Array(a) => a.clone(),
                v => {
                    return Err(HelixError::new(
                        format!("`zip` needs an array, but got a {}", v.type_name()),
                        line,
                        col,
                    )
                    .hint("e.g. `xs.zip(ys)` pairs elements positionally."))
                }
            };
            let n = items.len().min(other.len());
            let out: Vec<Value> = (0..n)
                .map(|i| Value::Tuple(Rc::new(vec![items[i].clone(), other[i].clone()])))
                .collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "enumerate" => {
            no_args(name)?;
            let out: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(i, v)| Value::Tuple(Rc::new(vec![Value::Int(i as i64), v.clone()])))
                .collect();
            Ok(Value::Array(Rc::new(out)))
        }
        "top" => {
            arity("top", args, 1, line, col)?;
            let n = as_int(&args[0], "top", line, col)?.max(0) as usize;
            // Frequency count by value equality, ordered by count desc then value asc.
            let mut counts: Vec<(Value, i64)> = Vec::new();
            for v in items.iter() {
                if let Some(e) = counts.iter_mut().find(|(k, _)| values_equal(k, v)) {
                    e.1 += 1;
                } else {
                    counts.push((v.clone(), 1));
                }
            }
            counts.sort_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| a.0.to_string().cmp(&b.0.to_string()))
            });
            let out: Vec<Value> = counts
                .into_iter()
                .take(n)
                .map(|(v, c)| Value::Tuple(Rc::new(vec![v, Value::Int(c)])))
                .collect();
            Ok(Value::Array(Rc::new(out)))
        }
        _ => Err(unknown_method("Array", name, ARRAY_METHODS, line, col)),
    }
}

fn empty_guard(xs: &[f64], who: &str, line: usize, col: usize) -> Result<(), HelixError> {
    if xs.is_empty() {
        Err(HelixError::new(
            format!("cannot compute `{}` of an empty array", who),
            line,
            col,
        ))
    } else {
        Ok(())
    }
}

/// Neumaier's improved Kahan compensated summation — bounds the rounding error of
/// a float sum, recovering terms that naive left-to-right summation would lose to
/// catastrophic cancellation. Every float aggregation routes through it.
pub(crate) fn neumaier_sum(xs: &[f64]) -> f64 {
    let mut sum = 0.0;
    let mut c = 0.0; // running compensation for lost low-order bits
    for &x in xs {
        let t = sum + x;
        if sum.abs() >= x.abs() {
            c += (sum - t) + x;
        } else {
            c += (x - t) + sum;
        }
        sum = t;
    }
    sum + c
}

fn population_std(xs: &[f64]) -> f64 {
    let mean = neumaier_sum(xs) / xs.len() as f64;
    let sq: Vec<f64> = xs.iter().map(|x| (x - mean).powi(2)).collect();
    let var = neumaier_sum(&sq) / xs.len() as f64;
    var.sqrt()
}

fn string_method(
    s: &Rc<String>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    if !args.is_empty() {
        return Err(HelixError::new(
            format!("`{}` takes no arguments, got {}", name, args.len()),
            line,
            col,
        ));
    }
    match name {
        "upper" => Ok(Value::Str(Rc::new(s.to_uppercase()))),
        "lower" => Ok(Value::Str(Rc::new(s.to_lowercase()))),
        "count" => Ok(Value::Int(s.chars().count() as i64)),
        "reverse" => Ok(Value::Str(Rc::new(s.chars().rev().collect()))),
        _ => Err(unknown_method("String", name, STRING_METHODS, line, col)),
    }
}

fn dna_method(
    s: &Rc<String>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "length" => {
            if !args.is_empty() {
                return Err(HelixError::new("`length` takes no arguments", line, col));
            }
            Ok(Value::Int(s.len() as i64))
        }
        "gc_content" => {
            if !args.is_empty() {
                return Err(HelixError::new("`gc_content` takes no arguments", line, col));
            }
            if s.is_empty() {
                return Err(HelixError::new(
                    "cannot compute `gc_content` of an empty sequence",
                    line,
                    col,
                ));
            }
            let gc = s.chars().filter(|c| *c == 'G' || *c == 'C').count();
            Ok(Value::Float(gc as f64 / s.len() as f64))
        }
        "complement" => {
            if !args.is_empty() {
                return Err(HelixError::new("`complement` takes no arguments", line, col));
            }
            Ok(Value::Dna(Rc::new(complement(s))))
        }
        "reverse_complement" => {
            if !args.is_empty() {
                return Err(HelixError::new(
                    "`reverse_complement` takes no arguments",
                    line,
                    col,
                ));
            }
            let rc: String = complement(s).chars().rev().collect();
            Ok(Value::Dna(Rc::new(rc)))
        }
        "find" => {
            arity("find", args, 1, line, col)?;
            let needle = match &args[0] {
                Value::Str(p) => (**p).clone(),
                Value::Dna(p) => (**p).clone(),
                v => {
                    return Err(HelixError::new(
                        format!("`find` needs a string or DNA pattern, but got a {}", v.type_name()),
                        line,
                        col,
                    ))
                }
            };
            // ACGT is ASCII, so the byte offset is the base offset.
            match s.find(&needle) {
                Some(idx) => Ok(Value::Int(idx as i64)),
                None => Ok(Value::Missing),
            }
        }
        "kmers" => {
            arity("kmers", args, 1, line, col)?;
            let k = as_int(&args[0], "kmers", line, col)?;
            if k <= 0 {
                return Err(HelixError::new(
                    format!("`kmers` needs a positive length, got {}", k),
                    line,
                    col,
                ));
            }
            let k = k as usize;
            let chars: Vec<char> = s.chars().collect();
            if k > chars.len() {
                return Err(HelixError::new(
                    format!(
                        "k-mer length {} is longer than the sequence (length {})",
                        k,
                        chars.len()
                    ),
                    line,
                    col,
                ));
            }
            let mut out = Vec::with_capacity(chars.len() - k + 1);
            for w in chars.windows(k) {
                out.push(Value::Str(Rc::new(w.iter().collect())));
            }
            Ok(Value::Array(Rc::new(out)))
        }
        _ => Err(unknown_method("Dna", name, DNA_METHODS, line, col)),
    }
}

fn complement(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A' => 'T',
            'T' => 'A',
            'C' => 'G',
            'G' => 'C',
            other => other,
        })
        .collect()
}

fn unknown_method(
    type_name: &str,
    name: &str,
    candidates: &[&str],
    line: usize,
    col: usize,
) -> HelixError {
    let mut err = HelixError::new(
        format!("a {} has no method `{}`", type_name, name),
        line,
        col,
    );
    if let Some(s) = suggest(name, candidates) {
        err = err.hint(format!("did you mean `{}`?", s));
    } else {
        err = err.hint(format!(
            "available {} methods: {}",
            type_name,
            candidates.join(", ")
        ));
    }
    err
}

#[cfg(test)]
mod tests;

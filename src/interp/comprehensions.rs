//! Comprehension evaluation for the tree-walker: `map`/`filter`/`where`/`reduce`/
//! `any`/`all` (`eval_comprehension`), plus the element-binder machinery
//! (`eval_with_pattern`/`eval_with_binder`) that binds `it` or a multi-param
//! pattern over each element. An `impl super::Interp` block split from the core.

use super::*;

impl super::Interp {
    pub(super) fn eval_comprehension(
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
}

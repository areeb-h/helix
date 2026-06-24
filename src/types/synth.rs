//! Per-node type synthesis helpers for the `Checker`: `synth_unary`,
//! `synth_binary`, `synth_call`, `synth_method` (+ `synth_array_method`), and the
//! binder-scope helpers (`with_binder`/`with_pattern`/`with_two`). An
//! `impl super::Checker` block split from the main `synth` dispatcher.

use super::*;

impl super::Checker {
    pub(super) fn synth_unary(
        &mut self,
        op: &UnOp,
        expr: &Expr,
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        let t = self.synth(expr)?;
        match op {
            UnOp::Neg => match &t {
                Type::Int | Type::Float | Type::Num | Type::Missing | Type::Unknown => Ok(t),
                Type::Tensor | Type::Array(_) => Ok(Type::Unknown),
                other => Err(HelixError::new(
                    format!("cannot negate a value of type {}", other),
                    line,
                    col,
                )),
            },
            UnOp::Not => match &t {
                Type::Bool | Type::Missing | Type::Unknown => Ok(t),
                Type::Array(_) | Type::Tensor => Ok(Type::Unknown),
                other => Err(HelixError::new(
                    format!("expected a boolean, found a value of type {}", other),
                    line,
                    col,
                )),
            },
        }
    }

    pub(super) fn synth_binary(
        &mut self,
        op: &BinOp,
        lt: &Type,
        rt: &Type,
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        use BinOp::*;
        match op {
            // `a ?? b` — result is whichever side survives; never errors.
            Coalesce => Ok(join(lt, rt)),
            // Equality works on any two operands and never errors.
            Eq | Ne => Ok(Type::Bool),
            And | Or => {
                for t in [lt, rt] {
                    if !matches!(t, Type::Bool | Type::Missing | Type::Unknown) {
                        return Err(HelixError::new(
                            format!("expected a boolean, found a value of type {}", t),
                            line,
                            col,
                        )
                        .hint("Helix has no \"truthiness\" — use an explicit comparison like `x > 0`."));
                    }
                }
                Ok(Type::Bool)
            }
            Lt | Gt | Le | Ge => {
                if matches!(lt, Type::Unknown | Type::Missing)
                    || matches!(rt, Type::Unknown | Type::Missing)
                {
                    return Ok(Type::Bool);
                }
                let both_str = matches!(lt, Type::String) && matches!(rt, Type::String);
                if both_str || (is_numeric(lt) && is_numeric(rt)) {
                    Ok(Type::Bool)
                } else {
                    Err(HelixError::new(
                        format!(
                            "operator `{}` needs numbers, but got a {}",
                            op.symbol(),
                            if is_numeric(lt) { rt } else { lt }
                        ),
                        line,
                        col,
                    ))
                }
            }
            // Arithmetic
            _ => {
                if matches!(lt, Type::Unknown) || matches!(rt, Type::Unknown) {
                    return Ok(Type::Unknown);
                }
                if matches!(lt, Type::Missing) || matches!(rt, Type::Missing) {
                    return Ok(Type::Missing);
                }
                match arith_broadcast(op, lt, rt) {
                    Some(t) => Ok(t),
                    None => Err(HelixError::new(
                        format!(
                            "operator `{}` needs numbers, but got a {}",
                            op.symbol(),
                            if is_numeric(lt) || matches!(lt, Type::Array(_) | Type::Tensor) {
                                rt
                            } else {
                                lt
                            }
                        ),
                        line,
                        col,
                    )),
                }
            }
        }
    }

    pub(super) fn synth_call(
        &mut self,
        name: &str,
        args: &[Type],
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        if crate::registry::lookup(name).is_some() {
            return builtin_type(name, args, line, col);
        }
        // user-defined function?
        if let Some(Type::Function { params, ret }) = self.env.get(name).cloned() {
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
            for (i, (p, a)) in params.iter().zip(args.iter()).enumerate() {
                if !compatible(a, p) {
                    return Err(HelixError::new(
                        format!(
                            "argument {} of `{}` should be {}, found a value of type {}",
                            i + 1,
                            name,
                            p,
                            a
                        ),
                        line,
                        col,
                    ));
                }
            }
            return Ok(*ret);
        }
        if let Some(t) = self.env.get(name) {
            return Err(HelixError::new(
                format!("`{}` is a {}, not a function", name, t),
                line,
                col,
            )
            .hint("only functions and the built-ins can be called."));
        }
        // unknown — suggest from builtins + user functions
        let mut cands: Vec<String> = crate::registry::names().map(|s| s.to_string()).collect();
        cands.extend(
            self.env
                .iter()
                .filter(|(_, t)| matches!(t, Type::Function { .. }))
                .map(|(k, _)| k.clone()),
        );
        let cand_refs: Vec<&str> = cands.iter().map(|s| s.as_str()).collect();
        let mut err = HelixError::new(format!("`{}` is not a known function", name), line, col);
        if let Some(s) = suggest(name, &cand_refs) {
            err = err.hint(format!("did you mean `{}`?", s));
        }
        Err(err)
    }

    pub(super) fn synth_method(
        &mut self,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        let rt = self.synth(recv)?;
        // Record the receiver's type so the bytecode compiler can route this method
        // by the receiver's true type (DataFrame vs Array vs Tensor), not its name.
        self.types.insert(recv as *const Expr, rt.clone());
        // `.is_missing()` is universal.
        if name == "is_missing" {
            return Ok(Type::Bool);
        }
        match &rt {
            // Permissive: any method on Unknown/Missing receiver is Unknown.
            Type::Unknown | Type::Missing => Ok(Type::Unknown),
            // DataFrame / GroupBy: args are the runtime schema boundary — UNCHECKED.
            Type::DataFrame => df_method_type(name, line, col),
            Type::GroupBy => groupby_method_type(name, line, col),
            Type::Array(el) => self.synth_array_method(el, name, args, line, col),
            Type::Tensor => {
                self.synth_simple_args(args)?;
                tensor_method_type(name, args.len(), line, col)
            }
            Type::String => {
                self.synth_simple_args(args)?;
                string_method_type(name, line, col)
            }
            Type::Dna => {
                self.synth_simple_args(args)?;
                dna_method_type(name, line, col)
            }
            other => Err(HelixError::new(
                format!("type {} has no method `{}`", other, name),
                line,
                col,
            )),
        }
    }

    /// Evaluate argument types for type-checking side effects (so e.g.
    /// `xs.take(undefined)` reports the undefined name).
    fn synth_simple_args(&mut self, args: &[Expr]) -> Result<(), HelixError> {
        for a in args {
            self.synth(a)?;
        }
        Ok(())
    }

    fn synth_array_method(
        &mut self,
        el: &Type,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        // Comprehension methods take UNEVALUATED bodies with `it` / lambda binders.
        match name {
            "map" | "filter" | "where" | "any" | "all" => {
                let (params, body) = comprehension_params(args);
                let body_t = self.with_pattern(&params, el.clone(), body)?;
                match name {
                    "map" => Ok(Type::Array(Box::new(body_t))),
                    "filter" | "where" => {
                        require_boolish(&body_t, name, line, col)?;
                        Ok(Type::Array(Box::new(el.clone())))
                    }
                    _ => {
                        require_boolish(&body_t, name, line, col)?;
                        Ok(Type::Bool)
                    }
                }
            }
            "reduce" => {
                if args.len() != 2 {
                    return Ok(Type::Unknown); // malformed → runtime errors; don't false-positive
                }
                let init_t = self.synth(&args[0])?;
                if let Expr::Lambda { params, body } = &args[1]
                    && params.len() == 2 {
                        let body_t = self.with_two(
                            &params[0],
                            init_t.clone(),
                            &params[1],
                            el.clone(),
                            body,
                        )?;
                        return Ok(join(&init_t, &body_t));
                    }
                Ok(Type::Unknown)
            }
            _ => {
                self.synth_simple_args(args)?;
                array_method_type(name, el, line, col)
            }
        }
    }

    /// Synth `body` with `name` bound to `t`, restoring afterward (nested
    /// comprehensions restore correctly).
    fn with_binder(&mut self, name: &str, t: Type, body: &Expr) -> Result<Type, HelixError> {
        let saved = self.env.insert(name.to_string(), t);
        let result = self.synth(body);
        match saved {
            Some(old) => {
                self.env.insert(name.to_string(), old);
            }
            None => {
                self.env.remove(name);
            }
        }
        result
    }

    /// Bind one element type to one name, or destructure it across several
    /// names (`(a, b) => ...`), then synthesize the body type.
    fn with_pattern(&mut self, names: &[String], el: Type, body: &Expr) -> Result<Type, HelixError> {
        if names.len() == 1 {
            return self.with_binder(&names[0], el, body);
        }
        // element types for each destructured name
        let types: Vec<Type> = match &el {
            Type::Tuple(ts) if ts.len() == names.len() => ts.clone(),
            Type::Array(inner) => vec![(**inner).clone(); names.len()],
            _ => vec![Type::Unknown; names.len()], // mismatch/Unknown -> permissive
        };
        let saved: Vec<(String, Option<Type>)> = names
            .iter()
            .map(|n| (n.clone(), self.env.get(n).cloned()))
            .collect();
        for (n, t) in names.iter().zip(types) {
            self.env.insert(n.clone(), t);
        }
        let result = self.synth(body);
        for (n, old) in saved {
            match old {
                Some(t) => {
                    self.env.insert(n, t);
                }
                None => {
                    self.env.remove(&n);
                }
            }
        }
        result
    }

    fn with_two(
        &mut self,
        n1: &str,
        t1: Type,
        n2: &str,
        t2: Type,
        body: &Expr,
    ) -> Result<Type, HelixError> {
        let s1 = self.env.insert(n1.to_string(), t1);
        let s2 = self.env.insert(n2.to_string(), t2);
        let result = self.synth(body);
        match s2 {
            Some(old) => {
                self.env.insert(n2.to_string(), old);
            }
            None => {
                self.env.remove(n2);
            }
        }
        match s1 {
            Some(old) => {
                self.env.insert(n1.to_string(), old);
            }
            None => {
                self.env.remove(n1);
            }
        }
        result
    }
}

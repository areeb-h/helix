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
                let both_dna = matches!(lt, Type::Dna) && matches!(rt, Type::Dna);
                // Tuples order lexicographically (element pairs are checked at
                // runtime — an unorderable PAIR errors there like a scalar would).
                let both_tuple = matches!(lt, Type::Tuple(_)) && matches!(rt, Type::Tuple(_));
                if both_str || both_dna || both_tuple || (is_numeric(lt) && is_numeric(rt)) {
                    Ok(Type::Bool)
                } else if matches!(lt, Type::String | Type::Dna)
                    || matches!(rt, Type::String | Type::Dna)
                {
                    // Same wording as the runtime (`ops::compare`) for the
                    // mixed-orderable case, so static and dynamic rejections of
                    // one program read identically.
                    Err(HelixError::new(
                        format!(
                            "cannot order {} and {} — `{}` compares two numbers, two strings, two DNA sequences, or two tuples of those",
                            lt,
                            rt,
                            op.symbol()
                        ),
                        line,
                        col,
                    ))
                } else {
                    // The truthful list — a "needs numbers" wording here predated
                    // string/DNA/tuple ordering and misdescribed the operator.
                    let bad = if is_numeric(lt) { rt } else { lt };
                    Err(HelixError::new(
                        format!(
                            "`{}` cannot order {} — it compares two numbers, two strings, two DNA sequences, or two tuples of those",
                            op.symbol(),
                            crate::value::with_article(&bad.to_string()),
                        ),
                        line,
                        col,
                    ))
                }
            }
            // Integer bitwise — both operands must be integers; the result is `Int`.
            BitAnd | BitOr | BitXor | Shl | Shr => {
                if matches!(lt, Type::Unknown) || matches!(rt, Type::Unknown) {
                    return Ok(Type::Unknown);
                }
                if matches!(lt, Type::Missing) || matches!(rt, Type::Missing) {
                    return Ok(Type::Missing);
                }
                let ok = |t: &Type| matches!(t, Type::Int | Type::Num);
                if ok(lt) && ok(rt) {
                    Ok(Type::Int)
                } else {
                    Err(HelixError::new(
                        format!(
                            "bitwise operator `{}` needs integers, but got {}",
                            op.symbol(),
                            crate::value::with_article(&if ok(lt) { rt } else { lt }.to_string())
                        ),
                        line,
                        col,
                    )
                    .hint("`&` `|` `^` `<<` `>>` work on integers only."))
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
                    None => {
                        let err = HelixError::new(
                            format!(
                                "operator `{}` needs numbers, but got {}",
                                op.symbol(),
                                crate::value::with_article(
                                    &if is_numeric(lt)
                                        || matches!(lt, Type::Array(_) | Type::Tensor)
                                    {
                                        rt
                                    } else {
                                        lt
                                    }
                                    .to_string()
                                )
                            ),
                            line,
                            col,
                        );
                        // The common reflex: reaching for `+` to join strings.
                        let strings = matches!(lt, Type::String) || matches!(rt, Type::String);
                        Err(if matches!(op, Add) && strings {
                            err.hint("strings don't join with `+` — use interpolation `\"{a}{b}\"`, or `xs.join(sep)` for a list of strings.")
                        } else {
                            err
                        })
                    }
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
        // A user binding of this name shadows a builtin of the same name — defining
        // `fn sign(..)` checks against *your* signature, not the math builtin's.
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
            // A name bound to `Unknown` — e.g. a function-valued parameter, which the
            // gradual checker can't assign a concrete arity/signature — is callable.
            // Permit it and yield `Unknown` (higher-order functions: `fn apply(f, x)
            // = f(x)`). Only a *known* non-function type is a hard error.
            if matches!(t, Type::Unknown) {
                return Ok(Type::Unknown);
            }
            return Err(HelixError::new(
                format!("`{}` is a {}, not a function", name, t),
                line,
                col,
            )
            .hint("only functions and the built-ins can be called."));
        }
        // No user binding → the builtin.
        if crate::registry::lookup(name).is_some() {
            return builtin_type(name, args, line, col);
        }
        // Unknown. The builtins are always in the suggester's universe, so only the
        // user's own functions need collecting here.
        let cands: Vec<&str> = self
            .env
            .iter()
            .filter(|(_, t)| matches!(t, Type::Function { .. }))
            .map(|(k, _)| k.as_str())
            .collect();
        let err = HelixError::new(format!("`{}` is not a known function", name), line, col);
        Err(match crate::suggest::hint(name, crate::suggest::Site::Function, &cands) {
            Some(h) => err.hint(h),
            None => err,
        })
    }

    pub(super) fn synth_method(
        &mut self,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        // Pre-ADR-0017 namespaced call (`stats.t_test(a, b)`) parses as a method on a
        // bare identifier. If that identifier is an unbound *removed namespace* (not a
        // local that shadows it), point the user at the new spelling before the receiver
        // resolves to a bare "not defined".
        if let Expr::Ident { name: ns, .. } = recv
            && !self.env.contains_key(ns.as_str())
            && let Some(hint) = crate::namespace::migration_hint(ns, name)
        {
            return Err(
                HelixError::new(format!("`{ns}.{name}` is no longer available"), line, col)
                    .hint(hint),
            );
        }
        let rt = self.synth(recv)?;
        // Record the receiver's type so the bytecode compiler can route this method
        // by the receiver's true type (DataFrame vs Array vs Tensor), not its name.
        self.types.insert(recv as *const Expr, rt.clone());
        // `.is_missing()` is universal — and takes no arguments (matching the
        // runtime), so the checker agrees rather than waving the arity through.
        if name == "is_missing" {
            if !args.is_empty() {
                return Err(HelixError::new(
                    "`is_missing` takes no arguments",
                    line,
                    col,
                ));
            }
            return Ok(Type::Bool);
        }
        // `.to_json()` is universal too — serializes any receiver to a String.
        if name == "to_json" {
            if !args.is_empty() {
                return Err(HelixError::new("`to_json` takes no arguments", line, col));
            }
            return Ok(Type::String);
        }
        // `$arg_extreme` — the internal packed kernel `argmin`/`argmax` desugar to. It is
        // unwritable from source, so no user program can reach this arm; the checker sees it
        // only because it is the left operand of the desugar's `??`.
        //
        // IT MUST NOT INSPECT THE RECEIVER. `"abc".argmax()` and `tensor(…).argmax()` are
        // COMPILE errors today ("type String has no method `enumerate`", with the full
        // method-list hint), and they come from the right-hand side of that `??`. Typing this
        // side by receiver would either reject them here with a different message or, worse,
        // stop the checker before it reaches the arm that produces the real one.
        //
        // `Type::Int` for a value that can be `missing` follows `index_of`, which does the
        // same thing for the same reason.
        if name == "$arg_extreme" {
            return Ok(Type::Int);
        }
        let result = match &rt {
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
            // Records get dynamic-access methods (`get`/`has`/`keys`/…) on top of static
            // `rec.field` access — the escape hatch for runtime-unknown shapes (parsed JSON).
            Type::Record(fields) => {
                let fields = fields.clone();
                self.synth_simple_args(args)?;
                record_method_type(name, &fields, line, col)
            }
            // A scalar receiver (Int/Float/Bool/…) — no method table at all. This is
            // where `(-1).abs()` lands, so cross the namespace and say that `abs` is
            // a function rather than leaving the user with a bare rejection.
            other => {
                let err = HelixError::new(
                    format!("type {} has no method `{}`", other, name),
                    line,
                    col,
                );
                Err(match crate::suggest::hint(name, crate::suggest::Site::Method, &[]) {
                    Some(h) => err.hint(h),
                    None => err,
                })
            }
        };
        // UFCS, half two, mirrored so the checker cannot reject what the engines now
        // run: a builtin-named method that no table claims types as the builtin with
        // the receiver prepended. Only on the ERROR path — a method that owns its
        // name keeps its type — and never for the permissive receivers (Unknown /
        // Missing / DataFrame / GroupBy), matching `ufcs_fallback_applies` at run
        // time: Unknown covers PyObject and Node, which are opaque to the checker.
        if result.is_err()
            && crate::registry::is_builtin_name(name)
            && !matches!(rt, Type::Unknown | Type::Missing | Type::DataFrame | Type::GroupBy)
            && !crate::registry::type_owns_method(&Self::receiver_table_name(&rt), name)
        {
            let mut bts = Vec::with_capacity(args.len() + 1);
            bts.push(rt.clone());
            for a in args {
                bts.push(self.synth(a)?);
            }
            // The builtin's own answer — including its own ERROR, which names the
            // real mismatch ("`sqrt` needs a number…") where the method error could
            // only say "no method".
            return builtin_type(name, &bts, line, col);
        }
        result
    }

    /// The registry-table name for a checker type, for `type_owns_method`. Types the
    /// registry does not table (scalars, tuples) return a name it maps to nothing, so
    /// the ownership test answers `false` and the fallback proceeds — which is right:
    /// a scalar has no methods a table could claim.
    fn receiver_table_name(rt: &Type) -> String {
        match rt {
            Type::Array(_) => "Array".to_string(),
            Type::String => "String".to_string(),
            Type::Dna => "Dna".to_string(),
            Type::Tensor => "Tensor".to_string(),
            Type::Record(_) => "Record".to_string(),
            other => other.to_string(),
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
            // `position` binds `it` like the others, but deliberately does NOT
            // `require_boolish` its body: it answers "the first index whose result is
            // exactly `Bool(want)`", so a non-boolean body is not an error, it simply
            // never matches. `[5, 6, 7].position(it)` is `missing`, and was `missing`
            // back when this desugared to `map(p).index_of(true)`.
            "position" => {
                let (params, body) = comprehension_params(&args[0..1]);
                self.with_pattern(&params, el.clone(), body)?;
                Ok(Type::Int)
            }
            "reduce" | "scan" => {
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
                        // `reduce` yields the final accumulator; `scan` an array of every
                        // intermediate accumulator.
                        let acc = join(&init_t, &body_t);
                        return Ok(if name == "scan" { Type::Array(Box::new(acc)) } else { acc });
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

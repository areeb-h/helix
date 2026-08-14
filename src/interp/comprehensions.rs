//! Comprehension evaluation for the tree-walker: `map`/`filter`/`where`/`reduce`/
//! `any`/`all` (`eval_comprehension`), plus the element-binder machinery
//! (`eval_with_pattern`/`eval_with_binder`) that binds `it` or a multi-param
//! pattern over each element. An `impl super::Interp` block split from the core.

use super::*;

/// Validate a comprehension's SHAPE — its arity, and that its function names a binder —
/// without evaluating a single argument.
///
/// These rules are structural, so the answer cannot depend on the receiver. That is
/// exactly how the VM and JIT treat them: both decide when they COMPILE the comprehension,
/// and so reject a malformed call whatever the receiver turns out to be at run time.
///
/// The walker reaches the same rules per-arm, AFTER matching the receiver, so a `missing`
/// receiver used to return early and silence the mistake. `missing.map()` was `missing`
/// here while `[1, 2].map()` was an error — the same malformed call, an error for one
/// receiver and a success for another, inconsistent with this engine's own behaviour. Via
/// `try` it laundered into an ordinary boolean where no error text could reveal it:
/// `(try missing.map()).ok` was `true` on the walker and `false` on the other two, and the
/// walker is the oracle's designated reference.
///
/// The init expression must NOT be evaluated on this path. All three engines agree that
/// `missing.reduce(1 / 0, (a, b) => a)` is `missing` while `[].reduce(1 / 0, (a, b) => a)`
/// divides by zero, so validating by running the comprehension against an empty array —
/// which is otherwise an appealing way to avoid restating anything — would introduce a new
/// divergence in place of the one being fixed.
///
/// This restates the arms' rules rather than being called by them: several arms need the
/// values their checks produce (`reduce` its two binder names, `position` its `want` flag)
/// and would have to re-derive them anyway. Two spellings of one rule is the defect shape
/// this codebase keeps finding, so the duplication is pinned by a test asserting that a
/// `missing` receiver and an array receiver produce BYTE-IDENTICAL errors for every
/// malformed shape — the same "compare a program against its own equivalent spelling"
/// method that surfaced the divergence.
fn comp_shape_check(
    name: &str,
    args: &[Expr],
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    let one = |example: &str, hint: &str| -> Result<(), HelixError> {
        if args.len() != 1 {
            return Err(comp_arity(name, example, line, col));
        }
        let (params, _) = comprehension_params(&args[0]);
        comp_needs_binder(&params, name, hint, line, col)
    };
    match name {
        "map" => one(
            "(it * 2)",
            "e.g. `xs.map(it * 2)` or `xs.map((a, b) => ...)`.",
        ),
        "filter" | "where" => one(
            "(it > 0)",
            "e.g. `xs.map(it * 2)` or `xs.map((a, b) => ...)`.",
        ),
        "any" | "all" => one(
            "(it > 0)",
            "e.g. `xs.any(it > 0)` or `xs.all((a, b) => a < b)`.",
        ),
        "position" => {
            if args.is_empty() || args.len() > 2 {
                return Err(comp_arity(name, "(it > 0)", line, col));
            }
            // The second argument is the desugarer's internal `want` flag; anything else
            // there is unreachable from source, exactly as in the evaluating arm.
            if !matches!(args.get(1), None | Some(Expr::Bool(_))) {
                return Err(comp_arity(name, "(it > 0)", line, col));
            }
            let (params, _) = comprehension_params(&args[0]);
            comp_needs_binder(&params, name, "e.g. `xs.position(it > 0)`.", line, col)
        }
        "reduce" | "scan" => {
            if args.len() != 2 {
                return Err(HelixError::new(
                    format!("`{name}` takes a starting value and an accumulator function"),
                    line,
                    col,
                )
                .hint("e.g. `xs.reduce(0, (acc, x) => acc + x)` to sum."));
            }
            match &args[1] {
                Expr::Lambda { params, .. } if params.len() == 2 => Ok(()),
                Expr::Lambda { params, .. } => Err(HelixError::new(
                    format!(
                        "`{name}`'s function needs exactly two parameters, but got {}",
                        params.len()
                    ),
                    line,
                    col,
                )
                .hint("the first is the running accumulator, e.g. `(acc, x) => acc + x`.")),
                _ => Err(HelixError::new(
                    format!("`{name}` needs an explicit accumulator function"),
                    line,
                    col,
                )
                .hint("name both binders: `xs.reduce(0, (acc, x) => acc + x)`.")),
            }
        }
        // Not a comprehension: nothing structural to say about it.
        _ => Ok(()),
    }
}

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
            // `missing.map(...)` etc. propagate rather than erroring (ADR 0001) — but only
            // when the CALL ITSELF IS WELL FORMED. A malformed one is a mistake in the
            // program, not a condition in the data, so it is reported whatever the
            // receiver holds; see `comp_shape_check`. Nothing here is evaluated, so
            // `missing.reduce(1 / 0, ...)` stays `missing` as all three engines agree.
            Value::Missing => {
                comp_shape_check(name, args, line, col)?;
                return Ok(Value::Missing);
            }
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
                comp_needs_binder(&params, name, "e.g. `xs.map(it * 2)` or `xs.map((a, b) => ...)`.", line, col)?;
                // `ColumnBuilder`, not a bare `Vec<Value>`, for the same reason the VM uses
                // one: it packs `Int`/`Float` results into 8 bytes each instead of 24-byte
                // boxed slots, and it is the single place the materialization limit is
                // enforced — so all three engines refuse the same program with the same
                // words (ADR 0024), rather than the walker alone aborting the process.
                let mut out = crate::value::ColumnBuilder::default();
                self.eval_pattern_loop(&params, &items, body, line, col, |_el, v| {
                    out.push(v)
                        .map_err(|lim| crate::vm::materialize_refused(lim, line, col))?;
                    Ok(None)
                })?;
                Ok(out.finish())
            }
            "filter" | "where" => {
                if args.len() != 1 {
                    return Err(comp_arity(name, "(it > 0)", line, col));
                }
                let (params, body) = comprehension_params(&args[0]);
                comp_needs_binder(&params, name, "e.g. `xs.map(it * 2)` or `xs.map((a, b) => ...)`.", line, col)?;
                let mut out = crate::value::ColumnBuilder::default();
                self.eval_pattern_loop(&params, &items, body, line, col, |el, keep| match keep {
                    Value::Bool(true) => {
                        out.push(el.clone())
                            .map_err(|lim| crate::vm::materialize_refused(lim, line, col))?;
                        Ok(None)
                    }
                    Value::Bool(false) => Ok(None),
                    other => Err(HelixError::new(
                        format!(
                            "`{}` expects a yes/no test, but the expression produced {}",
                            name,
                            crate::value::with_article(other.type_name())
                        ),
                        line,
                        col,
                    )
                    .hint("write a comparison, e.g. `xs.filter(it > 50)`.")),
                })?;
                Ok(out.finish())
            }
            // First index whose predicate result is EXACTLY `Bool(want)` (`want` is
            // `true` from source; `false` only from the `take_while`/`drop_while`
            // desugar), or `missing` if no element matches. Short-circuits.
            //
            // This was `map(p).index_of(Bool(want))`, and the arms below reproduce that
            // comparison exactly rather than approximately: `values_equal` is `false` for
            // every non-`Bool` against a `Bool`, so a `missing` result — and an outright
            // non-boolean one — is SKIPPED, not an error and not a match.
            // `[5, 6, 7].position(it)` is `missing`, not a type error, and stays so.
            // That is deliberately unlike `any`/`all`, which do reject a non-boolean test.
            "position" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(comp_arity(name, "(it > 0)", line, col));
                }
                let want = match args.get(1) {
                    None => true,
                    Some(Expr::Bool(b)) => *b,
                    // Unreachable from source: `desugar_position` rejects two arguments.
                    Some(_) => return Err(comp_arity(name, "(it > 0)", line, col)),
                };
                let (params, body) = comprehension_params(&args[0]);
                comp_needs_binder(&params, name, "e.g. `xs.position(it > 0)`.", line, col)?;
                let mut i: i64 = 0;
                let found = self.eval_pattern_loop(&params, &items, body, line, col, |_el, r| {
                    let hit = matches!(r, Value::Bool(b) if b == want);
                    i += 1;
                    Ok(hit.then(|| Value::Int(i - 1)))
                })?;
                Ok(found.unwrap_or(Value::Missing))
            }
            "any" | "all" => {
                if args.len() != 1 {
                    return Err(comp_arity(name, "(it > 0)", line, col));
                }
                let (params, body) = comprehension_params(&args[0]);
                comp_needs_binder(&params, name, "e.g. `xs.any(it > 0)` or `xs.all((a, b) => a < b)`.", line, col)?;
                let is_all = name == "all";
                let mut seen_missing = false;
                // `visit` short-circuits with Some(bool) the instant the answer is known.
                let short = self.eval_pattern_loop(&params, &items, body, line, col, |_el, r| {
                    match r {
                        Value::Bool(b) => {
                            if !is_all && b {
                                Ok(Some(Value::Bool(true)))
                            } else if is_all && !b {
                                Ok(Some(Value::Bool(false)))
                            } else {
                                Ok(None)
                            }
                        }
                        Value::Missing => {
                            seen_missing = true;
                            Ok(None)
                        }
                        other => Err(HelixError::new(
                            format!(
                                "`{}` expects a yes/no test, but the expression produced {}",
                                name,
                                crate::value::with_article(other.type_name())
                            ),
                            line,
                            col,
                        )
                        .hint("write a comparison, e.g. `xs.any(it > 0)`.")),
                    }
                })?;
                // No short-circuit: `all` is true unless something was false, `any` false
                // unless something was true; a `missing` in the deciding spot → missing.
                Ok(match short {
                    Some(v) => v,
                    None if seen_missing => Value::Missing,
                    None => Value::Bool(is_all),
                })
            }
            "reduce" | "scan" => {
                if args.len() != 2 {
                    return Err(HelixError::new(
                        format!("`{name}` takes a starting value and an accumulator function"),
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
                                "`{name}`'s function needs exactly two parameters, but got {}",
                                params.len()
                            ),
                            line,
                            col,
                        )
                        .hint("the first is the running accumulator, e.g. `(acc, x) => acc + x`."))
                    }
                    _ => {
                        return Err(HelixError::new(
                            format!("`{name}` needs an explicit accumulator function"),
                            line,
                            col,
                        )
                        .hint("name both binders: `xs.reduce(0, (acc, x) => acc + x)`."))
                    }
                };
                let mut acc = self.eval(&args[0])?; // init: evaluated in the OUTER scope
                // Bind the accumulator and element names ONCE for the whole fold, then
                // rewrite just their `.value` each step — instead of a remove/insert plus
                // a fresh `String` key per element (the old per-call `eval_with_two`).
                // Correctness mirrors that helper exactly: the init above is evaluated
                // before the binders exist; both binders are restored on *every* exit,
                // including an error mid-fold; and `pa` (acc) is written before `pb`
                // (element) so last-write-wins holds if a user names both binders the same.
                let saved_a = self.env.remove(pa);
                let saved_b = self.env.remove(pb);
                self.env
                    .insert(pa.to_string(), Binding { value: Value::Unit, mutable: false });
                self.env
                    .insert(pb.to_string(), Binding { value: Value::Unit, mutable: false });
                // `reduce` returns the final accumulator; `scan` returns the array of every
                // intermediate accumulator (one per element — a generalized `cumsum`).
                let want_scan = name == "scan";
                let mut out: Vec<Value> = if want_scan {
                    Vec::with_capacity(items.len())
                } else {
                    Vec::new()
                };
                let mut err: Option<HelixError> = None;
                for el in items.to_values().iter() {
                    self.env.get_mut(pa).unwrap().value = acc;
                    self.env.get_mut(pb).unwrap().value = el.clone();
                    match self.eval(body) {
                        Ok(v) => {
                            acc = v;
                            if want_scan {
                                out.push(acc.clone());
                            }
                        }
                        Err(e) => {
                            acc = Value::Unit; // moved out above; keep it initialized
                            err = Some(e);
                            break;
                        }
                    }
                }
                // Restore the shadowed bindings (or clear ours) — always, even on error.
                self.env.remove(pa);
                if let Some(b) = saved_a {
                    self.env.insert(pa.to_string(), b);
                }
                // Only touch `pb` when it is a DISTINCT name. When a user names both
                // binders the same (`(a, a)` — explicitly legal, last-write-wins), `pa`
                // and `pb` are one environment entry: `saved_b` was already `None`
                // (removed by the `pa` line), and an unconditional `remove(pb)` here
                // would delete the outer binding we JUST restored from `saved_a`,
                // leaving the name undefined after the fold (the VM keeps it). Skip it.
                if pb != pa {
                    self.env.remove(pb);
                    if let Some(b) = saved_b {
                        self.env.insert(pb.to_string(), b);
                    }
                }
                match err {
                    Some(e) => Err(e),
                    None if want_scan => Ok(Value::array(out)),
                    None => Ok(acc),
                }
            }
            _ => unreachable!(),
        }
    }

    /// Run `body` once per element of `items`, binding the comprehension pattern
    /// `names` (a single `it`/`x`, or several for a destructuring `(a, b) => …`) to
    /// each element. The binder slot(s) are installed **once** and only their `.value`
    /// is rewritten each step — the allocation-free replacement for a per-element
    /// bind/restore (no fresh `String` key or remove/insert churn per element). `visit`
    /// receives `(element, body_result)` and returns `Ok(Some(v))` to short-circuit the
    /// whole loop with `v` (for `any`/`all`) or `Ok(None)` to continue. The shadowed
    /// binding(s) are restored on **every** exit — normal end, short-circuit, or error —
    /// so nested comprehensions and the surrounding scope behave exactly as before.
    fn eval_pattern_loop<F>(
        &mut self,
        names: &[String],
        items: &crate::value::ArrayData,
        body: &Expr,
        line: usize,
        col: usize,
        mut visit: F,
    ) -> Result<Option<Value>, HelixError>
    where
        F: FnMut(&Value, Value) -> Result<Option<Value>, HelixError>,
    {
        let saved: Vec<Option<Binding>> = names.iter().map(|n| self.env.remove(n)).collect();
        for n in names.iter() {
            self.env
                .insert(n.to_string(), Binding { value: Value::Unit, mutable: false });
        }
        let mut outcome: Result<Option<Value>, HelixError> = Ok(None);
        // `iter_values()`, NOT `to_values()`. On a packed array the latter expands every
        // element into a boxed `Value` up front purely to iterate it: for a 100M-element
        // `Int` array that is a 1.6 GB `Vec` allocated before the first element is even
        // examined, and `handle_alloc_error` ABORTS the process when it cannot be had —
        // the tree-walker's half of the ADR 0024 violation, and the reason a guard on the
        // comprehension's *output* could never fix the walker. Iterating boxes one element
        // at a time and allocates nothing.
        for el in items.iter_values() {
            // Rewrite the binder value(s) for this element: a single binder takes it
            // directly; several destructure it (identical to the old per-element path).
            let bind_err = if names.len() == 1 {
                self.env.get_mut(&names[0]).unwrap().value = el.clone();
                None
            } else {
                match pattern_parts(&el, names.len(), line, col) {
                    Ok(parts) => {
                        for (n, v) in names.iter().zip(parts) {
                            self.env.get_mut(n).unwrap().value = v;
                        }
                        None
                    }
                    Err(e) => Some(e),
                }
            };
            if let Some(e) = bind_err {
                outcome = Err(e);
                break;
            }
            match self.eval(body) {
                Ok(v) => match visit(&el, v) {
                    Ok(None) => {}
                    other => {
                        outcome = other;
                        break;
                    }
                },
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
        }
        // Restore each shadowed binding (or clear ours), even on early exit / error.
        for (n, b) in names.iter().zip(saved) {
            self.env.remove(n);
            if let Some(b) = b {
                self.env.insert(n.to_string(), b);
            }
        }
        outcome
    }
}

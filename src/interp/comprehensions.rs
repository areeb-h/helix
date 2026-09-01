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

/// What a comprehension says about a receiver it cannot iterate.
///
/// Shared with the VM's `Op::DfColumnVerb`, which routes `where`/`filter` by type and can
/// only discover at run time that the receiver is not a frame either — the walker reaches
/// this same sentence for that program, and one definition is how they stay the same
/// sentence.
pub(crate) fn not_an_array(recv: &Value, name: &str, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!("type {} has no method `{}`", recv.type_name(), name),
        line,
        col,
    )
    .hint("`map`, `filter`, `where`, and `reduce` work on arrays.")
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
            other => return Err(not_an_array(other, name, line, col)),
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
                // THE TAKE-APPEND-STORE FAST PATH (ADR 0029, plan #1) — the VM's
                // discipline transplanted, not a parallel invention. Recognized ONCE:
                // the body is EXACTLY `acc.concat(e)` / `acc.insert(k, v)` on the
                // fold's own accumulator binder — the compiler's guarded
                // `emit_reduce_body_and_store` predicate, including `pa != pb` (with
                // duplicate binders the name means the ELEMENT) and never for `scan`
                // (each snapshot in `out` co-owns the accumulator, so in-place
                // extension would corrupt history; the Rc check would decline anyway —
                // excluding it here keeps that structural rather than incidental).
                // Everything hard is inherited: arguments evaluate FIRST with the
                // binding intact (`acc.concat([acc.count()])` sees the live value and
                // merely keeps the Rc shared), validation precedes the take with the
                // VM op's exact error texts, and the `Rc::get_mut` inside
                // `concat_in_place`/`insert_in_place` is the aliasing oracle — shared
                // inits, captured accumulators and self-referencing arguments all fall
                // to the copy path with identical answers (ADR 0029: correct at the
                // old cost, never wrong at any cost).
                enum FoldFast<'a> {
                    Concat(&'a [Expr]),
                    Insert(&'a [Expr]),
                    /// The string fold `"{acc}…tail…"` — same admission as the
                    /// compiler's `AppendStrIntoLocal` arm: parts[0] the bare
                    /// accumulator hole with no format spec, and no later hole
                    /// mentioning the accumulator.
                    AppendStr(&'a [crate::ast::InterpPart]),
                }
                let fold_step: Option<FoldFast> = if want_scan || pa == pb {
                    None
                } else if let Expr::Method { recv, name: vn, args: va, named, .. } = body
                    && named.is_empty()
                    && matches!(&**recv, Expr::Ident { name: n, .. } if n == pa)
                    && matches!((vn.as_str(), va.len()), ("concat", 1) | ("insert", 2))
                {
                    Some(if vn == "concat" {
                        FoldFast::Concat(va.as_slice())
                    } else {
                        FoldFast::Insert(va.as_slice())
                    })
                } else if let Expr::Interp(parts) = body
                    && matches!(
                        parts.first(),
                        Some(crate::ast::InterpPart::Expr(e, None))
                            if matches!(&**e, Expr::Ident { name: n, .. } if n == pa)
                    )
                    && !parts[1..].iter().any(|p| match p {
                        crate::ast::InterpPart::Expr(e, _) => crate::jit::expr_uses_ident(e, pa),
                        crate::ast::InterpPart::Lit(_) => false,
                    })
                {
                    Some(FoldFast::AppendStr(parts.as_slice()))
                } else {
                    None
                };
                let mut err: Option<HelixError> = None;
                // `iter_values()`, not `to_values()`: the latter boxes a packed
                // receiver into a `Vec<Value>` up front purely to iterate it — the
                // same ADR-0024 hazard `eval_pattern_loop` already fixed.
                for el in items.iter_values() {
                    self.env.get_mut(pa).unwrap().value = acc;
                    self.env.get_mut(pb).unwrap().value = el;
                    if let Some(step) = &fold_step {
                        let r = match step {
                            FoldFast::Concat(va) => self.fold_take_append(true, va, pa, line, col),
                            FoldFast::Insert(va) => self.fold_take_append(false, va, pa, line, col),
                            FoldFast::AppendStr(parts) => {
                                self.fold_append_str(parts, pa, line, col)
                            }
                        };
                        match r {
                            Ok(v) => acc = v,
                            Err(e) => {
                                acc = Value::Unit;
                                err = Some(e);
                                break;
                            }
                        }
                        continue;
                    }
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
    /// One element of the fold fast path: evaluate the argument(s) with the accumulator
    /// binding INTACT, validate everything fallible with the VM op's exact error texts
    /// (the oracle counts words), and only then take the binding's value —
    /// `mem::replace` drops the env's ownership, so an accumulator nothing else aliases
    /// becomes unique and `concat_in_place`/`insert_in_place` extend it in place.
    /// Mirrors `Op::ConcatIntoLocal`/`Op::InsertIntoLocal` (vm.rs) arm for arm,
    /// including the validation ORDER — a failing step leaves the binding exactly as
    /// the general path would have, which is what the error-mid-fold restore contract
    /// (init variable intact, shadowed outer binding restored) depends on.
    fn fold_take_append(
        &mut self,
        is_concat: bool,
        va: &[Expr],
        pa: &str,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        use crate::value::with_article;
        if is_concat {
            let arg = self.eval(&va[0])?;
            let Value::Array(add) = arg else {
                return Err(HelixError::new(
                    format!(
                        "`concat` expects arrays, but argument 1 is {}",
                        with_article(arg.type_name())
                    ),
                    line,
                    col,
                ));
            };
            let slot = &mut self.env.get_mut(pa).unwrap().value;
            if !matches!(slot, Value::Array(_)) {
                return Err(HelixError::new(
                    format!("{} has no method `concat`", with_article(slot.type_name())),
                    line,
                    col,
                ));
            }
            let Value::Array(cur) = std::mem::replace(slot, Value::Unit) else {
                unreachable!("accumulator type checked immediately above")
            };
            Ok(Value::concat_in_place(cur, &add))
        } else {
            let kv = self.eval(&va[0])?;
            let v = self.eval(&va[1])?;
            let slot = &mut self.env.get_mut(pa).unwrap().value;
            if !matches!(slot, Value::Dict(_)) {
                return Err(HelixError::new(
                    format!("{} has no method `insert`", with_article(slot.type_name())),
                    line,
                    col,
                ));
            }
            let k = crate::value::DictKey::from_value(&kv)
                .map_err(|m| HelixError::new(m, line, col))?;
            let Value::Dict(cur) = std::mem::replace(slot, Value::Unit) else {
                unreachable!("accumulator type checked immediately above")
            };
            Ok(Value::insert_in_place(cur, k, v))
        }
    }

    /// One element of the STRING fold fast path — the walker twin of
    /// `Op::AppendStrIntoLocal`, same order of operations: render the tail first (all
    /// fallible work with the binding untouched), the cap crossing with the
    /// interpolation's exact wording, the non-`Str` fresh-build fallback (the general
    /// path FORMATS a `0` init, it does not error), and only then the take — a unique
    /// `Rc` extends in place with fallible growth, a shared one pays one content copy.
    fn fold_append_str(
        &mut self,
        parts: &[crate::ast::InterpPart],
        pa: &str,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        use crate::ast::InterpPart;
        let mut tail = String::new();
        for part in &parts[1..] {
            match part {
                InterpPart::Lit(t) => tail.push_str(t),
                InterpPart::Expr(e, spec) => {
                    let v = self.eval(e)?;
                    let (el, ec) = e.position();
                    match spec {
                        Some(fs) => tail.push_str(
                            &fs.apply(&v).map_err(|m| HelixError::new(m, el, ec))?,
                        ),
                        None => crate::value::write_value(&mut tail, &v, el, ec)?,
                    }
                }
            }
        }
        let too_long = || {
            HelixError::new(
                format!("interpolated string exceeds {} bytes", crate::interp::MAX_STRING_LEN),
                line,
                col,
            )
            .hint("build large text incrementally or write it to a file instead.")
        };
        let slot = &mut self.env.get_mut(pa).unwrap().value;
        if matches!(&*slot, Value::Str(_)) {
            let Value::Str(cur) = &*slot else { unreachable!() };
            if cur.len() + tail.len() > crate::interp::MAX_STRING_LEN {
                return Err(too_long());
            }
            let Value::Str(cur) = std::mem::replace(slot, Value::Unit) else {
                unreachable!("accumulator type checked immediately above")
            };
            let new = match std::rc::Rc::try_unwrap(cur) {
                Ok(mut s) => {
                    if s.try_reserve(tail.len()).is_err() {
                        let bytes = s.len() + tail.len();
                        *slot = Value::Str(std::rc::Rc::new(s));
                        return Err(crate::vm::materialize_refused(
                            crate::value::MaterializeLimit::Alloc(bytes),
                            line,
                            col,
                        ));
                    }
                    s.push_str(&tail);
                    s
                }
                Err(shared) => {
                    let mut s = String::with_capacity(shared.len() + tail.len());
                    s.push_str(&shared);
                    s.push_str(&tail);
                    s
                }
            };
            Ok(Value::Str(std::rc::Rc::new(new)))
        } else {
            let mut s = String::with_capacity(tail.len() + 16);
            let (el, ec) = match parts.first() {
                Some(InterpPart::Expr(e, _)) => e.position(),
                _ => (line, col),
            };
            crate::value::write_value(&mut s, slot, el, ec)?;
            s.push_str(&tail);
            if s.len() > crate::interp::MAX_STRING_LEN {
                return Err(too_long());
            }
            Ok(Value::Str(std::rc::Rc::new(s)))
        }
    }

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

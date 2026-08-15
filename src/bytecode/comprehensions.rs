//! Lowering comprehensions to bytecode: `map`/`filter`/`where` and `any`/`all`
//! loops, plus `reduce` (with fused `range(...).reduce(...)` and the JIT
//! `TryJitReduce` fast path). An `impl super::Compiler` block split from the main
//! `compile_expr` dispatcher.

use super::*;

impl super::Compiler {
    /// Declare a comprehension element binder pattern. For a single binder
    /// (`it`/`x`) `CompNext` writes straight into its slot. For a multi-binder
    /// pattern (`(a, b)`) `CompNext` writes the element into a hidden slot, which
    /// the returned `DestructureBind` op then splits into the named param slots
    /// each iteration (mirroring the tree-walker's `eval_with_pattern`).
    fn declare_binder_pattern(b: &mut Builder, params: &[String]) -> (u32, Option<std::rc::Rc<Vec<u32>>>) {
        if params.len() == 1 {
            (b.declare_local(&params[0]), None)
        } else {
            let elem = b.declare_local("$elem");
            let slots: Vec<u32> = params.iter().map(|p| b.declare_local(p)).collect();
            (elem, Some(std::rc::Rc::new(slots)))
        }
    }

    pub(super) fn compile_comprehension(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> R<()> {
        if name == "reduce" {
            return self.compile_reduce(b, recv, args, line, col);
        }
        if name == "scan" {
            return self.compile_scan(b, recv, args, line, col);
        }
        if args.len() != 1 {
            let example = if name == "map" { "(it * 2)" } else { "(it > 0)" };
            return self.raise_after_recv(
                b,
                recv,
                format!("`{}` takes exactly one expression", name),
                format!("e.g. `xs.{}{}`.", name, example),
                line,
                col,
            );
        }
        let (params, body) = crate::interp::comprehension_params(&args[0]);
        if params.is_empty() {
            return self.raise_after_recv(
                b,
                recv,
                format!("`{}`'s function needs at least one parameter", name),
                "e.g. `xs.map(it * 2)` or `xs.map((a, b) => ...)`.".to_string(),
                line,
                col,
            );
        }
        // `where` gets its own kind purely so runtime errors quote the method
        // the user wrote (the walker threads the surface name identically).
        let kind = match name {
            "map" => CompKind::Map,
            "where" => CompKind::Where,
            _ => CompKind::Filter,
        };

        // #31 parallel nested-reduce: recognize `range(os,oe).map(i => range(is,ie).reduce(
        // init, (acc,j) => rbody))` where the inner reduce captures exactly the outer binder
        // `i` (a scalar). Emit the native operands `[os,oe,is,ie,init]` + a `TryJitNestedReduce`
        // guard HERE — before the receiver is compiled — so at the guard the stack holds only
        // those five. On success the VM runs the outer range in parallel (rayon over the native
        // inner kernel) and jumps past the fallback; on failure it pops the five and falls
        // through into the ordinary map-of-reduce below (the oracle path). The guard's `after`
        // is patched to the convergence point once known.
        let nested_guard: Option<(usize, u32)> = if matches!(kind, CompKind::Map) && params.len() == 1 {
            self.emit_nested_reduce_attempt(b, recv, &params[0], body, line, col)?
        } else {
            None
        };

        self.compile_expr(b, recv)?;

        // JIT fast path: a pure single-binder body over an `Int` array runs as a native
        // (optionally parallel) kernel. Runtime-guarded — non-`Int` arrays, `missing`,
        // ineligible bodies, and no-JIT builds fall through to the bytecode loop below.
        // The guard's `after` target is patched to the convergence point once known.
        let is_map = matches!(kind, CompKind::Map);
        let fns = self.jit_fn_set();
        let user_fns = self.user_fn_set();
        // `map` may capture free numeric variables (passed to the kernel as a `caps`
        // slice); `filter` kernels take no captures. A map body that is `i64`-closed
        // emits the kernel via the `i64` analysis; an `f64`-closed body (float literals
        // / `{+,-,*}` only) emits it via the `f64` analysis — the VM dispatches on the
        // source array's element type at run time (`Int`→i64 kernel, `Float`→f64). Both
        // analyses collect free vars in the same first-appearance order, so the stored
        // capture list is consistent whichever kernel the dispatch ends up taking.
        // Each analysis yields the ordered captures; the indexed ones add bounds, and the
        // mixed-indexed one may add synthetic `$aff*` terms (an affine index's pre-computed
        // base/coef — expressions, not variables, so the push loop below evaluates them).
        let scalar = |v: Vec<String>| {
            let caps: Vec<Capture> =
                v.into_iter().map(|name| Capture { name, kind: CaptureKind::Scalar }).collect();
            (caps, Vec::new(), Vec::new())
        };
        type MapAnalysis = (Vec<Capture>, Vec<IndexBound>, Vec<(String, Expr)>);
        let captures: Option<MapAnalysis> = if params.len() != 1 {
            None
        } else if is_map {
            crate::jit::map_kernel_captures(body, &params[0], &fns)
                .map(scalar)
                .or_else(|| {
                    crate::jit::map_kernel_captures_f64(body, &params[0], &user_fns).map(scalar)
                })
                // A body reading a captured array (`a[it]`). Tried AFTER the scalar analyses so
                // an unindexed body keeps its existing (bound-free) kernel unchanged — this arm
                // only ever admits shapes that used to fall to the per-element bytecode loop.
                // Its affine indices (`a[2*i]`) mint synthetic `$aff` base/coef terms,
                // which the push loop below evaluates — same contract as the mixed twin.
                .or_else(|| crate::jit::map_kernel_captures_indexed(body, &params[0], &fns))
                // The FLOAT-rooted indexed body (`a[i] + b[i]` over f64 arrays, `a[i*n + k]`)
                // the i64 analysis rejects. Same capture/bounds vocabulary — the JIT builds
                // the mixed (i64 range → f64 out) specialization from it, and a body BOTH
                // analyses admit (`a[i] + 1`) got the same list from the i64 one above, so
                // whichever runs, `caps[j]` and the VM's load order agree.
                .or_else(|| crate::jit::mixed_map_captures_indexed(body, &params[0], &fns, &user_fns, &self.mixed_sigs))
                // A **mixed** `Int`-source → `Float` body the i64/f64 analyses both reject —
                // e.g. `(it % 97) * 1.0` (integer `%`/`//`/bitwise/shift subexpression, float
                // root), including one that captures free scalars (`(c * it) * 0.5`, which is
                // what every nested array build looks like: the inner map captures the outer
                // binder). Storing the kernel lets the JIT build the mixed specialization the
                // VM dispatches for an `Int` source, instead of the whole map falling to the
                // per-element bytecode loop. The captures ride as `i64` and the VM re-checks
                // each is really an `Int` before dispatch — see `mixed_map_eligible`.
                .or_else(|| {
                    crate::jit::mixed_map_eligible(body, &params[0], &fns, &user_fns, &self.mixed_sigs)
                        .map(|c| (c, Vec::new(), Vec::new()))
                })
                // The Int-ROOTED mixed body — `to_int(to_float(it) * 1.5)`, which previously
                // had NO kernel shape at all. Tried LAST, so it only admits what every other
                // analysis rejected (the i64 analysis in particular ran first, so a plain
                // i64-closed body never lands here).
                .or_else(|| {
                    crate::jit::mixed_map_int_root_eligible(body, &params[0], &fns, &user_fns, &self.mixed_sigs)
                        .map(|c| (c, Vec::new(), Vec::new()))
                })
        } else {
            // A `filter` predicate may capture free `i64` scalars, same as a `map` body — the
            // VM proves each is an `Int` at dispatch and passes them as a `caps` slice.
            crate::jit::filter_kernel_eligible(body, &params[0], &fns)
                .map(|c| (c, Vec::new(), Vec::new()))
                // The f64 (`Floats`-source) predicate — `ys.filter(it < 5.0)` — stored so
                // the "filterf" build pass can compile its specialization. Tried second, so
                // a both-eligible predicate (`it > k`) stores the i64 analysis's captures;
                // both analyses walk left-to-right collecting first appearances, so the
                // lists coincide, which each build pass's re-check verifies against the
                // stored list before compiling.
                .or_else(|| {
                    crate::jit::filter_kernel_eligible_f64(body, &params[0], &user_fns)
                        .map(|c| (c, Vec::new(), Vec::new()))
                })
        };
        // Computed here, while `user_fns` is still borrowable — the capture-push loop below
        // needs `&mut self`, which would end the borrow.
        let kernel_raises = crate::jit::body_raises(body, &user_fns, &self.mixed_sigs);
        let kernel_guard: Option<(usize, u32)> = if let Some((caps, index_bounds, synth_exprs)) =
            captures
        {
            // Push each captured value (in capture order) just above the receiver array;
            // the VM pops them off whether or not it takes the native kernel. An array cap
            // pushes the ARRAY itself — the VM turns it into a base pointer only after
            // discharging that cap's bounds obligation. A synthetic `$aff*` slot pushes its
            // EXPRESSION's value — counter-free `+ - *` over names in scope, so evaluating
            // it once here is side-effect-free (the fall-through bytecode loop re-evaluates
            // the original index itself, and a double evaluation of anything effectful
            // would diverge from the oracle — same argument as the reduce site's).
            for cap in &caps {
                match synth_exprs.iter().find(|(n, _)| *n == cap.name) {
                    Some((_, e)) => self.compile_expr(b, &e.clone())?,
                    None => {
                        self.compile_expr(b, &Expr::Ident { name: cap.name.clone(), line, col })?
                    }
                }
            }
            let kernel = ArrayKernel {
                binder: params[0].clone(),
                body: body.clone(),
                captures: caps,
                index_bounds,
                raises: kernel_raises,
            };
            if is_map {
                let idx = self.map_kernels.len() as u32;
                self.map_kernels.push(kernel);
                Some((b.emit(Op::TryJitMap { kernel_idx: idx, after: 0 }, line, col), idx))
            } else {
                let idx = self.filter_kernels.len() as u32;
                self.filter_kernels.push(kernel);
                Some((b.emit(Op::TryJitFilter { kernel_idx: idx, after: 0 }, line, col), idx))
            }
        } else {
            None
        };

        let init_at = b.emit(Op::CompInit(kind, 0), line, col);

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;
        let (binder, destruct) = Self::declare_binder_pattern(b, &params);

        let loop_start = b.code.len() as u32;
        // Only `filter`/`where` (not `map`) read `cur_val` via `CompFilterPush`.
        let next_at = b.emit(Op::CompNext(binder, 0, !is_map), line, col);
        if let Some(slots) = &destruct {
            b.emit(Op::LoadLocal(binder), line, col);
            b.emit(Op::DestructureBind(slots.clone()), line, col);
        }
        self.compile_expr(b, body)?;
        b.emit(
            if matches!(kind, CompKind::Map) {
                Op::CompMapPush
            } else {
                Op::CompFilterPush(kind)
            },
            line,
            col,
        );
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(binder, end_at, !is_map);
        b.emit(Op::CompEnd, line, col);
        let jump_done = b.emit(Op::Jump(0), line, col);

        // missing-source landing: push `missing` as the whole result
        let missing_at = b.code.len() as u32;
        b.code[init_at] = Op::CompInit(kind, missing_at);
        let mk = b.add_const(Value::Missing);
        b.emit(Op::Const(mk), line, col);

        let done_at = b.code.len() as u32;
        b.code[jump_done] = Op::Jump(done_at);

        // The native kernel pushes its result array and lands here, where both the
        // bytecode-loop and missing-source paths converge with the result on the stack.
        if let Some((at, idx)) = kernel_guard {
            b.code[at] = if is_map {
                Op::TryJitMap { kernel_idx: idx, after: done_at }
            } else {
                Op::TryJitFilter { kernel_idx: idx, after: done_at }
            };
        }
        // #31: the nested-reduce guard converges at the SAME point (result on the stack) — on
        // success the parallel path jumps here, skipping the fallback map it wraps.
        if let Some((gpos, inner_idx)) = nested_guard {
            b.code[gpos] = Op::TryJitNestedReduce { inner_loop_idx: inner_idx, after: done_at };
        }

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }

    /// Compile `any`/`all` into a short-circuiting loop with a hidden
    /// "seen-missing" slot: `missing` in the undetermined position makes the
    /// answer `missing` (ADR-0001 three-valued logic), exactly like the interpreter.
    pub(super) fn compile_any_all(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> R<()> {
        if args.len() != 1 {
            return self.raise_after_recv(
                b,
                recv,
                format!("`{}` takes exactly one expression", name),
                format!("e.g. `xs.{}(it > 0)`.", name),
                line,
                col,
            );
        }
        let (params, body) = crate::interp::comprehension_params(&args[0]);
        if params.is_empty() {
            return self.raise_after_recv(
                b,
                recv,
                format!("`{}`'s function needs at least one parameter", name),
                "e.g. `xs.any(it > 0)` or `xs.all((a, b) => a < b)`.".to_string(),
                line,
                col,
            );
        }
        let is_all = name == "all";
        let kind = if is_all { CompKind::All } else { CompKind::Any };

        self.compile_expr(b, recv)?;
        let init_at = b.emit(Op::CompInit(kind, 0), line, col);

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;
        let (binder, destruct) = Self::declare_binder_pattern(b, &params);
        // hidden seen-missing flag (the name can't collide with user identifiers)
        let fk = b.add_const(Value::Bool(false));
        b.emit(Op::Const(fk), line, col);
        let sm = b.declare_local("$sm");
        b.emit(Op::StoreLocal(sm), line, col);

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(binder, 0, false), line, col);
        if let Some(slots) = &destruct {
            b.emit(Op::LoadLocal(binder), line, col);
            b.emit(Op::DestructureBind(slots.clone()), line, col);
        }
        self.compile_expr(b, body)?;
        let test_at = b.emit(Op::CompBoolTest(is_all, sm, 0), line, col);
        b.emit(Op::Jump(loop_start), line, col);

        // exhausted without short-circuiting: `missing` if any element was missing,
        // else the default (`all` → true, `any` → false).
        let exhausted = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(binder, exhausted, false);
        b.emit(Op::CompEndDiscard, line, col);
        b.emit(Op::LoadLocal(sm), line, col);
        let jif = b.emit(Op::JumpIfFalse(0), line, col);
        let mk = b.add_const(Value::Missing);
        b.emit(Op::Const(mk), line, col);
        let jdone1 = b.emit(Op::Jump(0), line, col);
        let notmiss = b.code.len() as u32;
        b.code[jif] = Op::JumpIfFalse(notmiss);
        let dk = b.add_const(Value::Bool(is_all));
        b.emit(Op::Const(dk), line, col);
        let jdone2 = b.emit(Op::Jump(0), line, col);

        // short-circuit landing: `any` → true, `all` → false
        let short = b.code.len() as u32;
        b.code[test_at] = Op::CompBoolTest(is_all, sm, short);
        b.emit(Op::CompEndDiscard, line, col);
        let sk = b.add_const(Value::Bool(!is_all));
        b.emit(Op::Const(sk), line, col);
        let jdone3 = b.emit(Op::Jump(0), line, col);

        // missing source
        let missing_at = b.code.len() as u32;
        b.code[init_at] = Op::CompInit(kind, missing_at);
        let mk2 = b.add_const(Value::Missing);
        b.emit(Op::Const(mk2), line, col);

        let done = b.code.len() as u32;
        b.code[jdone1] = Op::Jump(done);
        b.code[jdone2] = Op::Jump(done);
        b.code[jdone3] = Op::Jump(done);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }

    /// `position(p)` / `position(p, want)` — the first index whose predicate result is
    /// exactly `Bool(want)`, else `missing`. SHORT-CIRCUITS, which is the whole point:
    /// this replaced a `map(p).index_of(Bool(want))` desugar that ran the predicate over
    /// every element and materialized one `Value` per element, so
    /// `(0..90_000_000).take_while(it < 5)` cost 17.4s and 2.1 GB to answer `5`.
    ///
    /// The loop is `compile_any_all`'s with one extra local: a running index, bumped once
    /// per element and read at the short-circuit landing. Anything that is not
    /// `Bool(want)` — `false`, `missing`, or a non-boolean — falls through WITHOUT an
    /// error, because that is what `index_of` did (`values_equal` is `false` for every
    /// non-`Bool` against a `Bool`). `[5, 6, 7].position(it)` is `missing`, not a type
    /// error, and the walker's arm says the same.
    pub(super) fn compile_position(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> R<()> {
        if args.is_empty() || args.len() > 2 {
            return self.raise_after_recv(
                b,
                recv,
                "`position` takes exactly one expression".to_string(),
                "e.g. `xs.position(it > 0)`.".to_string(),
                line,
                col,
            );
        }
        // `want` is `true` from source; the two-argument form is generated only by the
        // `take_while`/`drop_while` desugar, whose literal is always a `Bool`.
        let want = match args.get(1) {
            None => true,
            Some(Expr::Bool(w)) => *w,
            Some(_) => {
                return self.raise_after_recv(
                    b,
                    recv,
                    "`position` takes exactly one expression".to_string(),
                    "e.g. `xs.position(it > 0)`.".to_string(),
                    line,
                    col,
                )
            }
        };
        let (params, body) = crate::interp::comprehension_params(&args[0]);
        if params.is_empty() {
            return self.raise_after_recv(
                b,
                recv,
                "`position`'s function needs at least one parameter".to_string(),
                "e.g. `xs.position(it > 0)`.".to_string(),
                line,
                col,
            );
        }

        self.compile_expr(b, recv)?;
        let init_at = b.emit(Op::CompInit(CompKind::Position, 0), line, col);

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;
        let (binder, destruct) = Self::declare_binder_pattern(b, &params);
        // running index, zero before the first element (name unforgeable in source)
        let zk = b.add_const(Value::Int(0));
        b.emit(Op::Const(zk), line, col);
        let ix = b.declare_local("$ix");
        b.emit(Op::StoreLocal(ix), line, col);

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(binder, 0, false), line, col);
        if let Some(slots) = &destruct {
            b.emit(Op::LoadLocal(binder), line, col);
            b.emit(Op::DestructureBind(slots.clone()), line, col);
        }
        self.compile_expr(b, body)?;
        let test_at = b.emit(Op::CompFindTest { want, idx_slot: ix, short_target: 0 }, line, col);
        b.emit(Op::Jump(loop_start), line, col);

        // exhausted without a match → `missing`
        let exhausted = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(binder, exhausted, false);
        b.emit(Op::CompEndDiscard, line, col);
        let mk = b.add_const(Value::Missing);
        b.emit(Op::Const(mk), line, col);
        let jdone1 = b.emit(Op::Jump(0), line, col);

        // short-circuit landing → the index that matched
        let short = b.code.len() as u32;
        b.code[test_at] = Op::CompFindTest { want, idx_slot: ix, short_target: short };
        b.emit(Op::CompEndDiscard, line, col);
        b.emit(Op::LoadLocal(ix), line, col);
        let jdone2 = b.emit(Op::Jump(0), line, col);

        // `missing` receiver propagates (ADR 0001), like every other comprehension
        let missing_at = b.code.len() as u32;
        b.code[init_at] = Op::CompInit(CompKind::Position, missing_at);
        let mk2 = b.add_const(Value::Missing);
        b.emit(Op::Const(mk2), line, col);

        let done = b.code.len() as u32;
        b.code[jdone1] = Op::Jump(done);
        b.code[jdone2] = Op::Jump(done);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }

    /// Detect + emit the native attempt for a **parallel nested reduce** (#31):
    /// `range(os,oe).map(i => range(is,ie).reduce(init, (acc,j) => rbody))` where the inner
    /// reduce captures EXACTLY the outer binder `i` (a scalar). The inner bounds/init must be
    /// `i`-independent and idempotent — they are evaluated once for the native attempt and
    /// recompiled in the fallback, so a side effect would run twice. On a match this pushes the
    /// five operands `[os,oe,is,ie,init]`, registers the inner reduce loop, and emits a
    /// `TryJitNestedReduce` guard, returning `(guard_pos, inner_idx)` for the caller to patch
    /// `after`. Emits nothing and returns `None` when the shape doesn't match (the ordinary
    /// map-of-reduce then compiles as usual).
    #[allow(clippy::too_many_arguments)]
    fn emit_nested_reduce_attempt(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        outer_binder: &str,
        body: &Expr,
        line: usize,
        col: usize,
    ) -> R<Option<(usize, u32)>> {
        // outer: `range(os, oe)`
        let Some((os, oe)) = self.builtin_range_call(b, recv) else { return Ok(None) };
        // body: `range(is, ie).reduce(init, (acc, j) => rbody)`
        let Expr::Method { recv: inner_recv, name, args, .. } = body else { return Ok(None) };
        if name.as_str() != "reduce" || args.len() != 2 {
            return Ok(None);
        }
        let Expr::Lambda { params, body: rbody, .. } = &args[1] else { return Ok(None) };
        if params.len() != 2 {
            return Ok(None);
        }
        let init = &args[0];
        let (pa, pb) = (params[0].as_str(), params[1].as_str());
        let Some((is, ie)) = self.builtin_range_call(b, inner_recv) else { return Ok(None) };
        // i64 inner only (a `Float` init is the f64 path — a follow-up).
        if crate::jit::is_float_acc_init(init) {
            return Ok(None);
        }
        // The inner bounds may be AFFINE in the outer binder — the TRIANGULAR `range(i + 1, n)`
        // of an all-pairs loop. Split each into `coeff * i + base`: the `i`-free `base` is pushed
        // as the operand (evaluated once in the outer scope, exactly as before), and `coeff` rides
        // in the `ReduceLoop` so each parallel worker can compute its OWN bounds from its OWN `i`.
        // A bound with no `i` in it yields `coeff = 0` / `base = <the bound>` — the rectangular
        // case, unchanged. A non-affine bound (`i * i`, `arr[i]`) declines here as it does today.
        let Some((sc, is_base)) = (match is {
            None => Some((0, None)),
            Some(e) => affine_in(e, outer_binder),
        }) else {
            return Ok(None);
        };
        let Some((ec, ie_base)) = affine_in(ie, outer_binder) else { return Ok(None) };
        // Every pushed operand must be idempotent (evaluated for the native attempt AND recompiled
        // in the fallback). Note this gates the affine BASES, not the raw bounds: `range(i + 1, n)`
        // pushes the base `1`, not the non-idempotent `Binary` `i + 1` — which is why the bases
        // must be existing subexpressions and never synthesized nodes.
        for e in [Some(oe), os, ie_base, is_base, Some(init)].into_iter().flatten() {
            if !is_idempotent(e) {
                return Ok(None);
            }
        }
        // The outer bounds and the `init` must still be free of `i`: they are evaluated ONCE at the
        // push site and cannot vary per outer iteration (an `init` mentioning `i` is genuinely
        // per-iteration, so hoisting it would be wrong). `affine_in` already guarantees the inner
        // BASES are `i`-free, so the inner bounds are no longer part of this gate.
        if expr_mentions(oe, outer_binder)
            || expr_mentions(init, outer_binder)
            || os.is_some_and(|e| expr_mentions(e, outer_binder))
        {
            return Ok(None);
        }
        // The inner reduce must be a captured i64 reduce whose captures are exactly one Scalar —
        // the outer binder `i`, which varies per outer iteration — plus zero or more i64 arrays
        // (loop-invariant bases it indexes by `i` and/or the counter `j`; the all-pairs distance
        // matrix). The arrays are evaluated ONCE below, in the outer scope (`i` is not yet bound),
        // so their VALUE is `i`-independent even though they're indexed by `i` inside; they ride
        // read-only across the parallel workers, and their bounds obligations (`bnds`) are hoisted
        // and checked once by the VM before the parallel region. An f64 array cap is NOT
        // parallelized yet — its non-associative fold needs the serial captured-reduce path — so
        // it declines here (falls to the ordinary serial map-of-reduce).
        let fns = self.jit_fn_set();
        // Synth (`$aff`) terms are declined here: this site pushes every capture as a BARE
        // IDENT (see the push loop below), so a synthetic slot's name would not resolve.
        let (caps, bnds) = match crate::jit::reduce_loop_captures(rbody, pa, pb, &fns) {
            Some((caps, bnds, synth))
                if synth.is_empty()
                    && bnds
                        .iter()
                        .all(|b| !matches!(b, crate::bytecode::IndexBound::Affine { .. })) =>
            {
                (caps, bnds)
            }
            _ => return Ok(None),
        };
        let one_scalar = caps.iter().filter(|c| c.kind == CaptureKind::Scalar).count() == 1;
        let shape_ok = one_scalar
            && caps.iter().all(|c| match c.kind {
                CaptureKind::Scalar => c.name == outer_binder,
                CaptureKind::ArrayI64 => true,
                CaptureKind::ArrayF64 => false,
                // A `map`-only kind: `reduce_loop_captures` (this path's collector) never emits
                // it, so it cannot appear on an inner reduce — decline defensively rather than
                // assume a shape.
                CaptureKind::ScalarValue => false,
            });
        if !shape_ok {
            return Ok(None);
        }

        // --- matched: push [arr_1 .. arr_K, os, oe, is, ie, init] — the array bases go BELOW the
        // five scalars so the VM reads the scalars at a fixed top-of-stack offset. Array caps are
        // pushed in `captures` order (ArrayI64 only); the VM consumes them in that same order. Each
        // is a bare free variable (recorded only when the index receiver is a free `Ident`), hence
        // idempotent for the fallback's by-name re-read. Then register the inner loop + guard. ---
        // Before the pushes take `&mut self`. This is the i64 nested-reduce path, whose kernel
        // has no poison ABI at all — but the flag is still computed honestly rather than
        // hard-coded, so widening the analysis later cannot leave this site quietly stale.
        let raises = {
            let user_fns = self.user_fn_set();
            crate::jit::body_raises(rbody, &user_fns, &self.mixed_sigs)
        };
        for c in caps.iter().filter(|c| c.kind == CaptureKind::ArrayI64) {
            let ident = Expr::Ident { name: c.name.clone(), line, col };
            self.compile_expr(b, &ident)?;
        }
        self.push_or_zero(b, os, line, col)?;
        self.compile_expr(b, oe)?;
        // The inner bounds push their affine BASES (each `i`-free); the VM adds `coeff * i` per
        // worker. For a rectangular range the base IS the bound, so this is the previous push.
        self.push_or_zero(b, is_base, line, col)?;
        self.push_or_zero(b, ie_base, line, col)?;
        self.compile_expr(b, init)?;
        let inner_idx = self.reduce_loops.len() as u32;
        self.reduce_loops.push(ReduceLoop {
            pa: pa.to_string(),
            pb: pb.to_string(),
            bodies: vec![rbody.as_ref().clone()],
            captures: caps,
            index_bounds: bnds,
            float: false,
            raises,
            inner_start_coeff: sc,
            inner_end_coeff: ec,
        });
        let guard = b.emit(Op::TryJitNestedReduce { inner_loop_idx: inner_idx, after: 0 }, line, col);
        Ok(Some((guard, inner_idx)))
    }

    /// Push a range start operand: the expression, or the literal `0` when omitted.
    fn push_or_zero(&mut self, b: &mut Builder, start: Option<&Expr>, line: usize, col: usize) -> R<()> {
        match start {
            None => {
                let c0 = b.add_const(Value::Int(0));
                b.emit(Op::Const(c0), line, col);
            }
            Some(e) => self.compile_expr(b, e)?,
        }
        Ok(())
    }

    /// `range(a, b).reduce(init, (acc, x) => body)` as a counting loop. No input
    /// array is built; `x` (the second binder) is the loop counter.
    #[allow(clippy::too_many_arguments)]
    fn compile_reduce_range(
        &mut self,
        b: &mut Builder,
        start: Option<&Expr>,
        end: &Expr,
        init: &Expr,
        pa: &str,
        pb: &str,
        body: &Expr,
        line: usize,
        col: usize,
    ) -> R<()> {
        // Push [start, end] for CompInitRange, then drive it with the same loop
        // as an array reduce — one dispatch per element, zero array allocated.
        match start {
            None => {
                let c0 = b.add_const(Value::Int(0));
                b.emit(Op::Const(c0), line, col);
            }
            Some(e) => self.compile_expr(b, e)?,
        }
        self.compile_expr(b, end)?;

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;

        // CRITICAL: compile `init` while the scope is still empty — the binders
        // (`pa`/`pb`) must NOT be visible to it. `reduce(x, (acc, x) => ...)`
        // evaluates `init` in the *outer* environment, so an `init` that mentions a
        // binder name must resolve to the outer binding, not the (unbound) loop
        // slot. Its value stays on the stack until it is stored into `acc` below.
        //
        // If the body is a pure `i64` expression over `{acc, x}`, register a native
        // loop for it and emit a runtime-guarded fast path. The guard (in the VM)
        // takes the native path only when start/end/init are all `Int` within the
        // cap; otherwise it falls through to the identical bytecode loop — so float
        // accumulators, over-cap ranges, and non-x86/`HELIX_NOJIT` builds all run
        // the oracle-matched path.
        // Scalar OR tuple `i64` accumulator → a native loop. `reduce_jit_bodies` returns
        // the (substituted) component bodies for either shape, or `None`. If that fails, a
        // *scalar* body may still be eligible by capturing free outer `i64` vars (the
        // nested-fold case), in which case its values are pushed above `[start, end]` and
        // handed to the kernel as `caps`.
        // The init literal's type fixes the accumulator type (as in the fused reduce path):
        // a `Float` init → a scalar `f64` fold over the i64 counter (`reduce_jit_f64_range_body`,
        // a mixed body whose root is `Float`). A non-float init → the i64 scalar/tuple path
        // (`reduce_jit_bodies`), or a scalar capturing the outer fold var. These must NOT
        // compete — `reduce_jit_bodies` keys only off body shape, so a `Float` init would
        // otherwise be claimed as a never-firing i64 kernel (the engagement-gate trap).
        let fns = self.jit_fn_set();
        // Synthetic `$aff*` captures: an affine index's pre-computed `base`/`coef` (`i*n` in
        // `a[i*n+k]`). Each is a counter-free `+ - *` expression over names already in scope, so
        // evaluating it ONCE here is both cheap and side-effect-free — which matters because the
        // fall-through bytecode loop re-evaluates the original index itself, and a double
        // evaluation of anything effectful would diverge from the oracle.
        let mut synth_exprs: Vec<(String, Expr)> = Vec::new();
        #[allow(clippy::type_complexity)]
        let (jit_bodies, captures, bounds, float): (Option<Vec<Expr>>, Vec<Capture>, Vec<IndexBound>, bool) =
            if crate::jit::is_float_acc_init(init) {
                // A `Float` init (scalar) or all-`Float` tuple/record init → an `f64`
                // accumulator folded over the i64 counter (`pb_is_int = true`). A scalar body
                // may index captured `f64` arrays by the counter (the float dot-product,
                // v1b) — try that first, then the capture-free scalar/tuple forms. (The f64 VM
                // path range-checks its array caps inline, so it carries no `IndexBound`s.)
                let user_fns = self.user_fn_set();
                if matches!(init, Expr::Float(_)) {
                    match crate::jit::reduce_jit_f64_range_captures(init, body, pa, pb, &fns, &user_fns) {
                        Some(c) => {
                            synth_exprs = c.synth;
                            (Some(vec![c.body]), c.caps, c.bounds, true)
                        }
                        None => match crate::jit::reduce_jit_f64_range_body(init, body, pa, pb, &fns, &user_fns, &self.mixed_sigs) {
                            Some(b) => (Some(vec![b]), Vec::new(), Vec::new(), true),
                            None => (None, Vec::new(), Vec::new(), false),
                        },
                    }
                } else {
                    match crate::jit::reduce_jit_f64_tuple_bodies(init, body, pa, pb, true, &user_fns) {
                        Some(bodies) => (Some(bodies), Vec::new(), Vec::new(), true),
                        None => (None, Vec::new(), Vec::new(), false),
                    }
                }
            } else {
                match crate::jit::reduce_jit_bodies(init, body, pa, pb, &fns) {
                    Some(bodies) => (Some(bodies), Vec::new(), Vec::new(), false),
                    None => match crate::jit::reduce_loop_captures(body, pa, pb, &fns) {
                        // AFFINE bounds are DECLINED on the reduce path, deliberately. The
                        // i64 affine admission exists for the standalone MAP kernel, where
                        // it measures 16-50x (`a[2*i]`, the 3-point stencil); admitting it
                        // here made the same body 1.9x SLOWER (416 -> 792 ms at 10M),
                        // because emitting the guard changes which compiled form the body
                        // gets while this path's dispatch never takes the kernel. Keeping
                        // the reduce at Counter/Scalar keeps it byte-identical to what it
                        // was. Lifting this needs the reduce dispatch to actually run the
                        // affine kernel, measured — see docs/v0.2.1-fix-plan.md.
                        Some((caps, bnds, syn))
                            if !caps.is_empty()
                                && syn.is_empty()
                                && bnds.iter().all(|b| {
                                    !matches!(b, crate::bytecode::IndexBound::Affine { .. })
                                }) =>
                        {
                            (Some(vec![body.clone()]), caps, bnds, false)
                        }
                        _ => (None, Vec::new(), Vec::new(), false),
                    },
                }
            };
        let acc;
        let x;
        let guard;
        if let Some(bodies) = jit_bodies {
            // Computed while `self` is only borrowed immutably — the capture pushes below
            // need `&mut self`, same constraint the map site records. This decides the
            // kernel's ABI, so it is stored on the loop rather than re-derived in the VM.
            let raises = {
                let user_fns = self.user_fn_set();
                bodies.iter().any(|bd| crate::jit::body_raises(bd, &user_fns, &self.mixed_sigs))
            };
            self.compile_expr(b, init)?; // stack: [start, end, init]
            acc = b.declare_local(pa);
            x = b.declare_local(pb);
            b.emit(Op::StoreLocal(acc), line, col); // stack: [start, end]; acc=init
            // Push each captured value above `[start, end]` (resolved in the enclosing
            // scope — captures are free vars, never `pa`/`pb`). A `Scalar` cap resolves to
            // its `i64` value, an `ArrayI64` cap to its `Value::Array`; the VM marshals each
            // by kind. The VM splits them off before `CompInitRange` whether or not it takes
            // the native path.
            for cap in &captures {
                // A synthetic affine `base`/`coef` slot pushes its EXPRESSION's value; every other
                // cap pushes the value of the name it captured.
                match synth_exprs.iter().find(|(n, _)| *n == cap.name) {
                    Some((_, e)) => self.compile_expr(b, &e.clone())?,
                    None => self.compile_expr(b, &Expr::Ident { name: cap.name.clone(), line, col })?,
                }
            }
            let loop_idx = self.reduce_loops.len() as u32;
            self.reduce_loops.push(ReduceLoop {
                pa: pa.to_string(),
                pb: pb.to_string(),
                bodies,
                captures,
                index_bounds: bounds,
                float,
                raises,
                // Not a nested-reduce call site: this loop's bounds are ordinary stack operands,
                // never affine in an outer binder. The VM reads these only via `TryJitNestedReduce`.
                inner_start_coeff: 0,
                inner_end_coeff: 0,
            });
            // `after` is patched once the trailing LoadLocal position is known.
            let at = b.emit(Op::TryJitReduce { loop_idx, acc_slot: acc, after: 0 }, line, col);
            b.emit(Op::CompInitRange, line, col); // consumes [start, end] on fall-through
            guard = Some((at, loop_idx));
        } else {
            b.emit(Op::CompInitRange, line, col); // consumes [start, end]
            self.compile_expr(b, init)?; // outer scope (binders not declared yet)
            acc = b.declare_local(pa);
            x = b.declare_local(pb);
            b.emit(Op::StoreLocal(acc), line, col);
            guard = None;
        }

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(x, 0, false), line, col);
        self.emit_reduce_body_and_store(b, body, pa, pb, acc, line, col)?;
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(x, end_at, false);
        b.emit(Op::CompEndDiscard, line, col);
        let after_at = b.code.len() as u32;
        if let Some((at, loop_idx)) = guard {
            b.code[at] = Op::TryJitReduce { loop_idx, acc_slot: acc, after: after_at };
        }
        b.emit(Op::LoadLocal(acc), line, col);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }

    /// `range(s,e).map(f).reduce(init, (pa,pb) => g)` — fuse the map INTO the fold via the
    /// identity `map(f).reduce(init,g) ≡ reduce(init, (acc,i) => g(acc, f(i)))`, so no
    /// intermediate array is built at all. The rewrite is exact: `map` preserves order and the
    /// fold is still left-to-right over the same element values, so the result is bit-identical.
    ///
    /// WHY SUBSTITUTION AND NOT AN f64 `FusionStage`. The f64 reduce kernel already handles
    /// everything such a body needs: an `f64` accumulator over the `i64` counter, `ArrayF64`
    /// captures with i128-proven bounds, affine indices, value scalars, multi-accumulator
    /// splitting, and the poison-flag division rule. Teaching [`FusionStage`] to carry f64
    /// elements would build a SECOND implementation of that same capability, which would then
    /// drift from the first. Substitution reuses the proven one.
    ///
    /// SAFETY comes from the emission shape, not from the identity. The fused body is emitted as
    /// a `TryJitReduce` GUARD whose fall-through is the ORIGINAL unfused expression, so the fused
    /// form runs ONLY as native code, and only after the VM proves its preconditions (`Int`
    /// bounds within the materialization cap, a `Float` init, every array index in range, and a
    /// clear poison flag for a dividing body). A kernel that satisfies those cannot raise — which
    /// retires the one real hazard of map-reduce fusion: unfused, `map` evaluates EVERY `f(i)`
    /// before any `g`, so if both could raise, the two spellings could report DIFFERENT errors
    /// (f's at a later index rather than g's at an earlier one). Every case that can raise takes
    /// the untouched original path, where the walker, the VM and the JIT agree by construction.
    ///
    /// Returns `true` if the guarded form was emitted (the caller must emit nothing further),
    /// `false` if it declined WITHOUT emitting anything.
    #[allow(clippy::too_many_arguments)] // as its neighbours: the reduce's parts are all distinct
    fn emit_map_reduce_fusion(
        &mut self,
        b: &mut Builder,
        orig: &Expr,
        recv: &Expr,
        init: &Expr,
        pa: &str,
        pb: &str,
        gbody: &Expr,
        line: usize,
        col: usize,
    ) -> R<bool> {
        // Suppressed while emitting a fall-through (this one's or a fused pipeline's), so the
        // recompiled original cannot re-detect itself and loop.
        if self.no_fuse || pa == pb {
            return Ok(false);
        }
        // recv must be `<source>.map(f)` with a single-parameter `f`. Both spellings count:
        // `map(x => x * 2)` AND the `it` sugar `map(it * 2)` — which is the IDIOMATIC one, so
        // matching `Expr::Lambda` directly (as this did first) silently excluded most real code
        // and left it 93× slower. `comprehension_params` is the same desugaring the compile path
        // itself uses, so the two cannot drift apart.
        let Expr::Method { recv: map_recv, name: mname, args: margs, .. } = recv else {
            return Ok(false);
        };
        if mname.as_str() != "map" || margs.len() != 1 {
            return Ok(false);
        }
        let (fp, fbody) = crate::interp::comprehension_params(&margs[0]);
        if fp.len() != 1 {
            return Ok(false);
        }
        // The source must be a `range` counter: the substituted body folds over the COUNTER, which
        // is what `reduce_jit_f64_range_*` (and its i128 bounds proof) is defined over.
        let Some((start, end)) = self.builtin_range_call(b, map_recv) else { return Ok(false) };
        // A `Float` or `Int` LITERAL init. Float takes the f64 substitution below, as always.
        // Int is admitted only when the substituted body CAPTURES (checked at the selection) —
        // the capture-free i64 chain already fuses through `FusedKernel`, and this site is only
        // reached when `collect_fusion_chain` declined, which for an i64 chain means a stage
        // captured. Before this, a captured i64 chain had NO fused form at all: it materialized
        // its intermediate (110 MB against 20 MB at 10M elements) and ran 0.34s against the
        // literal spelling's 0.00s.
        if !matches!(init, Expr::Float(_) | Expr::Int(_)) {
            return Ok(false);
        }
        // CAPTURE SAFETY: `f`'s body is about to sit inside a lambda binding `pa`, so a free
        // variable of `f` named `pa` would be captured by the accumulator and silently change
        // meaning. (`pb` needs no such check — it is the binder being replaced, and the new
        // counter name is synthetic, so nothing in `g` can be captured by it either.)
        if expr_mentions(fbody, pa) {
            return Ok(false);
        }
        // Every operand is evaluated at the push site AND again by the recompiled original on the
        // fall-through, so each must be idempotent.
        for e in [start, Some(end), Some(init)].into_iter().flatten() {
            if !is_idempotent(e) {
                return Ok(false);
            }
        }
        let counter = Expr::Ident { name: MR_COUNTER.to_string(), line, col };
        let Some(f_sub) = crate::jit::subst_ident(fbody, &fp[0], &counter) else {
            return Ok(false);
        };
        let Some(new_body) = crate::jit::subst_ident(gbody, pb, &f_sub) else { return Ok(false) };

        // Decide eligibility BEFORE emitting anything — the same two f64 range analyses
        // `compile_reduce_range` selects between, so the fused body is admitted on exactly the
        // terms an unfused one would be (and `reduce_bodies_eligible` re-derives both at build
        // time, which is what actually keeps compile-gate and build-gate from drifting).
        let user_fns = self.user_fn_set();
        let fns = self.jit_fn_set();
        let (bodies, captures, bounds, synth, float) = if matches!(init, Expr::Float(_)) {
            match crate::jit::reduce_jit_f64_range_captures(init, &new_body, pa, MR_COUNTER, &fns, &user_fns) {
                Some(c) => (vec![c.body], c.caps, c.bounds, c.synth, true),
                None => match crate::jit::reduce_jit_f64_range_body(init, &new_body, pa, MR_COUNTER, &fns, &user_fns, &self.mixed_sigs) {
                    Some(bd) => (vec![bd], Vec::new(), Vec::new(), Vec::new(), true),
                    None => return Ok(false),
                },
            }
        } else {
            // The i64 arm: the same collector `compile_reduce_range` uses for a captured scalar
            // body, so the fused body is admitted on exactly the terms an unfused one would be,
            // and `reduce_bodies_eligible` re-derives the identical list at build time. CAPTURES
            // MUST BE NON-EMPTY: a capture-free i64 chain belongs to `FusedKernel`, and if it was
            // declined for some other reason it should stay on the path it had, not gain a second
            // one here — this arm only admits shapes that previously had no fused form at all.
            // (An affine index's synthetic `$aff` terms are DECLINED here — this site pushes
            // every capture as a bare ident, exactly as `compile_reduce_range`'s captured arm
            // does, so a synthetic slot's name would not resolve. The parallel nested path
            // declines `Affine` bounds at dispatch anyway; keeping the shape off this arm
            // keeps it on the path it had.)
            match crate::jit::reduce_loop_captures(&new_body, pa, MR_COUNTER, &fns) {
                Some((caps, bnds, syn))
                    if !caps.is_empty()
                        && syn.is_empty()
                        && bnds.iter().all(|b| {
                            !matches!(b, crate::bytecode::IndexBound::Affine { .. })
                        }) =>
                {
                    (vec![new_body.clone()], caps, bnds, Vec::new(), false)
                }
                _ => return Ok(false),
            }
        };

        // Before the pushes below take `&mut self` — see the sibling site in `compile_reduce`.
        let raises = {
            let user_fns = self.user_fn_set();
            bodies.iter().any(|bd| crate::jit::body_raises(bd, &user_fns, &self.mixed_sigs))
        };

        // --- committed: emit [start, end], the acc slot, the captures, then the guard ---
        match start {
            None => {
                let c0 = b.add_const(Value::Int(0));
                b.emit(Op::Const(c0), line, col);
            }
            Some(e) => self.compile_expr(b, e)?,
        }
        self.compile_expr(b, end)?;

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;
        // `init` in the OUTER environment (no binder is in scope yet), then into a SYNTHETIC slot.
        // The slot must not be named `pa`: declaring `s` here would shadow an outer `s` for the
        // fall-through expression, so `range(0, s).map(…).reduce(0.0, (s, x) => …)` would resolve
        // its bound to this scratch slot.
        self.compile_expr(b, init)?;
        let acc = b.declare_local(MR_ACC);
        b.emit(Op::StoreLocal(acc), line, col);
        for cap in &captures {
            match synth.iter().find(|(n, _)| *n == cap.name) {
                Some((_, e)) => self.compile_expr(b, &e.clone())?,
                None => self.compile_expr(b, &Expr::Ident { name: cap.name.clone(), line, col })?,
            }
        }
        let loop_idx = self.reduce_loops.len() as u32;
        self.reduce_loops.push(ReduceLoop {
            pa: pa.to_string(),
            pb: MR_COUNTER.to_string(),
            bodies,
            captures,
            index_bounds: bounds,
            float,
            raises,
            inner_start_coeff: 0,
            inner_end_coeff: 0,
        });
        let at = b.emit(Op::TryJitReduce { loop_idx, acc_slot: acc, after: 0 }, line, col);

        // Fall-through: the VM leaves `[start, end]` on the stack (it only pops them when it TAKES
        // the native path), so discard them, then recompile the ORIGINAL unfused expression with
        // fusion suppressed so it cannot re-detect itself.
        b.emit(Op::Pop, line, col);
        b.emit(Op::Pop, line, col);
        self.no_fuse = true;
        let r = self.compile_expr(b, orig);
        self.no_fuse = false;
        r?;
        let jmp_at = b.emit(Op::Jump(0), line, col);

        // Native path lands here with the fold's result in the acc slot.
        let after_at = b.code.len() as u32;
        b.code[at] = Op::TryJitReduce { loop_idx, acc_slot: acc, after: after_at };
        b.emit(Op::LoadLocal(acc), line, col);
        let converge = b.code.len() as u32;
        b.code[jmp_at] = Op::Jump(converge);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(true)
    }

    fn compile_reduce(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> R<()> {
        if args.len() != 2 {
            return self.raise_after_recv(
                b,
                recv,
                "`reduce` takes a starting value and an accumulator function".to_string(),
                "e.g. `xs.reduce(0, (acc, x) => acc + x)` to sum.".to_string(),
                line,
                col,
            );
        }
        let (pa, pb, body) = match &args[1] {
            Expr::Lambda { params, body, .. } if params.len() == 2 => {
                (params[0].clone(), params[1].clone(), body.as_ref())
            }
            // Match the tree-walker's two precise messages (wrong arity vs not a
            // function), evaluating the receiver first for side-effect parity.
            Expr::Lambda { params, .. } => {
                return self.raise_after_recv(
                    b,
                    recv,
                    format!(
                        "`reduce`'s function needs exactly two parameters, but got {}",
                        params.len()
                    ),
                    "the first is the running accumulator, e.g. `(acc, x) => acc + x`.".to_string(),
                    line,
                    col,
                );
            }
            _ => {
                return self.raise_after_recv(
                    b,
                    recv,
                    "`reduce` needs an explicit accumulator function".to_string(),
                    "name both binders: `xs.reduce(0, (acc, x) => acc + x)`.".to_string(),
                    line,
                    col,
                );
            }
        };

        // Range fusion: `range(...).reduce(...)` becomes a counting loop with no
        // array materialized at all — the element binder *is* the counter.
        if let Some((start, end)) = self.builtin_range_call(b, recv) {
            return self.compile_reduce_range(b, start, end, &args[0], &pa, &pb, body, line, col);
        }

        // MAP→REDUCE fusion: `range(..).map(f).reduce(init, g)` folds `f` into `g` so the
        // intermediate array is never built. Guarded, with this same expression as the
        // fall-through — see `emit_map_reduce_fusion`.
        let orig = Expr::Method {
            recv: Box::new(recv.clone()),
            name: "reduce".to_string(),
            args: args.to_vec(),
            named: Vec::new(),
            line,
            col,
        };
        if self.emit_map_reduce_fusion(b, &orig, recv, &args[0], &pa, &pb, body, line, col)? {
            return Ok(());
        }

        self.compile_expr(b, recv)?;
        let init_at = b.emit(Op::CompInit(CompKind::Reduce, 0), line, col);

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;

        // Compile `init` while the scope is empty so the binders (`pa`/`pb`) are not
        // visible to it — `reduce` evaluates its initial accumulator in the *outer*
        // environment (see the note in `compile_reduce_range`).
        self.compile_expr(b, &args[0])?; // initial accumulator (outer scope)
        let acc = b.declare_local(&pa);
        let x = b.declare_local(&pb);
        b.emit(Op::StoreLocal(acc), line, col);

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(x, 0, false), line, col);
        self.emit_reduce_body_and_store(b, body, &pa, &pb, acc, line, col)?;
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(x, end_at, false);
        b.emit(Op::CompEndDiscard, line, col);
        b.emit(Op::LoadLocal(acc), line, col);
        let jump_done = b.emit(Op::Jump(0), line, col);

        let missing_at = b.code.len() as u32;
        b.code[init_at] = Op::CompInit(CompKind::Reduce, missing_at);
        let mk = b.add_const(Value::Missing);
        b.emit(Op::Const(mk), line, col);

        let done_at = b.code.len() as u32;
        b.code[jump_done] = Op::Jump(done_at);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }

    /// `xs.scan(init, (acc, x) => …)` — like `reduce`, but it COLLECTS every intermediate
    /// accumulator into an array (a generalized `cumsum`). Reuses the existing comprehension
    /// ops: a `Map` collector (so each pushed value lands in the result array) with the
    /// accumulator threaded through a local exactly as `reduce` does — `CompMapPush(acc)`
    /// each iteration, `CompEnd` to yield the array. Byte-identical to the tree-walker.
    fn compile_scan(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> R<()> {
        if args.len() != 2 {
            return self.raise_after_recv(
                b,
                recv,
                "`scan` takes a starting value and an accumulator function".to_string(),
                "e.g. `xs.scan(0, (acc, x) => acc + x)` for a running sum.".to_string(),
                line,
                col,
            );
        }
        let (pa, pb, body) = match &args[1] {
            Expr::Lambda { params, body, .. } if params.len() == 2 => {
                (params[0].clone(), params[1].clone(), body.as_ref())
            }
            Expr::Lambda { params, .. } => {
                return self.raise_after_recv(
                    b,
                    recv,
                    format!("`scan`'s function needs exactly two parameters, but got {}", params.len()),
                    "the first is the running accumulator, e.g. `(acc, x) => acc + x`.".to_string(),
                    line,
                    col,
                );
            }
            _ => {
                return self.raise_after_recv(
                    b,
                    recv,
                    "`scan` needs an explicit accumulator function".to_string(),
                    "name both binders: `xs.scan(0, (acc, x) => acc + x)`.".to_string(),
                    line,
                    col,
                );
            }
        };

        // Native fast path: `range(a, b).scan(init, (acc, x) => body)` with an `i64` body —
        // the prefix fold, which had no native form at all (0.54s against the `reduce` twin's
        // 0.00s at 10M elements). Operand protocol as `TryJitFused`: push `[start, end, init]`
        // plus any capture values, emit the guard (the VM consumes ALL operands whether or not
        // it takes the native path), and recompile THIS scan with `no_fuse` set as the
        // fall-through. Everything pushed is re-evaluated by that recompile, so each operand
        // must be idempotent — the same rule every other fusion site enforces.
        //
        // An `Int`-literal init only (the accumulator must be provably `i64`), scalar captures
        // only, and no index bounds: an `ArrayI64` capture would need the reduce path's bounds
        // discharge in the VM arm, and no probe has shown a scan body indexing an array yet —
        // decline rather than carry unexercised safety code. The kernel is SERIAL by nature
        // (element *i* of a prefix fold depends on element *i−1*), so unlike the map kernels
        // there is no parallel form to keep byte-identical; order is the definition.
        if !self.no_fuse
            && pa != pb
            && matches!(args[0], Expr::Int(_))
            && let Some((start, end)) = self.builtin_range_call(b, recv)
            && start.is_none_or(is_idempotent)
            && is_idempotent(end)
        {
            let fns = self.jit_fn_set();
            if let Some((caps, bounds, _)) = crate::jit::reduce_loop_captures(body, &pa, &pb, &fns)
                && bounds.is_empty()
                && caps.iter().all(|c| c.kind == CaptureKind::Scalar)
            {
                // Before the pushes take `&mut self`. The i64 scan kernel has no poison ABI
                // either; recorded rather than hard-coded for the same reason as the nested
                // site — a later widening must not silently skip a construction site.
                let raises = {
                    let user_fns = self.user_fn_set();
                    crate::jit::body_raises(body, &user_fns, &self.mixed_sigs)
                };
                match start {
                    None => {
                        let c0 = b.add_const(Value::Int(0));
                        b.emit(Op::Const(c0), line, col);
                    }
                    Some(e) => self.compile_expr(b, e)?,
                }
                self.compile_expr(b, end)?;
                self.compile_expr(b, &args[0])?;
                for cap in &caps {
                    self.compile_expr(b, &Expr::Ident { name: cap.name.clone(), line, col })?;
                }
                let loop_idx = self.scan_loops.len() as u32;
                self.scan_loops.push(ReduceLoop {
                    pa: pa.clone(),
                    pb: pb.clone(),
                    bodies: vec![body.clone()],
                    captures: caps,
                    index_bounds: bounds,
                    raises,
                    float: false,
                    inner_start_coeff: 0,
                    inner_end_coeff: 0,
                });
                let at = b.emit(Op::TryJitScan { loop_idx, after: 0 }, line, col);
                // Fall-through: this same scan, with the guard suppressed so it cannot
                // re-detect itself. Both paths leave exactly one array on the stack, and
                // nothing sits between the fall-through's end and the convergence point, so
                // no jump is needed.
                self.no_fuse = true;
                let r = self.compile_scan(b, recv, args, line, col);
                self.no_fuse = false;
                r?;
                let converge = b.code.len() as u32;
                b.code[at] = Op::TryJitScan { loop_idx, after: converge };
                return Ok(());
            }
        }

        self.compile_expr(b, recv)?;
        let init_at = b.emit(Op::CompInit(CompKind::Map, 0), line, col);

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;

        // `init` is evaluated in the outer scope (binders not visible), as in `reduce`.
        self.compile_expr(b, &args[0])?;
        let acc = b.declare_local(&pa);
        let x = b.declare_local(&pb);
        b.emit(Op::StoreLocal(acc), line, col);

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(x, 0, false), line, col);
        self.compile_expr(b, body)?; // new accumulator on the stack
        b.emit(Op::StoreLocal(acc), line, col);
        b.emit(Op::LoadLocal(acc), line, col); // push it again …
        b.emit(Op::CompMapPush, line, col); // … and append to the result array
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(x, end_at, false);
        b.emit(Op::CompEnd, line, col); // result array on the stack
        let jump_done = b.emit(Op::Jump(0), line, col);

        // missing-source landing: the whole result is `missing` (as for `map`).
        let missing_at = b.code.len() as u32;
        b.code[init_at] = Op::CompInit(CompKind::Map, missing_at);
        let mk = b.add_const(Value::Missing);
        b.emit(Op::Const(mk), line, col);

        let done_at = b.code.len() as u32;
        b.code[jump_done] = Op::Jump(done_at);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }
}

/// The source of a fuseable pipeline, as the AST to compile for the operands.
pub(super) enum FusionSourceExpr<'a> {
    Array(&'a Expr),
    Range(Option<&'a Expr>, &'a Expr),
}

/// A detected fuseable pipeline: the owned [`FusedKernel`] to register, plus the source
/// (and `reduce` init) expressions whose operands the guard pushes.
pub(super) struct FusionPlan<'a> {
    source: FusionSourceExpr<'a>,
    init: Option<&'a Expr>,
    kernel: FusedKernel,
}

/// An expression cheap and side-effect-free to evaluate twice — the fused guard pushes
/// the source/init for its native attempt and, on fall-through, the per-stage chain
/// recompiles them. Restricting fusion to such sources keeps that double-evaluation
/// unobservable.
/// Decompose `e` into an AFFINE function of `binder`: `e == coeff * binder + base`, returning
/// `(coeff, base)` where `base` NEVER mentions `binder` (`None` = the literal `0`, matching
/// [`super::Compiler::push_or_zero`]'s convention). `None` when `e` is not affine in `binder`.
///
/// This is what lets the #31 parallel nested-reduce accept a TRIANGULAR inner range. The inner
/// bounds are pushed as operands BEFORE the outer map's receiver is compiled — in the outer scope,
/// where `binder` is NOT yet bound — so a bound mentioning `binder` cannot be compiled there at
/// all (it would resolve to an unrelated outer `i`, or fail). Splitting it into an `i`-free `base`
/// (pushed, as before) plus a constant `coeff` (carried in the [`ReduceLoop`] and applied per
/// worker to its own `i`) sidesteps that entirely.
///
/// Deliberately narrow: only the forms whose base is an EXISTING subexpression, so the base is
/// still checked by the same [`is_idempotent`] gate as before and no expression is synthesized
/// (a synthesized `Binary` base would fail `is_idempotent` anyway). `i * i`, `arr[i]`, `i / 2`,
/// and `i - k` are not affine here and decline exactly as they do today.
fn affine_in<'e>(e: &'e Expr, binder: &str) -> Option<(i64, Option<&'e Expr>)> {
    // The common case: no mention of the binder at all → a constant (in `i`) bound. This is the
    // rectangular range, and it keeps the previous behavior bit-for-bit.
    if !expr_mentions(e, binder) {
        return Some((0, Some(e)));
    }
    match e {
        // `i` alone → 1*i + 0.
        Expr::Ident { name, .. } if name == binder => Some((1, None)),
        Expr::Binary { op, left, right, .. } => match op {
            // `<affine> + <i-free>` / `<i-free> + <affine>`. Exactly one side may mention the
            // binder — otherwise the base would have to fuse two subexpressions (a synthesized
            // node), which `is_idempotent` rejects regardless.
            BinOp::Add => {
                if !expr_mentions(right, binder) {
                    let (c, base) = affine_in(left, binder)?;
                    // The inner base must be the empty `0` for the outer base to be `right`.
                    base.is_none().then_some((c, Some(right.as_ref())))
                } else if !expr_mentions(left, binder) {
                    let (c, base) = affine_in(right, binder)?;
                    base.is_none().then_some((c, Some(left.as_ref())))
                } else {
                    None
                }
            }
            // `k * i` / `i * k` for a literal `k` → k*i + 0. A non-literal (runtime) coefficient
            // would have to ride as an operand; not worth it until a kernel needs it.
            BinOp::Mul => match (left.as_ref(), right.as_ref()) {
                (Expr::Int(k), Expr::Ident { name, .. }) if name == binder => Some((*k, None)),
                (Expr::Ident { name, .. }, Expr::Int(k)) if name == binder => Some((*k, None)),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn is_idempotent(e: &Expr) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Missing => true,
        Expr::Ident { .. } => true,
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().all(is_idempotent),
        Expr::Record(fields) => fields.iter().all(|(_, v)| is_idempotent(v)),
        // NOTE: `range(...)` is deliberately NOT idempotent here. A top-level range
        // *source* never reaches this fn (it's intercepted by `builtin_range_call`, which
        // also handles a user shadow); this fn is only consulted for a reduce `init` or a
        // nested Array/Tuple/Record element, where a `range(...)` yields an array that
        // never matches the Int/Float-init kernel dispatch — so admitting it bought no
        // fusion yet risked double-evaluating a side-effecting user `fn range` on the
        // native-attempt + fall-through path (an oracle divergence).
        _ => false,
    }
}

impl super::Compiler {
    /// The JIT-eligible user-function names as a borrowed set, for the kernel eligibility
    /// checks (a kernel body may call these).
    fn jit_fn_set(&self) -> std::collections::HashSet<&str> {
        self.jit_fns.iter().map(String::as_str).collect()
    }

    /// All user-defined function names — so a kernel's inline float builtins
    /// (`sqrt`/`abs`/`min`/`max`) are recognized only when not shadowed by a user fn.
    /// Emit a reduce body followed by the store back into the accumulator slot — the tail of
    /// every fold's loop body, shared by the range and array sources so they cannot drift.
    ///
    /// Takes the in-place path when the body is exactly `acc.concat(e)` with `acc` the fold's
    /// own accumulator binder. `e` is compiled FIRST, so it still sees the live accumulator
    /// (`acc.concat([acc.count()])` is fine, and merely declines the fast path at run time
    /// because the `Rc` is then shared). Any other body — including one that merely mentions
    /// `concat` somewhere inside it — takes the ordinary path unchanged.
    #[allow(clippy::too_many_arguments)] // as its neighbours: the fold's parts are all distinct
    fn emit_reduce_body_and_store(
        &mut self,
        b: &mut Builder,
        body: &Expr,
        pa: &str,
        pb: &str,
        acc: u32,
        line: usize,
        col: usize,
    ) -> R<()> {
        // `pa != pb` is a SOUNDNESS guard, not tidiness (ADR 0029 plan, fix #0).
        // Duplicate binders `(a, a)` are legal, last-write-wins — the walker writes `pa`
        // then `pb`, and `resolve_local` is last-declared-wins, so `a` in the body is
        // the ELEMENT. Matching the receiver by NAME against `pa` alone emitted
        // `ConcatIntoLocal(acc)` folding into the ACCUMULATOR: `[[1], [2]].reduce([],
        // (a, a) => a.concat([9]))` answered `[9, 9]` on VM/JIT while the walker
        // answered `[2, 9]` — a silent three-engine divergence, exit 0. Declining
        // costs nothing: the general path below is then identical on every engine.
        if pa != pb
            && let Expr::Method { recv, name, args, named, .. } = body
            && named.is_empty()
            && matches!(&**recv, Expr::Ident { name: n, .. } if n == pa)
            && let Some(op) = match (name.as_str(), args.len()) {
                ("concat", 1) => Some(Op::ConcatIntoLocal(acc)),
                ("insert", 2) => Some(Op::InsertIntoLocal(acc)),
                _ => None,
            }
        {
            for a in args {
                self.compile_expr(b, a)?;
            }
            b.emit(op, line, col);
            return Ok(());
        }
        self.compile_expr(b, body)?;
        b.emit(Op::StoreLocal(acc), line, col);
        Ok(())
    }

    fn user_fn_set(&self) -> std::collections::HashSet<&str> {
        self.func_names.iter().map(String::as_str).collect()
    }

    /// Build a `FusionStage` from a `map`/`filter`/`where` method, or `None` if it is not
    /// a single-binder JIT-eligible stage.
    fn fusion_stage(&self, name: &str, args: &[Expr]) -> Option<FusionStage> {
        if args.len() != 1 {
            return None;
        }
        let (params, body) = crate::interp::comprehension_params(&args[0]);
        if params.len() != 1 {
            return None;
        }
        let binder = params[0].clone();
        let fns = self.jit_fn_set();
        if name == "map" {
            crate::jit::map_kernel_eligible(body, &binder, &fns)
                .then(|| FusionStage::Map { binder, body: body.clone() })
        } else {
            // Capture-free only: a fused pipeline has no caps slice to carry them, so a
            // capturing predicate declines here and takes the standalone filter kernel.
            crate::jit::filter_kernel_eligible(body, &binder, &fns)
                .is_some_and(|c| c.is_empty())
                .then(|| FusionStage::Filter { binder, body: body.clone() })
        }
    }

    /// Detect a fuseable pipeline rooted at an outer `map`/`filter`/`where`/`reduce`
    /// method: walk the receiver chain collecting eligible single-binder stages down to
    /// an idempotent `Int` array or a `range` source. Returns `None` (→ the ordinary
    /// per-stage path) unless fusion actually removes an intermediate: a `Reduce` sink
    /// needs ≥1 stage, a `Collect` sink needs ≥2.
    pub(super) fn collect_fusion_chain<'a>(
        &self,
        b: &Builder,
        recv: &'a Expr,
        name: &str,
        args: &'a [Expr],
    ) -> Option<FusionPlan<'a>> {
        // The outer method is either the reduce sink or the last (outermost) stage.
        let mut outer_first: Vec<FusionStage> = Vec::new();
        // `recv` is rebound below: a FLOAT reduce may fold leading `map` stages into its own body
        // and then start the stage walk PAST them.
        let mut recv = recv;
        // Storage for a folded reduce body, which is a fresh `Expr` rather than a borrow of
        // `args`; the `&'a Expr` used below points either here or into `args`.
        let folded_store: Expr;
        let (sink, init): (FusionSink, Option<&'a Expr>) = if name == "reduce" {
            if args.len() != 2 {
                return None;
            }
            let (pa, pb, body) = match &args[1] {
                Expr::Lambda { params, body, .. } if params.len() == 2 => {
                    (params[0].clone(), params[1].clone(), body.as_ref())
                }
                _ => return None,
            };
            // FOLD leading `map` stages into a FLOAT reduce's body, so `xs.map(f).reduce(0.0, g)`
            // costs what the hand-composed `xs.reduce(0.0, g of f)` costs — measured 0.73s vs
            // 0.04s at n=20M, an 18x penalty for writing the pipeline the natural way.
            //
            // It has to happen HERE, before the sink is built, for two reasons. The body that
            // `reduce_jit_f64_body` validates must be the FINAL one, so the folded result gets
            // exactly the same scrutiny as a hand-written one — no second, weaker gate. And the
            // stages cannot be collected the usual way at all: `fusion_stage` builds `i64`
            // transforms, so a float map body is not an eligible stage, the walk below stops
            // immediately, and `cur` stays a method call that is not idempotent — which is why
            // this shape declined outright instead of merely fusing badly.
            //
            // Maps only: a `filter` would need an `if` in the body, which the f64 reduce analysis
            // does not admit. Capture safety mirrors the range path — a map body naming the
            // reduce's own binders would be captured by them — and anything `subst_ident` cannot
            // rewrite declines. Every decline leaves the pipeline exactly as it was.
            let mut body: &Expr = body;
            if crate::jit::is_float_acc_init(&args[0]) && matches!(&args[0], Expr::Float(_)) {
                let mut maps: Vec<(String, &Expr)> = Vec::new();
                let mut probe = recv;
                while let Expr::Method { recv: inner, name: m, args: margs, .. } = probe {
                    if m.as_str() != "map" || margs.len() != 1 {
                        break;
                    }
                    let (ps, mbody) = crate::interp::comprehension_params(&margs[0]);
                    if ps.len() != 1 {
                        break;
                    }
                    maps.push((ps[0].clone(), mbody));
                    probe = inner;
                }
                if !maps.is_empty() {
                    // `maps` is outermost-first, so compose in reverse: the element flows through
                    // the innermost map first.
                    let mut composed = Expr::Ident { name: pb.clone(), line: 0, col: 0 };
                    let mut ok = true;
                    for (binder, mbody) in maps.iter().rev() {
                        if expr_mentions(mbody, &pa) || expr_mentions(mbody, &pb) {
                            ok = false;
                            break;
                        }
                        match crate::jit::subst_ident(mbody, binder, &composed) {
                            Some(next) => composed = next,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok && let Some(folded) = crate::jit::subst_ident(body, &pb, &composed) {
                        folded_store = folded;
                        body = &folded_store;
                        recv = probe;
                    }
                }
            }
            // The init literal's type fixes the accumulator type — and they must not
            // compete: `reduce_jit_bodies` keys only off the body shape (`acc + x*x` is
            // structurally i64-eligible), so a `Float` init would otherwise be claimed as
            // an i64 kernel that can never match its `Float` array at dispatch (silent
            // fallback). A `0.0`-style init → the scalar `f64` kernel: a single body
            // folding a `Float` array left-to-right, which native `fadd`/`fmul` reproduce
            // bit-for-bit (`.reduce` is naive, unlike compensated `.sum`/`.mean`). The
            // f64 kernel is array-source + 0-stages only — enforced in the `enough` gate.
            if crate::jit::is_float_acc_init(&args[0]) {
                // A `Float` init (scalar) or all-`Float` tuple/record init → an `f64`
                // accumulator over a `Float`-array element (`pb_is_int = false`). The VM
                // dispatches it only on a `Floats` source; an `Ints` source falls back.
                let user_fns = self.user_fn_set();
                let bodies = if matches!(&args[0], Expr::Float(_)) {
                    vec![crate::jit::reduce_jit_f64_body(&args[0], body, &pa, &pb, &user_fns)?]
                } else {
                    crate::jit::reduce_jit_f64_tuple_bodies(&args[0], body, &pa, &pb, false, &user_fns)?
                };
                (FusionSink::Reduce { pa, pb, bodies, float: true }, Some(&args[0]))
            } else {
                let bodies =
                    crate::jit::reduce_jit_bodies(&args[0], body, &pa, &pb, &self.jit_fn_set())?;
                (FusionSink::Reduce { pa, pb, bodies, float: false }, Some(&args[0]))
            }
        } else if name == "count" {
            if !args.is_empty() {
                return None;
            }
            (FusionSink::Count, None)
        } else {
            outer_first.push(self.fusion_stage(name, args)?);
            (FusionSink::Collect, None)
        };

        // Walk inward, collecting stages (outermost-first) until a non-stage receiver.
        let mut cur = recv;
        while let Expr::Method { recv: inner, name: m, args: margs, .. } = cur {
            if !matches!(m.as_str(), "map" | "filter" | "where") {
                break;
            }
            match self.fusion_stage(m, margs) {
                Some(stage) => {
                    outer_first.push(stage);
                    cur = inner;
                }
                None => break,
            }
        }

        // Pipeline order is innermost→outermost (the reverse of how we collected).
        let mut stages = outer_first;
        stages.reverse();

        let (source, source_is_range) = if let Some((start, end)) = self.builtin_range_call(b, cur) {
            (FusionSourceExpr::Range(start, end), true)
        } else if matches!(cur, Expr::Call { name, .. } if name == "range") {
            // `range` is shadowed by a user binding - don't fuse; the per-stage
            // path compiles the user call, matching the tree-walker.
            return None;
        } else if is_idempotent(cur) {
            (FusionSourceExpr::Array(cur), false)
        } else {
            return None;
        };
        // A range source has no array to collect into; `Reduce`/`Count` still fuse.
        if source_is_range && matches!(sink, FusionSink::Collect) {
            return None;
        }
        if let Some(i) = init
            && !is_idempotent(i)
        {
            return None;
        }
        let enough = match &sink {
            // A `reduce` over an ARRAY runs as the native array→reduce kernel even with no
            // stages (`ys.reduce(0, (a,x) => a+x*x)`) — there's no intermediate to remove,
            // but the native loop still beats per-element VM dispatch (~75-100×). A bare
            // `range(...).reduce(...)` (0 stages) is handled by `compile_reduce_range`
            // instead, so a range source here still needs ≥1 stage to be worth fusing.
            // The f64 reduce kernel loads `f64` elements straight from a `Float` array,
            // so it requires an array source and no element-transforming stages.
            FusionSink::Reduce { float: true, .. } => !source_is_range && stages.is_empty(),
            FusionSink::Reduce { .. } => !stages.is_empty() || !source_is_range,
            FusionSink::Collect => stages.len() >= 2,
            // `count` only benefits when a filter actually drops elements (a map-only
            // chain's count is just the length).
            FusionSink::Count => stages.iter().any(|s| matches!(s, FusionStage::Filter { .. })),
        };
        if !enough {
            return None;
        }
        let kernel = FusedKernel { source_is_range, stages, sink };
        Some(FusionPlan { source, init, kernel })
    }

    /// Emit a fused pipeline: push the source (and `reduce` init) operands, a
    /// `TryJitFused` guard, then the ordinary per-stage chain as the fall-through (the
    /// guard discards the operands and the chain recompiles the idempotent source). The
    /// native path produces the result and jumps past the fall-through. `orig` is the
    /// whole outer method expression, recompiled (with fusion suppressed) as the
    /// fall-through.
    pub(super) fn compile_fused(
        &mut self,
        b: &mut Builder,
        orig: &Expr,
        plan: FusionPlan,
        line: usize,
        col: usize,
    ) -> R<()> {
        let FusionPlan { source, init, kernel } = plan;
        let kernel_idx = self.fused_kernels.len() as u32;
        self.fused_kernels.push(kernel);

        match source {
            FusionSourceExpr::Array(e) => self.compile_expr(b, e)?,
            FusionSourceExpr::Range(start, end) => {
                match start {
                    None => {
                        let c0 = b.add_const(Value::Int(0));
                        b.emit(Op::Const(c0), line, col);
                    }
                    Some(e) => self.compile_expr(b, e)?,
                }
                self.compile_expr(b, end)?;
            }
        }
        if let Some(i) = init {
            self.compile_expr(b, i)?;
        }
        let at = b.emit(Op::TryJitFused { kernel_idx, after: 0 }, line, col);

        // Fall-through: recompile the whole chain (any sink), with fusion suppressed so it
        // does not re-detect itself. (The single-stage kernels still apply within it.)
        self.no_fuse = true;
        let r = self.compile_expr(b, orig);
        self.no_fuse = false;
        r?;

        let after = b.code.len() as u32;
        b.code[at] = Op::TryJitFused { kernel_idx, after };
        Ok(())
    }
}

/// The synthetic counter binder for a fused map→reduce body. `$`-prefixed, so it cannot collide
/// with any user identifier (the lexer never produces one) — which is what makes the
/// substitution capture-free with no renaming pass: nothing in `g` can be captured by it.
const MR_COUNTER: &str = "$mrc";
/// The synthetic name for a fused map→reduce's accumulator SLOT. Synthetic for a concrete
/// reason: declaring the slot under the user's own accumulator name would shadow an outer
/// variable of that name for the fall-through expression.
const MR_ACC: &str = "$mracc";

/// Conservative "does `name` appear as an identifier anywhere in `e`?" — used to keep the
/// nested-reduce operands (`is`/`ie`/`init`/`oe`) independent of the outer binder, so they can
/// be hoisted out of the parallel loop and evaluated once. Ignores shadowing (a shadowed
/// occurrence still returns `true`) and treats any expression shape it doesn't walk into as a
/// possible mention (`_ => true`) — both err toward declining the parallel path, never toward
/// wrongly hoisting an `i`-dependent operand.
fn expr_mentions(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) => false,
        Expr::Ident { name: n, .. } => n == name,
        Expr::Binary { left, right, .. } => expr_mentions(left, name) || expr_mentions(right, name),
        Expr::Unary { expr, .. } => expr_mentions(expr, name),
        Expr::Call { args, .. } => args.iter().any(|a| expr_mentions(a, name)),
        Expr::Method { recv, args, .. } => {
            expr_mentions(recv, name) || args.iter().any(|a| expr_mentions(a, name))
        }
        Expr::Index { recv, index, .. } => expr_mentions(recv, name) || expr_mentions(index, name),
        Expr::If { cond, then_branch, else_branch, .. } => {
            expr_mentions(cond, name) || expr_mentions(then_branch, name) || expr_mentions(else_branch, name)
        }
        // Any other shape: conservatively assume it might mention the binder → decline.
        _ => true,
    }
}

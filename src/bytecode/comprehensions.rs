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
        let kind = if name == "map" { CompKind::Map } else { CompKind::Filter };

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
        let captures: Option<Vec<String>> = if params.len() != 1 {
            None
        } else if is_map {
            crate::jit::map_kernel_captures(body, &params[0], &fns)
                .or_else(|| crate::jit::map_kernel_captures_f64(body, &params[0], &user_fns))
                // A **mixed** `Int`-source → `Float` body the i64/f64 analyses both reject —
                // e.g. `(it % 97) * 1.0` (integer `%`/`//`/bitwise/shift subexpression, float
                // root). Capture-free; storing the kernel lets the JIT build the mixed
                // specialization the VM dispatches for an `Int` source, instead of the whole
                // map falling to the per-element bytecode loop.
                .or_else(|| crate::jit::mixed_map_eligible(body, &params[0], &user_fns).then(Vec::new))
        } else if crate::jit::filter_kernel_eligible(body, &params[0], &fns) {
            Some(Vec::new())
        } else {
            None
        };
        let kernel_guard: Option<(usize, u32)> = if let Some(caps) = captures {
            // Push each captured value (in capture order) just above the receiver array;
            // the VM pops them off whether or not it takes the native kernel.
            for cap in &caps {
                self.compile_expr(b, &Expr::Ident { name: cap.clone(), line, col })?;
            }
            let kernel =
                ArrayKernel { binder: params[0].clone(), body: body.clone(), captures: caps };
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
            if matches!(kind, CompKind::Map) { Op::CompMapPush } else { Op::CompFilterPush },
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
        let (jit_bodies, captures, float): (Option<Vec<Expr>>, Vec<Capture>, bool) =
            if crate::jit::is_float_acc_init(init) {
                // A `Float` init (scalar) or all-`Float` tuple/record init → an `f64`
                // accumulator folded over the i64 counter (`pb_is_int = true`). A scalar body
                // may index captured `f64` arrays by the counter (the float dot-product,
                // v1b) — try that first, then the capture-free scalar/tuple forms.
                let user_fns = self.user_fn_set();
                if matches!(init, Expr::Float(_)) {
                    match crate::jit::reduce_jit_f64_range_captures(init, body, pa, pb, &user_fns) {
                        Some((b, caps)) => (Some(vec![b]), caps, true),
                        None => match crate::jit::reduce_jit_f64_range_body(init, body, pa, pb, &user_fns) {
                            Some(b) => (Some(vec![b]), Vec::new(), true),
                            None => (None, Vec::new(), false),
                        },
                    }
                } else {
                    match crate::jit::reduce_jit_f64_tuple_bodies(init, body, pa, pb, true, &user_fns) {
                        Some(bodies) => (Some(bodies), Vec::new(), true),
                        None => (None, Vec::new(), false),
                    }
                }
            } else {
                match crate::jit::reduce_jit_bodies(init, body, pa, pb, &fns) {
                    Some(bodies) => (Some(bodies), Vec::new(), false),
                    None => match crate::jit::reduce_loop_captures(body, pa, pb, &fns) {
                        Some(caps) if !caps.is_empty() => (Some(vec![body.clone()]), caps, false),
                        _ => (None, Vec::new(), false),
                    },
                }
            };
        let acc;
        let x;
        let guard;
        if let Some(bodies) = jit_bodies {
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
                self.compile_expr(b, &Expr::Ident { name: cap.name.clone(), line, col })?;
            }
            let loop_idx = self.reduce_loops.len() as u32;
            self.reduce_loops.push(ReduceLoop {
                pa: pa.to_string(),
                pb: pb.to_string(),
                bodies,
                captures,
                float,
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
        self.compile_expr(b, body)?;
        b.emit(Op::StoreLocal(acc), line, col);
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
        self.compile_expr(b, body)?;
        b.emit(Op::StoreLocal(acc), line, col);
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
            crate::jit::filter_kernel_eligible(body, &binder, &fns)
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

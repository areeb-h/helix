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
        let kernel_guard: Option<(usize, u32)> = if params.len() == 1
            && (if is_map {
                crate::jit::map_kernel_eligible(body, &params[0])
            } else {
                crate::jit::filter_kernel_eligible(body, &params[0])
            }) {
            let kernel = ArrayKernel { binder: params[0].clone(), body: body.clone() };
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
        let next_at = b.emit(Op::CompNext(binder, 0), line, col);
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
        b.code[next_at] = Op::CompNext(binder, end_at);
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
        let next_at = b.emit(Op::CompNext(binder, 0), line, col);
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
        b.code[next_at] = Op::CompNext(binder, exhausted);
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
        let eligible = crate::jit::reduce_loop_eligible(body, pa, pb);
        let acc;
        let x;
        let guard;
        if eligible {
            self.compile_expr(b, init)?; // stack: [start, end, init]
            acc = b.declare_local(pa);
            x = b.declare_local(pb);
            b.emit(Op::StoreLocal(acc), line, col); // stack: [start, end]; acc=init
            let loop_idx = self.reduce_loops.len() as u32;
            self.reduce_loops.push(ReduceLoop {
                pa: pa.to_string(),
                pb: pb.to_string(),
                body: body.clone(),
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
        let next_at = b.emit(Op::CompNext(x, 0), line, col);
        self.compile_expr(b, body)?;
        b.emit(Op::StoreLocal(acc), line, col);
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(x, end_at);
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
        if let Some((start, end)) = as_range_call(recv) {
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
        let next_at = b.emit(Op::CompNext(x, 0), line, col);
        self.compile_expr(b, body)?;
        b.emit(Op::StoreLocal(acc), line, col);
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(x, end_at);
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
}

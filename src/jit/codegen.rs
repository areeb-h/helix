//! Cranelift codegen — the GATED half of the JIT (ADR 0032). Everything here
//! emits or finalizes native code; the eligibility analysis it consults lives
//! ungated in `super::analysis`, so bytecode never differs across builds.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F64, I8, I64};
use cranelift_codegen::ir::{AbiParam, Block, InstBuilder, MemFlags, Type, Value as ClValue};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::ast::{BinOp, Expr, Stmt, UnOp};
use crate::bytecode::CaptureKind;

#[allow(unused_imports)]
use super::*;
use super::analysis::{
    bodies_eligible, eligible_set, float_reduce_body_eligible, infer_f64_typed, mixed_fn_sig,
    recursive_funcs, reduce_bodies_eligible, reduce_multiacc_term, tail_loop_captures,
    tail_loopable_set, MixedSig, ACC_IDENTS,
};


/// Compile every eligible numeric function — and every JIT-eligible `reduce` loop
/// the compiler flagged — to native code, or `None` if nothing is eligible (so the
/// caller skips JIT setup).
pub fn build(
    program: &[Stmt],
    reduce_loops: &[crate::bytecode::ReduceLoop],
    map_kernels: &[crate::bytecode::ArrayKernel],
    filter_kernels: &[crate::bytecode::ArrayKernel],
    fused_kernels: &[crate::bytecode::FusedKernel],
    scan_loops: &[crate::bytecode::ReduceLoop],
) -> Option<Jit> {
    // The native call transmutes Cranelift output to `extern "C"`, which matches
    // the convention we force (SystemV) only on x86-64 Linux. On every other
    // target, decline to JIT and let the VM run everything — correct, just not
    // native. (This is the conservative fix for the ABI-soundness hazard.)
    if !cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        return None;
    }

    let funcs: Vec<FnDef> = program
        .iter()
        .filter_map(|s| match s {
            Stmt::Func { name, params, body, .. } => Some(FnDef { name, params, body }),
            _ => None,
        })
        .collect();
    if funcs.is_empty()
        && reduce_loops.is_empty()
        && map_kernels.is_empty()
        && filter_kernels.is_empty()
        && fused_kernels.is_empty()
        && scan_loops.is_empty()
    {
        return None;
    }

    let mut module = make_module()?;
    let mut compiled: Vec<(String, NumKind, FuncId, usize)> = Vec::new();

    // Only the i64 specialization is type-safe: with all-`Int` args every op (and
    // returning a param) yields `Int`, exactly matching the interpreter. The f64
    // specialization always returns `Float`, but a float-arg function can still
    // produce an `Int` (a literal, or an Int-only subexpression) — so f64 codegen
    // would diverge from the interpreter on result type. Float functions run on
    // the VM instead (correct, and they're not hot after recursion is excluded).
    let kind = NumKind::Int;
    let int_eligible = eligible_set(&funcs, kind);
    // Tail-self-recursive members of `int_eligible` compile as native loops (their
    // recursion exclusion is lifted in `eligible_set` via this same pure predicate).
    let tail_loop = tail_loopable_set(&funcs);
    // All user-function names — so the f64 kernel's inline builtins (`sqrt`/`abs`/`min`/
    // `max`) are recognized only when not shadowed by a user function of the same name.
    let user_fns: HashSet<&str> = funcs.iter().map(|f| f.name).collect();
    // Eligible user functions, declared first so kernels and other functions can call
    // them. Kept alive for the kernel-compilation blocks below.
    let mut fn_ids: HashMap<&str, FuncId> = HashMap::new();
    if !int_eligible.is_empty() {
        for f in &funcs {
            if int_eligible.contains(f.name) {
                let mut sig = module.make_signature();
                // Force SystemV to match the `extern "C"` transmute on x86-64 Linux.
                sig.call_conv = CallConv::SystemV;
                for _ in f.params {
                    sig.params.push(AbiParam::new(kind.cl_type()));
                }
                sig.returns.push(AbiParam::new(kind.cl_type()));
                let id = module
                    .declare_function(&format!("{}${}", f.name, kind.suffix()), Linkage::Local, &sig)
                    .ok()?;
                fn_ids.insert(f.name, id);
            }
        }

        let mut ctx = module.make_context();
        let mut bctx = FunctionBuilderContext::new();
        for f in &funcs {
            if !int_eligible.contains(f.name) {
                continue;
            }
            ctx.func.signature.call_conv = CallConv::SystemV;
            for _ in f.params {
                ctx.func.signature.params.push(AbiParam::new(kind.cl_type()));
            }
            ctx.func.signature.returns.push(AbiParam::new(kind.cl_type()));

            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut bctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            builder.seal_block(entry);

            let mut vars: HashMap<&str, Variable> = HashMap::new();
            for (i, (pname, _)) in f.params.iter().enumerate() {
                let pv = builder.block_params(entry)[i];
                let var = builder.declare_var(kind.cl_type());
                builder.def_var(var, pv);
                vars.insert(pname.as_str(), var);
            }

            if tail_loop.contains(f.name) {
                // Tail-self-recursive: lower as a native LOOP. Every tail self-call
                // rebinds the parameter Variables and jumps back to `hdr` — no native
                // stack growth, exactly the VM's `TailCallFn` frame reuse. `hdr` stays
                // unsealed until the body is generated (back-edges pending); Cranelift's
                // seal-late Variable mechanism builds the loop phis.
                let param_vars: Vec<Variable> =
                    f.params.iter().map(|(p, _)| vars[p.as_str()]).collect();
                let hdr = builder.create_block();
                let exit = builder.create_block();
                let ret = builder.declare_var(kind.cl_type());
                // Dominating default so `exit`'s `use_var` is defined even for a body
                // whose every path re-loops (`fn f(n) = f(n)` — which then spins exactly
                // like the VM's TailCallFn would; this value is never read).
                let zero = match kind {
                    NumKind::Int => builder.ins().iconst(I64, 0),
                    NumKind::Float => builder.ins().f64const(0.0),
                };
                builder.def_var(ret, zero);
                builder.ins().jump(hdr, &[]);
                builder.switch_to_block(hdr);
                let tl = TailLoop { self_name: f.name, params: &param_vars, hdr, exit, ret };
                gen_tail(&mut builder, f.body, &mut vars, &fn_ids, &mut module, kind, &tl);
                builder.seal_block(hdr);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                let rv = builder.use_var(ret);
                builder.ins().return_(&[rv]);
            } else {
                let ret = gen_value(&mut builder, f.body, &mut vars, &fn_ids, &mut module, kind);
                builder.ins().return_(&[ret]);
            }
            builder.finalize();

            module.define_function(fn_ids[f.name], &mut ctx).ok()?;
            module.clear_context(&mut ctx);

            compiled.push((f.name.to_string(), kind, fn_ids[f.name], f.params.len()));
        }
    }

    // TAIL LOOPS THAT READ A GLOBAL. `value_eligible`'s `Ident` arm admits only
    // parameters, so one global read anywhere in a tail-recursive function dropped the
    // whole loop to the bytecode VM — measured at 10M iterations, 0.01s compiled against
    // 0.80s interpreted. These compile with the globals appended as trailing `i64`
    // parameters, which the VM reads and marshals at dispatch.
    //
    // Like the MIXED specializations below, they are dispatched ONLY from the VM's
    // `CallFn`: they live outside `fn_ids` / `int_eligible`, so no kernel and no other
    // native can call them with the wrong signature, and `int_eligible_fns` — the
    // bytecode compiler's view of what a kernel may call — is untouched.
    let mut compiled_caps: Vec<(String, FuncId, Vec<String>, usize)> = Vec::new();
    {
        let cap_loops = tail_loop_captures(&funcs, &int_eligible, kind);
        if !cap_loops.is_empty() {
            let mut ctx = module.make_context();
            let mut bctx = FunctionBuilderContext::new();
            for (fname, caps) in &cap_loops {
                let Some(f) = funcs.iter().find(|g| g.name == *fname) else {
                    continue;
                };
                let nparams = f.params.len();
                let total = nparams + caps.len();
                let mut sig = module.make_signature();
                sig.call_conv = CallConv::SystemV;
                for _ in 0..total {
                    sig.params.push(AbiParam::new(kind.cl_type()));
                }
                sig.returns.push(AbiParam::new(kind.cl_type()));
                let id = module
                    .declare_function(&format!("{fname}$caploop"), Linkage::Local, &sig)
                    .ok()?;

                ctx.func.signature.call_conv = CallConv::SystemV;
                for _ in 0..total {
                    ctx.func.signature.params.push(AbiParam::new(kind.cl_type()));
                }
                ctx.func.signature.returns.push(AbiParam::new(kind.cl_type()));

                let mut builder = FunctionBuilder::new(&mut ctx.func, &mut bctx);
                let entry = builder.create_block();
                builder.append_block_params_for_function_params(entry);
                builder.switch_to_block(entry);
                builder.seal_block(entry);

                // Parameters first, then the captures — the order `tail_loop_captures`
                // returned them in, which is the order the VM marshals.
                let mut vars: HashMap<&str, Variable> = HashMap::new();
                let mut param_vars: Vec<Variable> = Vec::with_capacity(nparams);
                for i in 0..total {
                    let pv = builder.block_params(entry)[i];
                    let var = builder.declare_var(kind.cl_type());
                    builder.def_var(var, pv);
                    let name: &str =
                        if i < nparams { f.params[i].0.as_str() } else { caps[i - nparams] };
                    vars.insert(name, var);
                    if i < nparams {
                        param_vars.push(var);
                    }
                }

                let hdr = builder.create_block();
                let exit = builder.create_block();
                let ret = builder.declare_var(kind.cl_type());
                let zero = match kind {
                    NumKind::Int => builder.ins().iconst(I64, 0),
                    NumKind::Float => builder.ins().f64const(0.0),
                };
                builder.def_var(ret, zero);
                builder.ins().jump(hdr, &[]);
                builder.switch_to_block(hdr);
                // `params` is the REAL parameters only. `gen_tail`'s back-edge zips the
                // self-call's arguments against this slice, so the trailing capture
                // variables are never rebound — which is exactly right: a global cannot
                // change while native code is running, so a capture is loop-invariant.
                let tl = TailLoop { self_name: f.name, params: &param_vars, hdr, exit, ret };
                gen_tail(&mut builder, f.body, &mut vars, &fn_ids, &mut module, kind, &tl);
                builder.seal_block(hdr);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                let rv = builder.use_var(ret);
                builder.ins().return_(&[rv]);
                builder.finalize();

                module.define_function(id, &mut ctx).ok()?;
                module.clear_context(&mut ctx);
                compiled_caps.push((
                    fname.to_string(),
                    id,
                    caps.iter().map(|c| c.to_string()).collect(),
                    nparams,
                ));
            }
        }
    }

    // MIXED (per-parameter Int/Float, from explicit annotations) tail-loop
    // specializations — the mandelbrot-class scalar loops whose state is f64 but whose
    // counter/result is i64, dispatched ONLY from the VM's `CallFn` (never from kernels
    // or other natives, so they live outside `fn_ids` / `int_eligible` and cannot
    // interfere with any existing path). The external signature is uniformly all-`i64`
    // (bits ABI — see [`MixedFn`]); the prologue bitcasts Float params, the epilogue
    // bitcasts a Float result.
    let mut compiled_mixed: Vec<(String, FuncId, u16, bool, usize)> = Vec::new();
    // Hoisted out of the block below so the MAP kernels can call these specializations too
    // (a `Float`-parameter callee inside a map body): the sigs to marshal by, and their ids.
    let mut mixed_sigs: HashMap<&str, MixedSig> = HashMap::new();
    let mut mixed_ids: HashMap<&str, FuncId> = HashMap::new();
    // The same signatures in the OWNED, name-keyed shape the map analyses take — the twin of
    // what the bytecode compiler computed via `mixed_fn_sigs`, so compile gate and build gate
    // are reading one table.
    let msig_table: MixedSigTable = mixed_fn_sigs(program);
    {
        // PHASE 1 — eligibility + declaration, in program order. Each accepted
        // function's signature enters `mixed_sigs` immediately, so a LATER mixed body
        // may call an EARLIER one (`fn escape(px: Int, py: Int) = step(…)` — the
        // define-before-use rule guarantees callees precede callers). Recursive
        // functions qualify only in the tail-loopable shape; non-recursive ones
        // compile straight-line with the same walker/codegen.
        let recursive = recursive_funcs(&funcs);
        let mut mixed_defs: Vec<(&FnDef, u16, Vec<NumKind>, NumKind)> = Vec::new();
        for f in &funcs {
            let Some((mask, param_kinds, ret_kind)) =
                mixed_fn_sig(f, &tail_loop, &recursive, &int_eligible, &mixed_sigs, &user_fns)
            else {
                continue;
            };
            // A body whose every path re-loops never returns; `Int` is a placeholder.
            let ret_kind = ret_kind.unwrap_or(NumKind::Int);
            // arity user slots + the trailing poison-pointer slot (see [`MixedFn`]).
            let n_slots = f.params.len() + 1;
            let mut sig = module.make_signature();
            sig.call_conv = CallConv::SystemV;
            for _ in 0..n_slots {
                sig.params.push(AbiParam::new(I64));
            }
            sig.returns.push(AbiParam::new(I64));
            let Ok(id) =
                module.declare_function(&format!("{}$mixed", f.name), Linkage::Local, &sig)
            else {
                continue;
            };
            mixed_sigs.insert(f.name, MixedSig { params: param_kinds.clone(), ret: ret_kind });
            mixed_ids.insert(f.name, id);
            mixed_defs.push((f, mask, param_kinds, ret_kind));
        }

        // PHASE 2 — define every declared body (each sees the FULL sig registry).
        let mut ctx = module.make_context();
        let mut bctx = FunctionBuilderContext::new();
        for (f, mask, param_kinds, ret_kind) in mixed_defs {
            let id = mixed_ids[f.name];
            let n_slots = f.params.len() + 1;
            ctx.func.signature.call_conv = CallConv::SystemV;
            for _ in 0..n_slots {
                ctx.func.signature.params.push(AbiParam::new(I64));
            }
            ctx.func.signature.returns.push(AbiParam::new(I64));

            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut bctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            builder.seal_block(entry);

            // Params: raw i64 slots; Float ones are bitcast back to f64 (pure bit move).
            let mut vars: HashMap<&str, Variable> = HashMap::new();
            let mut env: HashMap<&str, NumKind> = HashMap::new();
            let mut param_vars: Vec<Variable> = Vec::with_capacity(f.params.len());
            for (j, (pname, _)) in f.params.iter().enumerate() {
                let raw = builder.block_params(entry)[j];
                let k = param_kinds[j];
                let var = builder.declare_var(k.cl_type());
                let val = match k {
                    NumKind::Int => raw,
                    NumKind::Float => builder.ins().bitcast(F64, MemFlags::new(), raw),
                };
                builder.def_var(var, val);
                vars.insert(pname.as_str(), var);
                env.insert(pname.as_str(), k);
                param_vars.push(var);
            }

            // Same loop skeleton as the i64 tail branch (see `gen_tail`): unsealed
            // header, dominating ret default, back-edges rebind the param Variables.
            // Plus the poison machinery: the trailing param is the `*mut i8` poison
            // pointer; every float comparison bails to `poison_blk` on an unordered
            // (NaN) operand, which stores 1 and returns — the VM then discards the
            // result and re-runs on bytecode, raising the interpreter's exact error.
            let poison_ptr = builder.block_params(entry)[f.params.len()];
            let hdr = builder.create_block();
            let exit = builder.create_block();
            let poison_blk = builder.create_block();
            let ret = builder.declare_var(ret_kind.cl_type());
            let zero = match ret_kind {
                NumKind::Int => builder.ins().iconst(I64, 0),
                NumKind::Float => builder.ins().f64const(0.0),
            };
            builder.def_var(ret, zero);
            builder.ins().jump(hdr, &[]);
            builder.switch_to_block(hdr);
            let tl = MixedTail {
                self_name: f.name,
                params: &param_vars,
                param_kinds: &param_kinds,
                hdr,
                exit,
                ret,
                poison_blk,
                poison_ptr,
                sigs: &mixed_sigs,
                ids: &mixed_ids,
            };
            gen_tail_mixed(&mut builder, f.body, &mut vars, &mut env, &mut module, &tl);
            builder.seal_block(hdr);
            // poison_blk: unreachable when the body has no float comparisons — filled
            // regardless (Cranelift accepts filled unreachable blocks, as with the
            // all-paths-loop exit in the i64 tail branch).
            builder.switch_to_block(poison_blk);
            builder.seal_block(poison_blk);
            let one8 = builder.ins().iconst(I8, 1);
            builder.ins().store(MemFlags::trusted(), one8, poison_ptr, 0);
            let z64 = builder.ins().iconst(I64, 0);
            builder.ins().return_(&[z64]);
            builder.switch_to_block(exit);
            builder.seal_block(exit);
            let rv = builder.use_var(ret);
            let out = match ret_kind {
                NumKind::Int => rv,
                NumKind::Float => builder.ins().bitcast(I64, MemFlags::new(), rv),
            };
            builder.ins().return_(&[out]);
            builder.finalize();

            module.define_function(id, &mut ctx).ok()?;
            module.clear_context(&mut ctx);
            compiled_mixed.push((
                f.name.to_string(),
                id,
                mask,
                ret_kind == NumKind::Float,
                f.params.len(),
            ));
        }
    }

    // Compile each flagged `reduce` loop into a native `fn(i64,i64,i64)->i64`. We
    // re-check eligibility defensively: if a site somehow isn't compilable, its
    // slot stays `None` and the VM keeps running the bytecode loop for it.
    let mut reduce_ids: Vec<Option<FuncId>> = Vec::with_capacity(reduce_loops.len());
    {
        let mut ctx = module.make_context();
        let mut bctx = FunctionBuilderContext::new();
        for (i, rl) in reduce_loops.iter().enumerate() {
            if !reduce_bodies_eligible(rl, &int_eligible, &user_fns, &msig_table) {
                reduce_ids.push(None);
                continue;
            }
            // Both shapes take 3 params (start, end, and `init` for a scalar acc or an
            // `acc_ptr` for a tuple acc); a scalar returns the accumulator, a tuple writes
            // its slots back through the pointer (no return). A **scalar f64** reduce takes
            // its `init` and returns its result as `f64` (the i64 counter is still i64).
            let float = rl.bodies.len() == 1 && rl.float;
            let mut sig = module.make_signature();
            sig.call_conv = CallConv::SystemV;
            sig.params.push(AbiParam::new(I64)); // start
            sig.params.push(AbiParam::new(I64)); // end
            sig.params.push(AbiParam::new(if float { F64 } else { I64 })); // init (f64) / acc_ptr
            if rl.bodies.len() == 1 {
                sig.returns.push(AbiParam::new(if float { F64 } else { I64 }));
            }
            let id = match module.declare_function(&format!("reduce${i}"), Linkage::Local, &sig) {
                Ok(id) => id,
                Err(_) => {
                    reduce_ids.push(None);
                    continue;
                }
            };
            let mixed_tables = MixedTables { sigs: &msig_table, ids: &mixed_ids };
            match define_reduce_loop(&mut module, &mut ctx, &mut bctx, id, rl, &fn_ids, &mixed_tables) {
                Some(()) => reduce_ids.push(Some(id)),
                None => reduce_ids.push(None),
            }
        }
    }

    // Compile each flagged `map`/`filter` body and fuseable pipeline into a native
    // kernel. Same defensive re-check + per-site `None` slot as the reduce loops; kernel
    // bodies may call the eligible functions in `fn_ids`.
    // `map` compiles two specializations — `i64` (Int source) and `f64` (Float source);
    // the VM picks by the array's element type at runtime. `filter`/fused stay `Int`.
    let map_ids = define_array_kernels(
        &mut module, map_kernels, "map", false, &fn_ids, &int_eligible, &user_fns, NumKind::Int,
        None, false, &msig_table, &mixed_ids,
    );
    let map_f64_ids = define_array_kernels(
        &mut module, map_kernels, "mapf", false, &fn_ids, &int_eligible, &user_fns, NumKind::Float,
        None, false, &msig_table, &mixed_ids,
    );
    // The mixed `Int`-source → `Float` specialization (`range.map(j => j*0.001)`): reads
    // `i64`, writes `f64`. `elem_kind` is ignored when mixed (the body is typed per node).
    let map_mixed_ids = define_array_kernels(
        &mut module, map_kernels, "mapm", false, &fn_ids, &int_eligible, &user_fns, NumKind::Int,
        Some(NumKind::Float), false, &msig_table, &mixed_ids,
    );
    // Its VALUE-SCALAR variant: the same stored kernels, with captures riding as f64 bits —
    // dispatched when a runtime `Float` capture makes the Int-proven marshal decline.
    let map_mixed_value_ids = define_array_kernels(
        &mut module, map_kernels, "mapmv", false, &fn_ids, &int_eligible, &user_fns, NumKind::Int,
        Some(NumKind::Float), true, &msig_table, &mixed_ids,
    );
    // The Int-ROOTED mixed specialization: i64 in, i64 OUT, Float intermediates
    // (`to_int(to_float(i) * 1.5)`). Same ABI as the plain i64 kernel, so it rides the same
    // FFI wrappers, dispatch marshalling, and in-place reuse.
    let map_mixed_int_ids = define_array_kernels(
        &mut module, map_kernels, "mapmi", false, &fn_ids, &int_eligible, &user_fns, NumKind::Int,
        Some(NumKind::Int), false, &msig_table, &mixed_ids,
    );
    let filter_ids = define_array_kernels(
        &mut module, filter_kernels, "filter", true, &fn_ids, &int_eligible, &user_fns, NumKind::Int,
        None, false, &msig_table, &mixed_ids,
    );
    // The f64 (`Floats`-source) filter specialization — the same dual-build pattern as
    // "map"/"mapf", from the same stored kernels. The predicate compiles under the
    // `F64Proof` comparison subset; a NaN meeting an ordering comparison poisons at run
    // time (the kernel returns -1) and the dispatch falls back to the bytecode loop for
    // the interpreter's exact error.
    let filter_f64_ids = define_array_kernels(
        &mut module, filter_kernels, "filterf", true, &fn_ids, &int_eligible, &user_fns, NumKind::Float,
        None, false, &msig_table, &mixed_ids,
    );
    let fused_ids = define_fused_kernels(&mut module, fused_kernels, &fn_ids, &int_eligible, &user_fns);

    // `scan` (prefix-fold) loops — SERIAL kernels (see `define_scan_loop`). Same defensive
    // re-check discipline as the reduce loops: re-derive the capture list from the body with
    // the SAME collector the compiler used and require it to reproduce the stored one exactly,
    // so codegen's `caps[i]` and the VM's push order cannot drift.
    let mut scan_ids: Vec<Option<FuncId>> = Vec::with_capacity(scan_loops.len());
    {
        let mut ctx = module.make_context();
        let mut bctx = FunctionBuilderContext::new();
        for (i, rl) in scan_loops.iter().enumerate() {
            let ok = rl.bodies.len() == 1
                && !rl.float
                && rl.index_bounds.is_empty()
                && rl.captures.iter().all(|c| c.kind == CaptureKind::Scalar)
                && reduce_loop_captures(&rl.bodies[0], &rl.pa, &rl.pb, &int_eligible)
                    .is_some_and(|(caps, bnds, _)| caps == rl.captures && bnds.is_empty());
            if !ok {
                scan_ids.push(None);
                continue;
            }
            let mut sig = module.make_signature();
            sig.call_conv = CallConv::SystemV;
            for _ in 0..5 {
                sig.params.push(AbiParam::new(I64)); // start, end, init, dst, caps
            }
            let Ok(id) = module.declare_function(&format!("scan${i}"), Linkage::Local, &sig)
            else {
                scan_ids.push(None);
                continue;
            };
            match define_scan_loop(&mut module, &mut ctx, &mut bctx, id, rl, &fn_ids) {
                Some(()) => scan_ids.push(Some(id)),
                None => scan_ids.push(None),
            }
        }
    }

    if compiled.is_empty()
        && compiled_mixed.is_empty()
        && compiled_caps.is_empty()
        && reduce_ids.iter().all(|r| r.is_none())
        && map_ids.iter().all(|r| r.is_none())
        && map_f64_ids.iter().all(|r| r.is_none())
        && map_mixed_ids.iter().all(|r| r.is_none())
        && map_mixed_int_ids.iter().all(|r| r.is_none())
        && map_mixed_value_ids.iter().all(|r| r.is_none())
        && filter_ids.iter().all(|r| r.is_none())
        && filter_f64_ids.iter().all(|r| r.is_none())
        && fused_ids.iter().all(|r| r.is_none())
        && scan_ids.iter().all(|r| r.is_none())
    {
        return None;
    }
    module.finalize_definitions().ok()?;

    let mut by_name: HashMap<String, NativeFn> = HashMap::new();
    for (name, kind, id, arity) in compiled {
        let ptr = module.get_finalized_function(id);
        let entry = by_name
            .entry(name)
            .or_insert(NativeFn { i64_ptr: None, f64_ptr: None, mixed: None, arity });
        match kind {
            NumKind::Int => entry.i64_ptr = Some(ptr),
            NumKind::Float => entry.f64_ptr = Some(ptr),
        }
    }
    for (name, id, float_mask, ret_float, arity) in compiled_mixed {
        let ptr = module.get_finalized_function(id);
        let entry = by_name
            .entry(name)
            .or_insert(NativeFn { i64_ptr: None, f64_ptr: None, mixed: None, arity });
        entry.mixed = Some(MixedFn { ptr, float_mask, ret_float });
    }

    // name -> (entry point, the globals to marshal in order, real arity)
    let cap_fns: HashMap<String, (*const u8, Vec<String>, usize)> = compiled_caps
        .into_iter()
        .map(|(name, id, caps, arity)| {
            (name, (module.get_finalized_function(id), caps, arity))
        })
        .collect();

    let finalize = |ids: Vec<Option<FuncId>>, module: &JITModule| -> Vec<Option<*const u8>> {
        ids.into_iter().map(|id| id.map(|id| module.get_finalized_function(id))).collect()
    };
    let reduce_ptrs = finalize(reduce_ids, &module);
    let map_ptrs = finalize(map_ids, &module);
    let map_ptrs_f64 = finalize(map_f64_ids, &module);
    let map_ptrs_mixed = finalize(map_mixed_ids, &module);
    let map_ptrs_mixed_int = finalize(map_mixed_int_ids, &module);
    let map_ptrs_mixed_value = finalize(map_mixed_value_ids, &module);
    let filter_ptrs = finalize(filter_ids, &module);
    let filter_ptrs_f64 = finalize(filter_f64_ids, &module);
    let fused_ptrs = finalize(fused_ids, &module);
    let scan_ptrs = finalize(scan_ids, &module);

    Some(Jit {
        _module: module,
        by_name,
        cap_fns,
        reduce_ptrs,
        map_ptrs,
        map_ptrs_f64,
        map_ptrs_mixed,
        map_ptrs_mixed_int,
        map_ptrs_mixed_value,
        filter_ptrs,
        filter_ptrs_f64,
        fused_ptrs,
        scan_ptrs,
    })
}

/// All stages and the reduce sink of a fused pipeline must be JIT-eligible.
fn fusion_eligible(k: &crate::bytecode::FusedKernel, fns: &HashSet<&str>, user_fns: &HashSet<&str>) -> bool {
    use crate::bytecode::{FusionSink, FusionStage};
    k.stages.iter().all(|s| match s {
        FusionStage::Map { binder, body } => map_kernel_eligible(body, binder, fns),
        // A fused pipeline has no caps slice, so a CAPTURING predicate must decline here and
        // fall to the standalone filter kernel (which does carry captures) instead.
        FusionStage::Filter { binder, body } => {
            filter_kernel_eligible(body, binder, fns).is_some_and(|c| c.is_empty())
        }
    }) && match &k.sink {
        FusionSink::Collect | FusionSink::Count => true,
        // A float reduce is checked against the f64 subset (using `user_fns` to exclude a
        // user-shadowed `sqrt`/`min`, exactly like the f64 map path); the i64 path uses
        // `bodies_eligible`. Scalar (1 body) over `{pa, pb}`; tuple (N>1) over the f64 slots
        // `{$acc0…, pb}` (the array element `pb` is `f64`).
        FusionSink::Reduce { pa, pb, bodies, float: true } if bodies.len() == 1 => {
            float_reduce_body_eligible(&bodies[0], pa, pb, &HashSet::new(), user_fns)
        }
        FusionSink::Reduce { pb, bodies, float: true, .. } => {
            let n = bodies.len();
            (2..=MAX_ACC_SLOTS).contains(&n) && {
                let mut binders: HashMap<&str, NumKind> = HashMap::new();
                for &slot in ACC_IDENTS.iter().take(n) {
                    binders.insert(slot, NumKind::Float);
                }
                binders.insert(pb.as_str(), NumKind::Float);
                bodies.iter().all(|c| infer_f64_typed(c, &binders, user_fns) == Some(NumKind::Float))
            }
        }
        FusionSink::Reduce { pa, pb, bodies, float: false } => bodies_eligible(pa, pb, bodies, fns),
    }
}

/// Declare + define every fuseable pipeline kernel (one slot each, `None` if declined).
fn define_fused_kernels(
    module: &mut JITModule,
    kernels: &[crate::bytecode::FusedKernel],
    fn_ids: &HashMap<&str, FuncId>,
    eligible: &HashSet<&str>,
    user_fns: &HashSet<&str>,
) -> Vec<Option<FuncId>> {
    let mut ids: Vec<Option<FuncId>> = Vec::with_capacity(kernels.len());
    let mut ctx = module.make_context();
    let mut bctx = FunctionBuilderContext::new();
    for (i, k) in kernels.iter().enumerate() {
        if !fusion_eligible(k, eligible, user_fns) {
            ids.push(None);
            continue;
        }
        // A scalar **float** reduce takes its `init` as `f64` (param 2) and returns `f64`;
        // everything else is `i64`. A tuple reduce writes its slots through `acc_ptr` (no
        // return) — see `define_fused_kernel`.
        let float_reduce = matches!(&k.sink,
            crate::bytecode::FusionSink::Reduce { bodies, float: true, .. } if bodies.len() == 1);
        let tuple_reduce = matches!(&k.sink,
            crate::bytecode::FusionSink::Reduce { bodies, .. } if bodies.len() > 1);
        let mut sig = module.make_signature();
        sig.call_conv = CallConv::SystemV;
        sig.params.push(AbiParam::new(I64)); // src pointer / range start
        sig.params.push(AbiParam::new(I64)); // length / range end
        sig.params.push(AbiParam::new(if float_reduce { F64 } else { I64 })); // init (f64 for a float reduce)
        if !tuple_reduce {
            sig.returns.push(AbiParam::new(if float_reduce { F64 } else { I64 }));
        }
        let id = match module.declare_function(&format!("fused${i}"), Linkage::Local, &sig) {
            Ok(id) => id,
            Err(_) => {
                ids.push(None);
                continue;
            }
        };
        ids.push(define_fused_kernel(module, &mut ctx, &mut bctx, id, k, fn_ids).map(|()| id));
    }
    ids
}

/// Declare + define a batch of `map`/`filter` kernels, returning one slot per kernel
/// (`None` for any the JIT declined). `is_filter` selects the predicate/compaction
/// codegen and the `-> i64` (kept-count) signature.
#[allow(clippy::too_many_arguments)]
fn define_array_kernels(
    module: &mut JITModule,
    kernels: &[crate::bytecode::ArrayKernel],
    tag: &str,
    is_filter: bool,
    fn_ids: &HashMap<&str, FuncId>,
    eligible: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    elem_kind: NumKind,
    mixed_root: Option<NumKind>,
    // The VALUE-SCALAR variant of the plain mixed map ("mapmv"): captures load as `f64`
    // bits and type `Float`, admitted by the `MixT` analysis instead of the Int-proven one.
    value_scalars: bool,
    // The mixed specializations a kernel body may CALL, and their codegen identities.
    msigs: &MixedSigTable,
    mixed_ids: &HashMap<&str, FuncId>,
) -> Vec<Option<FuncId>> {
    let mut ids: Vec<Option<FuncId>> = Vec::with_capacity(kernels.len());
    let mut ctx = module.make_context();
    let mut bctx = FunctionBuilderContext::new();
    for (i, k) in kernels.iter().enumerate() {
        // Eligibility per kind: filter (Int comparison), mixed map (Int source, float body
        // — `mixed_map_eligible`), Float map (the safe `+ - *` subset over a Floats source
        // — `map_kernel_captures_f64`), or Int map (capture-aware, body re-checked so a
        // captured-var body compiles).
        // An INDEXED body (`a[it]`) is admitted by the i64 path (caps marshaled from `Ints`
        // arrays) and by the MIXED path (caps marshaled from `Floats` arrays, F64 loads,
        // f64 result) — each specialization re-derives the capture list from the body and
        // must reproduce the stored one exactly, so codegen's `caps[j]` and the VM's load
        // order cannot drift. The f64-SOURCE map and filter still decline indexed bodies:
        // their source is a data array, i.e. an element-value binder — the gather shape
        // whose bounds are undischargeable (see `ArrayKernel::index_bounds`) — and that is
        // permanent, not a v1 gap.
        let indexed = !k.index_bounds.is_empty();
        let ok = if is_filter {
            // Two filter specializations from the same stored kernel, like "map"/"mapf":
            // the i64 pass re-checks under the Int comparison subset, the f64 pass under
            // the `F64Proof` one. Each requires its re-derived capture list to equal the
            // stored one, so codegen's `caps[j]` and the VM's marshal order cannot drift.
            !indexed
                && if matches!(elem_kind, NumKind::Float) {
                    filter_kernel_eligible_f64(&k.body, &k.binder, user_fns)
                        .is_some_and(|c| c == k.captures)
                } else {
                    filter_kernel_eligible(&k.body, &k.binder, eligible)
                        .is_some_and(|c| c == k.captures)
                }
        } else if mixed_root == Some(NumKind::Float) && value_scalars {
            // The value-scalar variant: unindexed only, and the `MixT` analysis must admit
            // the body (each capture used only where a genuine float promotes it). Compared
            // by NAMES AND ORDER against the stored list — the stored kinds are the plain
            // analysis's `Scalar`s, while this specialization's are `ScalarValue`; the kinds
            // are a per-specialization loading decision, not an identity.
            !indexed
                && body_raises(&k.body, user_fns, msigs) == k.raises
                && mixed_map_value_scalar_eligible(&k.body, &k.binder, eligible, user_fns, msigs).is_some_and(
                    |c| {
                        c.len() == k.captures.len()
                            && c.iter().zip(&k.captures).all(|(a, b)| a.name == b.name)
                    },
                )
        } else if mixed_root == Some(NumKind::Float) {
            // `raises` decides the built SIGNATURE (poison out-param or not), so it gets the
            // same drift guard as the capture list: re-derive from the body and require the
            // stored flag to match, or the VM would call a 5-param kernel through a 4-param
            // wrapper.
            body_raises(&k.body, user_fns, msigs) == k.raises
                && if indexed {
                // Synthetic `$aff*` naming is a deterministic function of the body (dedup
                // by printed form), so the re-derived capture list — `$aff` slots included
                // — must equal the stored one exactly or the build declines.
                mixed_map_captures_indexed(&k.body, &k.binder, eligible, user_fns, msigs)
                    .is_some_and(|(c, bnd, _)| c == k.captures && bnd == k.index_bounds)
            } else {
                // Same drift guard as the indexed arms: re-derive the capture list and require
                // it to equal the stored one, so codegen's `caps[j]` and the VM's marshal order
                // cannot disagree.
                mixed_map_eligible(&k.body, &k.binder, eligible, user_fns, msigs)
                    .is_some_and(|c| c == k.captures)
            }
        } else if mixed_root == Some(NumKind::Int) {
            // The Int-ROOTED mixed map (i64 out through Float intermediates). The extra
            // `map_kernel_captures(..).is_none()` leg keeps it from double-compiling bodies
            // the plain i64 kernel already covers — an i64-closed body is trivially also
            // Int-rooted-mixed-eligible, and the i64 specialization wins at dispatch, so a
            // second kernel for it would be pure waste. No indexed form yet: an indexed
            // Int-rooted body has shown up in no probe, and its bounds discharge would be
            // unexercised safety code.
            !indexed
                && body_raises(&k.body, user_fns, msigs) == k.raises
                && mixed_map_int_root_eligible(&k.body, &k.binder, eligible, user_fns, msigs)
                    .is_some_and(|c| c == k.captures)
                && map_kernel_captures(&k.body, &k.binder, eligible).is_none()
        } else if matches!(elem_kind, NumKind::Float) {
            !indexed && map_kernel_captures_f64(&k.body, &k.binder, user_fns).is_some()
        } else if indexed {
            // Re-derive from the body and require the SAME capture list the compiler stored,
            // so codegen's `caps[j]` and the VM's load order cannot drift apart.
            map_kernel_captures_indexed(&k.body, &k.binder, eligible)
                .is_some_and(|(c, bnd, _)| c == k.captures && bnd == k.index_bounds)
        } else {
            map_kernel_captures(&k.body, &k.binder, eligible).is_some()
        };
        if !ok {
            ids.push(None);
            continue;
        }
        let mut sig = module.make_signature();
        sig.call_conv = CallConv::SystemV;
        for _ in 0..3 {
            sig.params.push(AbiParam::new(I64)); // src, dst, len (pointers + length: i64)
        }
        // The caps pointer is the 4th param for BOTH shapes, matching what
        // `define_array_kernel` actually builds (filter gained captures in Stage 3q; this
        // declaration previously omitted the param — harmless only because kernels are
        // reached via `get_finalized_function` + transmute, never by cross-function call).
        sig.params.push(AbiParam::new(I64)); // caps ptr (loop-invariant captures)
        if mixed_root.is_some() && k.raises {
            sig.params.push(AbiParam::new(I64)); // poison out-cell ptr (raising rounders)
        }
        if is_filter {
            sig.returns.push(AbiParam::new(I64)); // kept count
        }
        let id = match module.declare_function(&format!("{tag}${i}"), Linkage::Local, &sig) {
            Ok(id) => id,
            Err(_) => {
                ids.push(None);
                continue;
            }
        };
        let done = define_array_kernel(
            module, &mut ctx, &mut bctx, id, k, is_filter, fn_ids, elem_kind, mixed_root,
            value_scalars, msigs, mixed_ids,
        );
        ids.push(done.map(|()| id));
    }
    ids
}

/// Build the JIT module, or `None` on any setup failure (so the caller falls
/// back to the VM instead of aborting — these `unwrap`s previously turned a
/// renamed Cranelift flag or an unsupported host into a hard process abort).
fn make_module() -> Option<JITModule> {
    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").ok()?;
    flag_builder.set("is_pic", "false").ok()?;
    // Optimize generated code — the e-graph mid-end (constant folding, GVN,
    // strength reduction, branch/alias opts). Default is "none"; this is a free
    // codegen-quality win, paid once at compile time.
    flag_builder.set("opt_level", "speed").ok()?;
    let isa_builder = cranelift_native::builder().ok()?;
    let isa = isa_builder.finish(settings::Flags::new(flag_builder)).ok()?;
    let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    Some(JITModule::new(builder))
}


// ---------- codegen ----------

/// Emit the native prefix-fold loop for `range(start,end).scan(init, (pa,pb)=>body)`:
/// `acc=init; j=0; x=start; while x<end { acc=body(acc,x); dst[j]=acc; j+=1; x+=1 }`.
/// The reduce loop's i64 scalar shape, plus one store per iteration — each successive
/// accumulator lands in `dst[j]`, which is exactly what the bytecode loop's `CompMapPush`
/// collects. SERIAL by definition: element *j* depends on element *j−1*, so there is no
/// parallel form and byte-identity needs no ordering argument. Integer arithmetic wraps
/// (`iadd`/`imul`) as everywhere else, and an empty range writes nothing.
///
/// Signature: `fn(start, end, init, dst, caps)`, all `i64` (two pointers ride as `i64`).
/// `dst` has exactly `end - start` slots — the caller allocates from the SAME endpoints it
/// passes here, and the VM capped the length before dispatch. `caps` carries the
/// loop-invariant `Scalar` captures in the stored order, loaded once in the entry block.
fn define_scan_loop<'a>(
    module: &mut JITModule,
    ctx: &mut cranelift_codegen::Context,
    bctx: &mut FunctionBuilderContext,
    fid: FuncId,
    rl: &'a crate::bytecode::ReduceLoop,
    fn_ids: &HashMap<&'a str, FuncId>,
) -> Option<()> {
    ctx.func.signature.call_conv = CallConv::SystemV;
    for _ in 0..5 {
        ctx.func.signature.params.push(AbiParam::new(I64));
    }
    let mut b = FunctionBuilder::new(&mut ctx.func, bctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let p = b.block_params(entry).to_vec();
    let (start, end, init, dst, caps_ptr) = (p[0], p[1], p[2], p[3], p[4]);

    let acc = b.declare_var(I64);
    let x = b.declare_var(I64);
    let j = b.declare_var(I64);
    let dst_var = b.declare_var(I64);
    let end_var = b.declare_var(I64);
    b.def_var(acc, init);
    b.def_var(x, start);
    let zero = b.ins().iconst(I64, 0);
    b.def_var(j, zero);
    b.def_var(dst_var, dst);
    b.def_var(end_var, end);

    // Bind the binders and the hoisted capture loads. Insertion order matters for nothing —
    // codegen resolves by name — but the caps' SLOT order is the stored capture order, which
    // the build gate proved identical to what the VM pushes.
    let mut vars: HashMap<&'a str, Variable> = HashMap::new();
    for (i, cap) in rl.captures.iter().enumerate() {
        let v = b.ins().load(I64, MemFlags::trusted(), caps_ptr, (i * 8) as i32);
        let cv = b.declare_var(I64);
        b.def_var(cv, v);
        vars.insert(cap.name.as_str(), cv);
    }
    vars.insert(rl.pa.as_str(), acc);
    vars.insert(rl.pb.as_str(), x);

    let header = b.create_block();
    let body_blk = b.create_block();
    let exit_blk = b.create_block();
    b.ins().jump(header, &[]);

    b.switch_to_block(header);
    let xv = b.use_var(x);
    let ev = b.use_var(end_var);
    let cond = b.ins().icmp(IntCC::SignedLessThan, xv, ev);
    b.ins().brif(cond, body_blk, &[], exit_blk, &[]);

    b.switch_to_block(body_blk);
    b.seal_block(body_blk);
    let r = gen_value(&mut b, &rl.bodies[0], &mut vars, fn_ids, module, NumKind::Int);
    b.def_var(acc, r);
    let jv = b.use_var(j);
    let off = b.ins().imul_imm(jv, 8);
    let dp = b.use_var(dst_var);
    let addr = b.ins().iadd(dp, off);
    b.ins().store(MemFlags::trusted(), r, addr, 0);
    let nj = b.ins().iadd_imm(jv, 1);
    b.def_var(j, nj);
    let xv2 = b.use_var(x);
    let nx = b.ins().iadd_imm(xv2, 1);
    b.def_var(x, nx);
    b.ins().jump(header, &[]);
    b.seal_block(header);

    b.switch_to_block(exit_blk);
    b.seal_block(exit_blk);
    b.ins().return_(&[]);

    b.finalize();
    module.define_function(fid, ctx).ok()?;
    module.clear_context(ctx);
    Some(())
}

/// Emit a native loop for `range(start,end).reduce(init, (pa,pb)=>body)`:
/// `acc=init; x=start; while x<end { acc=body(acc,x); x+=1 } return acc`.
/// Integer arithmetic wraps (`iadd`/`imul`), matching the interpreter's release
/// semantics, and the empty range returns `init` — identical to the VM loop.
fn define_reduce_loop(
    module: &mut JITModule,
    ctx: &mut cranelift_codegen::Context,
    bctx: &mut FunctionBuilderContext,
    fid: FuncId,
    rl: &crate::bytecode::ReduceLoop,
    fn_ids: &HashMap<&str, FuncId>,
    mixed: &MixedTables,
) -> Option<()> {
    // Scalar accumulator (1 body): `fn(start, end, init) -> i64`. Tuple accumulator
    // (N bodies): `fn(start, end, acc_ptr)` — the N `i64` slots are loaded from / stored
    // to `acc_ptr`, kept in N registers across the loop. Both keep `i64` arithmetic
    // wrapping, matching the interpreter (and the differential oracle).
    let n = rl.bodies.len();
    let scalar = n == 1;
    // An `f64` accumulator over the `i64` counter `pb`: the body is lowered per-node (integer
    // subexpressions of `pb` stay `i64`, promoting at the first float operand). `float` covers
    // both the scalar (1 slot) and tuple (N>1 slots) shapes; the counter stays `i64`.
    let float = rl.float;
    let float_scalar = scalar && float;
    let slot_ty = if float { F64 } else { I64 }; // accumulator slot register
    // param2: scalar → the init VALUE (f64 for a float scalar); tuple → the `acc_ptr` (i64).
    let third_ty = if float_scalar { F64 } else { I64 };
    // A scalar body may capture loop-invariant outer `i64` values, passed via a 4th
    // pointer param `caps` (the nested-fold case). Tuple/float accumulators don't capture.
    let has_caps = scalar && !rl.captures.is_empty();
    // A RAISING scalar f64 reduce takes an extra `*mut i8` **poison** out-param: the codegen ORs
    // `divisor == 0` into it (a `/0` where the interpreter raises), and the VM falls back if set.
    // Mutually exclusive with `has_caps` — a caps body (the float dot-product) never divides.
    //
    // `rl.raises` is READ, not re-derived. This decides the built SIGNATURE, and the VM must
    // select its call wrapper by the same answer: a 5-argument kernel invoked through a
    // 4-argument signature is undefined behaviour, not a wrong number. Both sides now read
    // one field the compiler set, so they cannot drift — which is what makes it safe to widen
    // the predicate to cover calls, whose callee bodies the VM cannot even see.
    let needs_poison = float_scalar && !has_caps && rl.raises;
    ctx.func.signature.call_conv = CallConv::SystemV;
    ctx.func.signature.params.push(AbiParam::new(I64)); // start
    ctx.func.signature.params.push(AbiParam::new(I64)); // end
    ctx.func.signature.params.push(AbiParam::new(third_ty)); // scalar: init; tuple: acc_ptr (i64)
    if has_caps {
        ctx.func.signature.params.push(AbiParam::new(I64)); // caps: *const i64
    }
    if needs_poison {
        ctx.func.signature.params.push(AbiParam::new(I64)); // poison: *mut i8 (dividing scalar f64)
    }
    if scalar {
        ctx.func.signature.returns.push(AbiParam::new(third_ty));
    }

    let mut b = FunctionBuilder::new(&mut ctx.func, bctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let start = b.block_params(entry)[0];
    let end = b.block_params(entry)[1];
    let third = b.block_params(entry)[2]; // scalar: init value; tuple: acc slot pointer
    // The poison out-param (dividing scalar f64 only) is the block param right after `third`
    // (no caps in that case). An `i8` poison var, seeded to 0, is OR'd by the Div arm and stored
    // back through this pointer at loop exit.
    let poison_ptr = if needs_poison { Some(b.block_params(entry)[3]) } else { None };
    let poison_var: Option<Variable> = if needs_poison {
        let v = b.declare_var(I8);
        let zero = b.ins().iconst(I8, 0);
        b.def_var(v, zero);
        Some(v)
    } else {
        None
    };
    // Everything a mixed CALLEE needs, built only when this kernel actually carries poison —
    // the callee's ABI ends in a `*mut i8` it may set, and without somewhere for that flag to
    // land the call would silently swallow the callee's `/0` or NaN compare. `raises` (which
    // counts a mixed call) is what guarantees `needs_poison` is true whenever the analysis
    // admitted one, so `None` here means the body provably contains no mixed call.
    let mixed_ctx: Option<MixedCallCtx> = needs_poison.then(|| MixedCallCtx {
        sigs: mixed.sigs,
        ids: mixed.ids,
        poison_cell: b.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            8,
            3,
        )),
    });

    let x_var = b.declare_var(I64);
    let end_var = b.declare_var(I64);
    b.def_var(x_var, start);
    b.def_var(end_var, end);

    // One register per accumulator slot. Scalar seeds slot 0 with `init`; tuple loads
    // each slot from `acc_ptr[k]`. An f64 accumulator's slots are `f64` (8 bytes either way,
    // so the i64 `acc_ptr` carries the f64 bit patterns the VM packed/unpacks).
    let acc_vars: Vec<Variable> = (0..n).map(|_| b.declare_var(slot_ty)).collect();
    if scalar {
        b.def_var(acc_vars[0], third);
    } else {
        for (k, &v) in acc_vars.iter().enumerate() {
            let loaded = b.ins().load(slot_ty, MemFlags::trusted(), third, (k * 8) as i32);
            b.def_var(v, loaded);
        }
    }

    // Load each captured value once (loop-invariant) from the `caps` pointer (4th param). The
    // load TYPE is per cap kind, not per kernel: a `ScalarValue` (a VALUE scalar such as the
    // coefficient `c` in `s + c*a[i]`) rides as `f64` — the VM packs `Value::Int` as
    // `(i as f64).to_bits()` or passes `Value::Float`'s bits — while a `Scalar` (an INDEX
    // scalar) and every array BASE POINTER are `i64`. Typing the load by the cap is what keeps
    // a pointer from being read as a float and vice versa.
    let cap_vars: Vec<Variable> = if has_caps {
        let caps_ptr = b.block_params(entry)[3];
        rl.captures
            .iter()
            .enumerate()
            .map(|(j, cap)| {
                let ty = if cap.kind == CaptureKind::ScalarValue { F64 } else { I64 };
                let v = b.ins().load(ty, MemFlags::trusted(), caps_ptr, (j * 8) as i32);
                let var = b.declare_var(ty);
                b.def_var(var, v);
                var
            })
            .collect()
    } else {
        Vec::new()
    };

    // MULTI-ACCUMULATOR fast path (i64 scalar associative SUM `acc = acc + term(pb)`): the single
    // accumulator serialises on the add-latency dependency chain, so split it into K independent
    // partial accumulators over a K-strided main loop + a remainder tail, combined at exit. Integer
    // add is associative + commutative, so `init + Σ term` is BIT-IDENTICAL regardless of grouping —
    // ~2.3× per core (breaks the latency chain). The caps/index machinery rides `vars` unchanged; a
    // range under the length K degrades gracefully to the tail (== the single-accumulator loop).
    if let Some(term) = reduce_multiacc_term(rl) {
        const K: i64 = 4;
        // `main_end = start + (n/K)*K` — the largest K-multiple within `[start,end)`, computed once
        // (i128-safe span is already VM-capped at 100M), so the strided loop never overflows `x+K`.
        let n = b.ins().isub(end, start);
        let kc = b.ins().iconst(I64, K);
        let blocks = b.ins().sdiv(n, kc);
        let main_len = b.ins().imul(blocks, kc);
        let main_end = b.ins().iadd(start, main_len);

        let macc: Vec<Variable> = (0..K).map(|_| b.declare_var(I64)).collect();
        let zero = b.ins().iconst(I64, 0);
        for &m in &macc {
            b.def_var(m, zero); // partials start at the additive identity; `init` is added at exit
        }
        let pb_var = b.declare_var(I64); // rebound to `x+d` before lowering `term`
        let mut vars: HashMap<&str, Variable> = HashMap::new();
        vars.insert(rl.pb.as_str(), pb_var);
        for (j, cap) in rl.captures.iter().enumerate() {
            if let Some(&cv) = cap_vars.get(j) {
                vars.insert(cap.name.as_str(), cv);
            }
        }

        let main_hdr = b.create_block();
        let main_body = b.create_block();
        let tail_hdr = b.create_block();
        let tail_body = b.create_block();
        let done = b.create_block();

        b.ins().jump(main_hdr, &[]);

        // main_hdr: `x < main_end ?`
        b.switch_to_block(main_hdr);
        let xh = b.use_var(x_var);
        let cmain = b.ins().icmp(IntCC::SignedLessThan, xh, main_end);
        b.ins().brif(cmain, main_body, &[], tail_hdr, &[]);

        // main_body: K independent partials at x+0..x+K-1
        b.switch_to_block(main_body);
        b.seal_block(main_body);
        let xb = b.use_var(x_var);
        for (d, &m) in macc.iter().enumerate() {
            let xd = if d == 0 {
                xb
            } else {
                let dc = b.ins().iconst(I64, d as i64);
                b.ins().iadd(xb, dc)
            };
            b.def_var(pb_var, xd);
            let t = gen_value(&mut b, term, &mut vars, fn_ids, module, NumKind::Int);
            let a = b.use_var(m);
            let na = b.ins().iadd(a, t);
            b.def_var(m, na);
        }
        let kc2 = b.ins().iconst(I64, K);
        let xbn = b.use_var(x_var);
        let nx = b.ins().iadd(xbn, kc2);
        b.def_var(x_var, nx);
        b.ins().jump(main_hdr, &[]);
        b.seal_block(main_hdr);

        // tail_hdr: `x < end ?` (the final `(end-start) mod K` elements)
        b.switch_to_block(tail_hdr);
        let xt = b.use_var(x_var);
        let et = b.use_var(end_var);
        let ctail = b.ins().icmp(IntCC::SignedLessThan, xt, et);
        b.ins().brif(ctail, tail_body, &[], done, &[]);

        // tail_body: fold single-stride into macc[0]
        b.switch_to_block(tail_body);
        b.seal_block(tail_body);
        let xtb = b.use_var(x_var);
        b.def_var(pb_var, xtb);
        let tt = gen_value(&mut b, term, &mut vars, fn_ids, module, NumKind::Int);
        let a0 = b.use_var(macc[0]);
        let na0 = b.ins().iadd(a0, tt);
        b.def_var(macc[0], na0);
        let one = b.ins().iconst(I64, 1);
        let nxt = b.ins().iadd(xtb, one);
        b.def_var(x_var, nxt);
        b.ins().jump(tail_hdr, &[]);
        b.seal_block(tail_hdr);

        // done: `init + Σ macc` — the horizontal combine (acc_vars[0] was seeded to `init`)
        b.switch_to_block(done);
        b.seal_block(done);
        let mut total = b.use_var(acc_vars[0]);
        for &m in &macc {
            let mv = b.use_var(m);
            total = b.ins().iadd(total, mv);
        }
        b.ins().return_(&[total]);

        b.finalize();
        module.define_function(fid, ctx).ok()?;
        module.clear_context(ctx);
        return Some(());
    }

    let header = b.create_block();
    let body_blk = b.create_block();
    let exit_blk = b.create_block();

    b.ins().jump(header, &[]);

    // header: `x < end ?` — branch into the body or out. Sealed only after the
    // body's back-edge is emitted (its two predecessors are entry and body).
    b.switch_to_block(header);
    let xv = b.use_var(x_var);
    let ev = b.use_var(end_var);
    let cond = b.ins().icmp(IntCC::SignedLessThan, xv, ev);
    b.ins().brif(cond, body_blk, &[], exit_blk, &[]);

    b.switch_to_block(body_blk);
    b.seal_block(body_blk);
    let mut vars: HashMap<&str, Variable> = HashMap::new();
    if scalar {
        vars.insert(rl.pa.as_str(), acc_vars[0]);
    } else {
        for (k, &v) in acc_vars.iter().enumerate() {
            vars.insert(ACC_IDENTS[k], v);
        }
    }
    vars.insert(rl.pb.as_str(), x_var);
    // Bind each captured variable to its (loop-invariant) loaded `i64`. A `Scalar` cap's
    // slot IS its value (read by a bare `Ident`); an `ArrayI64` cap's slot is the packed
    // array base pointer (read by the `Index` arm of `gen_value` as `caps_slot + idx*8`).
    // Both ride `vars` transparently — eligibility guarantees a name is used one way only,
    // so the same map never confuses a value for a base.
    for (j, cap) in rl.captures.iter().enumerate() {
        if let Some(&cv) = cap_vars.get(j) {
            vars.insert(cap.name.as_str(), cv);
        }
    }
    // Compute every component from the OLD slot values, then assign — so a component that
    // reads another slot (`(a[0] + x, a[1] + a[0])`) sees the pre-update value.
    let mut new_vals: Vec<ClValue> = Vec::with_capacity(n);
    if float {
        // f64 fold over the i64 counter `pb`: typed per-node (roots are `f64`, eligibility-
        // guaranteed). Scalar binds `pa`; a tuple binds the `$acc{k}` slots. `vars` is unused.
        let mut binders: HashMap<&str, (Variable, NumKind)> = HashMap::new();
        if scalar {
            binders.insert(rl.pa.as_str(), (acc_vars[0], NumKind::Float));
        } else {
            for (k, &v) in acc_vars.iter().enumerate() {
                binders.insert(ACC_IDENTS[k], (v, NumKind::Float));
            }
        }
        binders.insert(rl.pb.as_str(), (x_var, NumKind::Int));
        // ArrayF64 captures (v1b float dot-product): each `cap_var` holds the packed f64-array
        // base pointer; the `Index` arm reads `arr[pb]` from it. Scalar float reduces only;
        // empty for the capture-free scalar/tuple float folds.
        let mut arrays: HashMap<&str, Variable> = HashMap::new();
        for (j, cap) in rl.captures.iter().enumerate() {
            if let Some(&cv) = cap_vars.get(j) {
                match cap.kind {
                    CaptureKind::ArrayF64 => {
                        arrays.insert(cap.name.as_str(), cv);
                    }
                    // A loop-invariant `i64` scalar an affine index reads (`i`/`n` in `a[i*n+k]`).
                    // Bound as an `Int` binder so the generic `Index` arm can evaluate the ORIGINAL
                    // index expression from it. Synthetic `$aff*` caps carry the same index's
                    // pre-computed base/coef for the VM's bounds check ONLY — the body never names
                    // them, so binding them here is harmless and they simply go unread.
                    // An INDEX scalar: `i64` (`i`/`n` in `a[i*n+k]`), bound as an `Int` binder so
                    // the generic `Index` arm can evaluate the ORIGINAL index expression from it.
                    CaptureKind::Scalar => {
                        binders.insert(cap.name.as_str(), (cv, NumKind::Int));
                    }
                    // A VALUE scalar (the coefficient `c` in `s + c*a[i]`): loaded `f64` below,
                    // so it is a `Float` binder. `infer_f64_indexed` admitted it only where a
                    // genuine float promotes it, which is what makes the `f64` typing match the
                    // interpreter bit-for-bit.
                    CaptureKind::ScalarValue => {
                        binders.insert(cap.name.as_str(), (cv, NumKind::Float));
                    }
                    CaptureKind::ArrayI64 => {}
                }
            }
        }
        let mut cx = F64Ctx {
            binders: &mut binders,
            arrays: &arrays,
            poison: poison_var,
            fn_ids,
            module,
            mixed: mixed_ctx.as_ref(),
        };
        for body in &rl.bodies {
            new_vals.push(gen_f64_typed(&mut b, body, &mut cx).0);
        }
    } else {
        for body in &rl.bodies {
            let v = gen_value(&mut b, body, &mut vars, fn_ids, module, NumKind::Int);
            new_vals.push(v);
        }
    }
    for (k, &v) in acc_vars.iter().enumerate() {
        b.def_var(v, new_vals[k]);
    }
    let xv2 = b.use_var(x_var);
    let one = b.ins().iconst(I64, 1);
    let nx = b.ins().iadd(xv2, one);
    b.def_var(x_var, nx);
    b.ins().jump(header, &[]);

    b.seal_block(header);

    b.switch_to_block(exit_blk);
    b.seal_block(exit_blk);
    // Write the accumulated poison flag back through the out-param before returning (dividing
    // scalar f64 only). Non-zero ⇒ some iteration divided by zero ⇒ the VM discards this result
    // and falls back to the exact-erroring bytecode loop.
    if let (Some(pv), Some(pp)) = (poison_var, poison_ptr) {
        let pval = b.use_var(pv);
        b.ins().store(MemFlags::trusted(), pval, pp, 0);
    }
    if scalar {
        let result = b.use_var(acc_vars[0]);
        b.ins().return_(&[result]);
    } else {
        for (k, &v) in acc_vars.iter().enumerate() {
            let val = b.use_var(v);
            b.ins().store(MemFlags::trusted(), val, third, (k * 8) as i32);
        }
        b.ins().return_(&[]);
    }

    b.finalize();
    module.define_function(fid, ctx).ok()?;
    module.clear_context(ctx);
    Some(())
}

/// Emit a native per-element kernel over a packed `i64` buffer:
/// - **map** `extern "C" fn(src,dst,len)`: `for i in 0..len { dst[i] = body(src[i]) }`.
/// - **filter** `extern "C" fn(src,dst,len)->i64`: a branchless compaction —
///   `store dst[w]=src[i]; w += pred(src[i])` — returning `w` (kept count), so
///   `dst[0..w]` holds the kept elements in order. Integer arithmetic wraps, matching
///   the interpreter's release semantics and the bytecode loop byte-for-byte.
#[allow(clippy::too_many_arguments)]
fn define_array_kernel<'a>(
    module: &mut JITModule,
    ctx: &mut cranelift_codegen::Context,
    bctx: &mut FunctionBuilderContext,
    fid: FuncId,
    k: &'a crate::bytecode::ArrayKernel,
    is_filter: bool,
    fn_ids: &HashMap<&'a str, FuncId>,
    elem_kind: NumKind,
    mixed_root: Option<NumKind>,
    value_scalars: bool,
    msigs: &MixedSigTable,
    mixed_ids: &HashMap<&str, FuncId>,
) -> Option<()> {
    // Element + capture values are `i64` (map over an `Int` array) or `f64` (map over a
    // `Float` array); the buffer pointers and length are always `i64`. Filter is `Int`.
    // A `mixed` map reads `i64` elements through a per-node-typed body; its ROOT decides
    // what it writes — `Some(Float)` stores the `f64` result, `Some(Int)` the `i64` one
    // (float intermediates, integer output — `to_int(to_float(i) * 1.5)`).
    let mixed = mixed_root.is_some();
    let elem_ty = if mixed {
        I64
    } else if matches!(elem_kind, NumKind::Float) {
        F64
    } else {
        I64
    };
    // A RAISING body (a rounder that can leave i64 range — `ArrayKernel::raises`) takes a
    // 5th param: the poison out-cell (`*mut i64`). Only the mixed families can admit such
    // bodies; the i64/f64/filter analyses reject the rounders outright.
    let raising = mixed && k.raises;
    ctx.func.signature.call_conv = CallConv::SystemV;
    for _ in 0..3 {
        ctx.func.signature.params.push(AbiParam::new(I64)); // src, dst, len
    }
    // The caps pointer is the 4th param for BOTH shapes — a filter predicate may capture
    // loop-invariant `i64` scalars exactly as a map body may. Filter additionally RETURNS the
    // kept count; that is the only signature difference.
    ctx.func.signature.params.push(AbiParam::new(I64)); // caps ptr
    if raising {
        ctx.func.signature.params.push(AbiParam::new(I64)); // poison out-cell ptr
    }
    if is_filter {
        ctx.func.signature.returns.push(AbiParam::new(I64));
    }

    let mut b = FunctionBuilder::new(&mut ctx.func, bctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let src = b.block_params(entry)[0];
    let dst = b.block_params(entry)[1];
    let len = b.block_params(entry)[2];
    // The caps pointer (loop-invariant captured `i64` values), bound below. Present for both
    // map and filter.
    let caps_ptr = Some(b.block_params(entry)[3]);
    let poison_ptr = raising.then(|| b.block_params(entry)[4]);

    let i_var = b.declare_var(I64); // read cursor
    let w_var = b.declare_var(I64); // write cursor (filter); == i for map
    let src_var = b.declare_var(I64);
    let dst_var = b.declare_var(I64);
    let len_var = b.declare_var(I64);
    // The poison accumulator every `gen_value_typed` body receives; a non-raising body never
    // writes it and the dead variable costs nothing. Stored to the out-cell at exit when the
    // signature carries one.
    let poison_var = b.declare_var(I64);
    let zero = b.ins().iconst(I64, 0);
    b.def_var(i_var, zero);
    b.def_var(w_var, zero);
    b.def_var(src_var, src);
    b.def_var(dst_var, dst);
    b.def_var(len_var, len);
    b.def_var(poison_var, zero);

    // Hoist the loop-invariant capture loads into the entry (pre-loop) block — read each
    // once rather than re-loading it from `caps` on every iteration (mirrors the reduce
    // kernel's entry-block capture loads). Immediate-offset load straight off `caps_ptr`.
    let cap_vars: Vec<Variable> = if let Some(cp) = caps_ptr {
        k.captures
            .iter()
            .enumerate()
            .map(|(j, cap)| {
                // Load type per cap KIND, not per kernel — this is what keeps a base pointer
                // from being reinterpreted as an `f64`, or a value scalar from riding as the
                // wrong width:
                //   * Scalar (an INDEX scalar, or a value scalar in the i64/f64 map) → the
                //     element type: `i64` for the i64 kernel, `f64` for the f64 (Floats-source)
                //     kernel, which the VM marshals to match.
                //   * ScalarValue in the MIXED kernel → `f64` (SAXPY's coefficient; the VM
                //     marshals `Value::Int`→f64 or passes `Value::Float` bits). Elsewhere it
                //     rides `i64` exactly like a Scalar.
                //   * ArrayI64/ArrayF64 → a base POINTER, always `i64`.
                use crate::bytecode::CaptureKind as CK;
                let ty = match cap.kind {
                    // The value-scalar variant loads EVERY scalar cap as `f64` bits — the
                    // stored kinds are the plain analysis's `Scalar`s, but this
                    // specialization's marshal promotes each to f64 at dispatch.
                    CK::Scalar | CK::ScalarValue if mixed && value_scalars => F64,
                    CK::ScalarValue if mixed => F64,
                    CK::Scalar | CK::ScalarValue => elem_ty,
                    CK::ArrayI64 | CK::ArrayF64 => I64,
                };
                let v = b.ins().load(ty, MemFlags::trusted(), cp, (j * 8) as i32);
                let cvar = b.declare_var(ty);
                b.def_var(cvar, v);
                cvar
            })
            .collect()
    } else {
        Vec::new()
    };

    let header = b.create_block();
    let body_blk = b.create_block();
    let exit_blk = b.create_block();
    b.ins().jump(header, &[]);

    // header: `i < len ?`
    b.switch_to_block(header);
    let iv = b.use_var(i_var);
    let lv = b.use_var(len_var);
    let cond = b.ins().icmp(IntCC::SignedLessThan, iv, lv);
    b.ins().brif(cond, body_blk, &[], exit_blk, &[]);

    b.switch_to_block(body_blk);
    b.seal_block(body_blk);
    // elem = src[i]
    let iv2 = b.use_var(i_var);
    let ioff = b.ins().imul_imm(iv2, 8);
    let srcp = b.use_var(src_var);
    let saddr = b.ins().iadd(srcp, ioff);
    let elem = b.ins().load(elem_ty, MemFlags::trusted(), saddr, 0);
    let elem_var = b.declare_var(elem_ty);
    b.def_var(elem_var, elem);

    let mut vars: HashMap<&'a str, Variable> = HashMap::new();
    vars.insert(k.binder.as_str(), elem_var);
    // Bind each captured variable to its pre-hoisted entry-block load (loop-invariant;
    // `caps[j]` is `i64` for an `Int` kernel, `f64` for a `Float` one — the VM coerces).
    for (j, cap) in k.captures.iter().enumerate() {
        vars.insert(cap.name.as_str(), cap_vars[j]);
    }

    if is_filter {
        // dst[w] = elem; w += (pred ? 1 : 0)
        let wv = b.use_var(w_var);
        let woff = b.ins().imul_imm(wv, 8);
        let dstp = b.use_var(dst_var);
        let daddr = b.ins().iadd(dstp, woff);
        b.ins().store(MemFlags::trusted(), elem, daddr, 0);
        // `elem_kind` selects the comparison family (icmp for the i64 pass, fcmp for the
        // f64 one); the poison variable is only ever written by Float ordering
        // comparisons, so the i64 pass leaves it dead exactly as before.
        let keep = gen_cond(&mut b, &k.body, &mut vars, fn_ids, module, elem_kind, Some(poison_var));
        let keep64 = b.ins().uextend(I64, keep);
        let wv2 = b.use_var(w_var);
        let nw = b.ins().iadd(wv2, keep64);
        b.def_var(w_var, nw);
    } else {
        // dst[i] = body(elem). A mixed map types the body node-by-node (i64 element in,
        // root kind out); the plain map uses the monomorphized `elem_kind` codegen.
        let r = if let Some(root) = mixed_root {
            // The `ScalarValue` caps ride as `f64` in this kernel — hand their names to the
            // typed codegen so its `Ident` arm types them `Float` (the prologue loaded them
            // F64). In the value-scalar variant EVERY capture rides that way.
            let f64_scalars: HashSet<&str> = if value_scalars {
                k.captures.iter().map(|c| c.name.as_str()).collect()
            } else {
                k.captures
                    .iter()
                    .filter(|c| c.kind == crate::bytecode::CaptureKind::ScalarValue)
                    .map(|c| c.name.as_str())
                    .collect()
            };
            // The cell a mixed CALLEE writes its poison flag into (its ABI wants a pointer;
            // ours is a register accumulator). One slot per kernel, reset before each call.
            let cell = b.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let mixed_ctx = MixedCallCtx { sigs: msigs, ids: mixed_ids, poison_cell: cell };
            let mut cx = TypedCtx {
                vars: &vars,
                binder: &k.binder,
                f64_scalars: &f64_scalars,
                fn_ids,
                module,
                mixed: &mixed_ctx,
                poison: poison_var,
            };
            let (r, kind) = gen_value_typed(&mut b, &k.body, &mut cx);
            // The build gate re-derived the root via the same analysis this codegen mirrors,
            // so a mismatch here is a twin-drift bug, not a runtime condition.
            debug_assert!(kind == root, "mixed map root drifted between analysis and codegen");
            r
        } else {
            gen_value(&mut b, &k.body, &mut vars, fn_ids, module, elem_kind)
        };
        let iv3 = b.use_var(i_var);
        let doff = b.ins().imul_imm(iv3, 8);
        let dstp = b.use_var(dst_var);
        let daddr = b.ins().iadd(dstp, doff);
        b.ins().store(MemFlags::trusted(), r, daddr, 0);
    }

    let iv4 = b.use_var(i_var);
    let ni = b.ins().iadd_imm(iv4, 1);
    b.def_var(i_var, ni);
    b.ins().jump(header, &[]);
    b.seal_block(header);

    b.switch_to_block(exit_blk);
    b.seal_block(exit_blk);
    // A raising kernel reports its accumulated poison through the out-cell — written once,
    // at exit, so the loop itself carries the flag in a register.
    if let Some(pp) = poison_ptr {
        let pv = b.use_var(poison_var);
        b.ins().store(MemFlags::trusted(), pv, pp, 0);
    }
    if is_filter {
        let wv = b.use_var(w_var);
        // The f64 filter reports an accumulated NaN poison as -1 — the runner maps it to
        // `None` and the dispatch falls back to the bytecode loop, which raises the
        // interpreter's "cannot compare these values" at the exact element. The i64 pass
        // never writes the poison variable, so its return is the plain count as before.
        let ret = if !mixed && matches!(elem_kind, NumKind::Float) {
            let pv = b.use_var(poison_var);
            let bad = b.ins().icmp_imm(IntCC::NotEqual, pv, 0);
            let neg1 = b.ins().iconst(I64, -1);
            b.ins().select(bad, neg1, wv)
        } else {
            wv
        };
        b.ins().return_(&[ret]);
    } else {
        b.ins().return_(&[]);
    }

    b.finalize();
    module.define_function(fid, ctx).ok()?;
    module.clear_context(ctx);
    Some(())
}

/// Emit a fused pipeline as one native loop with no intermediate arrays. Threads each
/// element through the stages in registers (a `Map` transforms it; a `Filter` branches a
/// rejected element to the loop's continue block — stream fusion's *Skip*), then the sink
/// (`Collect`: `dst[w]=cur; w++`, return kept count; `Reduce`: `acc=red(acc,cur)`, return
/// acc). Source is an `Int` array (`src,…,len`) or a `range` counter (`start,end`).
/// Signatures: array+Collect `fn(src,dst,len)->i64`; array+Reduce `fn(src,len,init)->i64`;
/// range+Reduce `fn(start,end,init)->i64`. Integer arithmetic wraps, matching the oracle.
fn define_fused_kernel<'a>(
    module: &mut JITModule,
    ctx: &mut cranelift_codegen::Context,
    bctx: &mut FunctionBuilderContext,
    fid: FuncId,
    k: &'a crate::bytecode::FusedKernel,
    fn_ids: &HashMap<&'a str, FuncId>,
) -> Option<()> {
    use crate::bytecode::{FusionSink, FusionStage};
    let is_reduce = matches!(k.sink, FusionSink::Reduce { .. });
    let n_acc = match &k.sink {
        FusionSink::Reduce { bodies, .. } => bodies.len(),
        _ => 0,
    };
    let tuple_reduce = n_acc > 1;
    // A float-flagged reduce sink folds a `Float` array (the VM dispatches it only on a
    // `Floats` source): scalar (1 slot) → the accumulator is `f64`; tuple (N>1) → every slot
    // is `f64`. The element is `f64` in both. The i64 paths keep `I64` throughout.
    let float_sink = matches!(&k.sink, FusionSink::Reduce { float: true, .. });
    let float_reduce = float_sink && n_acc == 1; // scalar f64 accumulator
    let float_tuple = float_sink && n_acc > 1; // N-slot f64 accumulator
    let acc_ty = if float_reduce { F64 } else { I64 }; // scalar accumulator register
    let slot_ty = if float_tuple { F64 } else { I64 }; // tuple slot register
    let elem_ty = if float_sink { F64 } else { I64 }; // Float-array element

    ctx.func.signature.call_conv = CallConv::SystemV;
    ctx.func.signature.params.push(AbiParam::new(I64)); // src pointer / range start
    ctx.func.signature.params.push(AbiParam::new(I64)); // length / range end
    ctx.func.signature.params.push(AbiParam::new(acc_ty)); // init (f64 for a float reduce)
    // Scalar reduce / collect / count return one value (accumulator or kept count); a
    // tuple reduce instead writes its N slots back through the `acc_ptr` (param 3).
    if !tuple_reduce {
        ctx.func.signature.returns.push(AbiParam::new(acc_ty));
    }

    let mut b = FunctionBuilder::new(&mut ctx.func, bctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let (p0, p1, p2) = (b.block_params(entry)[0], b.block_params(entry)[1], b.block_params(entry)[2]);

    let idx_var = b.declare_var(I64); // read cursor (array index `i`, or range counter `x`)
    let limit_var = b.declare_var(I64);
    let sink_var = b.declare_var(acc_ty); // scalar accumulator (f64 for a float reduce) / cursor
    let src_var = b.declare_var(I64);
    let dst_var = b.declare_var(I64);
    // A tuple reduce keeps its N slots in their own registers (loaded from `acc_ptr`); an
    // f64 tuple's slots are `f64`.
    let acc_vars: Vec<Variable> = (0..if tuple_reduce { n_acc } else { 0 })
        .map(|_| b.declare_var(slot_ty))
        .collect();
    let z = b.ins().iconst(I64, 0);

    // Wire the three params to roles by shape: range → (start,end,init?); array+reduce →
    // (src,len,init); array+count → (src,len,_); array+collect → (src,dst,len). The
    // accumulator (`sink_var`) starts at the reduce `init`, else 0 (a counter / write
    // cursor).
    if k.source_is_range {
        b.def_var(idx_var, p0); // x starts at `start`
        b.def_var(limit_var, p1); // `end`
        b.def_var(sink_var, if is_reduce { p2 } else { z });
        b.def_var(src_var, z);
        b.def_var(dst_var, z);
    } else {
        b.def_var(idx_var, z);
        b.def_var(src_var, p0);
        match &k.sink {
            FusionSink::Collect => {
                b.def_var(limit_var, p2); // len
                b.def_var(sink_var, z); // w = 0
                b.def_var(dst_var, p1);
            }
            FusionSink::Reduce { .. } => {
                b.def_var(limit_var, p1); // len
                b.def_var(sink_var, p2); // acc init
                b.def_var(dst_var, z);
            }
            FusionSink::Count => {
                b.def_var(limit_var, p1); // len
                b.def_var(sink_var, z); // counter = 0
                b.def_var(dst_var, z);
            }
        }
    }
    // A tuple reduce's third param is the `acc_ptr`; seed each slot register from it.
    // (`sink_var` was harmlessly set to the pointer above and stays unused.)
    if tuple_reduce {
        for (k2, &v) in acc_vars.iter().enumerate() {
            let loaded = b.ins().load(slot_ty, MemFlags::trusted(), p2, (k2 * 8) as i32);
            b.def_var(v, loaded);
        }
    }

    let header = b.create_block();
    let body = b.create_block();
    let cont = b.create_block();
    let exit = b.create_block();
    b.ins().jump(header, &[]);

    // header: idx < limit ?
    b.switch_to_block(header);
    let iv = b.use_var(idx_var);
    let lv = b.use_var(limit_var);
    let cond = b.ins().icmp(IntCC::SignedLessThan, iv, lv);
    b.ins().brif(cond, body, &[], exit, &[]);

    // body: load the element, run stages, run the sink.
    b.switch_to_block(body);
    b.seal_block(body);
    let elem = if k.source_is_range {
        b.use_var(idx_var) // the counter value itself
    } else {
        let i = b.use_var(idx_var);
        let off = b.ins().imul_imm(i, 8);
        let base = b.use_var(src_var);
        let addr = b.ins().iadd(base, off);
        // The element is `f64` for any float reduce (scalar or tuple — `Floats` source), else
        // `i64` (8 bytes either way). `elem_ty` (not `acc_ty`) so an f64 *tuple*, whose scalar
        // `acc_ty` is the unused `I64`, still loads an `f64` element.
        b.ins().load(elem_ty, MemFlags::trusted(), addr, 0)
    };
    let cur_var = b.declare_var(elem_ty);
    b.def_var(cur_var, elem);

    // Thread the element through the stages. A Map rebinds `cur`; a Filter splits the
    // straight-line body and sends a rejected element to `cont` (skip the sink).
    for stage in &k.stages {
        match stage {
            FusionStage::Map { binder, body: bexpr } => {
                let mut vars: HashMap<&'a str, Variable> = HashMap::new();
                vars.insert(binder.as_str(), cur_var);
                let nv = gen_value(&mut b, bexpr, &mut vars, fn_ids, module, NumKind::Int);
                b.def_var(cur_var, nv);
            }
            FusionStage::Filter { binder, body: bexpr } => {
                let mut vars: HashMap<&'a str, Variable> = HashMap::new();
                vars.insert(binder.as_str(), cur_var);
                let keep = gen_cond(&mut b, bexpr, &mut vars, fn_ids, module, NumKind::Int, None);
                let accept = b.create_block();
                b.ins().brif(keep, accept, &[], cont, &[]);
                b.switch_to_block(accept);
                b.seal_block(accept);
            }
        }
    }

    // sink
    match &k.sink {
        FusionSink::Collect => {
            let w = b.use_var(sink_var);
            let off = b.ins().imul_imm(w, 8);
            let base = b.use_var(dst_var);
            let addr = b.ins().iadd(base, off);
            let cur = b.use_var(cur_var);
            b.ins().store(MemFlags::trusted(), cur, addr, 0);
            let nw = b.ins().iadd_imm(w, 1);
            b.def_var(sink_var, nw);
        }
        FusionSink::Reduce { pa, pb, bodies, .. } => {
            if float_tuple {
                // N `f64` slots → N components, typed per-node over `{$acc0…(Float), pb(Float
                // element)}`, computed from the OLD slots then assigned.
                let mut binders: HashMap<&str, (Variable, NumKind)> = HashMap::new();
                for (k2, &v) in acc_vars.iter().enumerate() {
                    binders.insert(ACC_IDENTS[k2], (v, NumKind::Float));
                }
                binders.insert(pb.as_str(), (cur_var, NumKind::Float));
                let no_arrays: HashMap<&str, Variable> = HashMap::new();
                // No poison and no mixed table: this shape admits no user call at all (its
                // analysis `infer_f64_typed` guards its call arm on `!user_fns.contains`).
                let mut cx = F64Ctx {
                    binders: &mut binders,
                    arrays: &no_arrays,
                    poison: None,
                    fn_ids,
                    module,
                    mixed: None,
                };
                let new_vals: Vec<ClValue> =
                    bodies.iter().map(|body| gen_f64_typed(&mut b, body, &mut cx).0).collect();
                for (k2, &v) in acc_vars.iter().enumerate() {
                    b.def_var(v, new_vals[k2]);
                }
            } else if tuple_reduce {
                // N slots → N components, computed from the OLD slots then assigned.
                let mut vars: HashMap<&'a str, Variable> = HashMap::new();
                for (k2, &v) in acc_vars.iter().enumerate() {
                    vars.insert(ACC_IDENTS[k2], v);
                }
                vars.insert(pb.as_str(), cur_var);
                let mut new_vals: Vec<ClValue> = Vec::with_capacity(n_acc);
                for body in bodies {
                    new_vals.push(gen_value(&mut b, body, &mut vars, fn_ids, module, NumKind::Int));
                }
                for (k2, &v) in acc_vars.iter().enumerate() {
                    b.def_var(v, new_vals[k2]);
                }
            } else {
                let mut vars: HashMap<&'a str, Variable> = HashMap::new();
                vars.insert(pa.as_str(), sink_var);
                vars.insert(pb.as_str(), cur_var);
                // `f64` body for a float reduce (`fadd`/`fmul`/`fcmp`+`select`/`fsqrt`); else i64.
                let kind = if float_reduce { NumKind::Float } else { NumKind::Int };
                let nacc = gen_value(&mut b, &bodies[0], &mut vars, fn_ids, module, kind);
                b.def_var(sink_var, nacc);
            }
        }
        FusionSink::Count => {
            // A surviving element bumps the counter; nothing is stored.
            let w = b.use_var(sink_var);
            let nw = b.ins().iadd_imm(w, 1);
            b.def_var(sink_var, nw);
        }
    }
    b.ins().jump(cont, &[]);

    // cont: advance the cursor and loop. (Predecessors: the sink fall-through plus every
    // filter's reject edge — all emitted above, so seal now.)
    b.switch_to_block(cont);
    b.seal_block(cont);
    let i = b.use_var(idx_var);
    let ni = b.ins().iadd_imm(i, 1);
    b.def_var(idx_var, ni);
    b.ins().jump(header, &[]);
    b.seal_block(header);

    b.switch_to_block(exit);
    b.seal_block(exit);
    if tuple_reduce {
        // Write the folded slots back through `acc_ptr`; no scalar return.
        for (k2, &v) in acc_vars.iter().enumerate() {
            let val = b.use_var(v);
            b.ins().store(MemFlags::trusted(), val, p2, (k2 * 8) as i32);
        }
        b.ins().return_(&[]);
    } else {
        let result = b.use_var(sink_var); // kept count (collect) or accumulator (reduce)
        b.ins().return_(&[result]);
    }

    b.finalize();
    module.define_function(fid, ctx).ok()?;
    module.clear_context(ctx);
    Some(())
}

/// Loop context for a tail-self-recursive function body ([`build`]'s tail branch): the
/// function's own name, its parameter `Variable`s in declaration order (captured BEFORE
/// any `let` shadowing, so the back-edge rebinds the real parameters), the loop
/// header/exit blocks, and the result variable the exit block returns.
struct TailLoop<'p> {
    self_name: &'p str,
    params: &'p [Variable],
    hdr: Block,
    exit: Block,
    ret: Variable,
}

/// Generate a tail-self-recursive function body as a native loop. Tail positions — `if`
/// branches and `let` bodies, exactly what `self_calls_tail_only` admitted — recurse; a
/// tail self-call evaluates ALL its argument values first (they must read the *current*
/// parameters: `go(n - 1, acc + n)` reads the same `n` twice), then rebinds the parameter
/// Variables and jumps back to the header; any other expression is a value position —
/// compute it (self-free by eligibility), store the result, jump to the exit. Every path
/// terminates its block, so there is no merge block: the CFG is exactly a `while` loop.
fn gen_tail<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
    kind: NumKind,
    tl: &TailLoop,
) {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => {
            let then_b = b.create_block();
            let else_b = b.create_block();
            let cv = gen_cond(b, cond, vars, fn_ids, module, kind, None);
            b.ins().brif(cv, then_b, &[], else_b, &[]);
            b.switch_to_block(then_b);
            b.seal_block(then_b);
            gen_tail(b, then_branch, vars, fn_ids, module, kind, tl);
            b.switch_to_block(else_b);
            b.seal_block(else_b);
            gen_tail(b, else_branch, vars, fn_ids, module, kind, tl);
        }
        Expr::Let { bindings, body, .. } => {
            // Same shadow/restore discipline as `gen_value`'s Let — the map mutation
            // must be undone for a sibling `if` branch generated after this subtree.
            let mut saved: Vec<(&'a str, Option<Variable>)> = Vec::new();
            for (n, v) in bindings {
                let vv = gen_value(b, v, vars, fn_ids, module, kind);
                let var = b.declare_var(kind.cl_type());
                b.def_var(var, vv);
                saved.push((n.as_str(), vars.insert(n.as_str(), var)));
            }
            gen_tail(b, body, vars, fn_ids, module, kind, tl);
            for (n, old) in saved.into_iter().rev() {
                match old {
                    Some(o) => {
                        vars.insert(n, o);
                    }
                    None => {
                        vars.remove(n);
                    }
                }
            }
        }
        Expr::Call { name, args, .. } if name == tl.self_name => {
            // The back-edge. Evaluate every argument BEFORE rebinding any parameter —
            // later arguments read the pre-call parameter values, never a fresh rebind.
            let argv: Vec<ClValue> = args
                .iter()
                .map(|a| gen_value(b, a, vars, fn_ids, module, kind))
                .collect();
            for (var, v) in tl.params.iter().zip(argv) {
                b.def_var(*var, v);
            }
            b.ins().jump(tl.hdr, &[]);
        }
        other => {
            let v = gen_value(b, other, vars, fn_ids, module, kind);
            b.def_var(tl.ret, v);
            b.ins().jump(tl.exit, &[]);
        }
    }
}

/// Generate a mixed-eligible VALUE expression over a typed environment, returning the
/// value and its kind. The env-generalization of [`gen_value_typed`], mirroring
/// [`infer_typed_env`] node for node: Int⊗Int arms are byte-identical to `gen_value`'s
/// i64 codegen; a Float side promotes the Int side via `fcvt_from_sint` (the
/// interpreter's numeric promotion); builtins follow the interpreter's kinds exactly.
/// Bail to `poison` before a native `srem`/`sdiv` that would TRAP. Two inputs do:
/// a zero divisor (which the interpreter RAISES on), and `(i64::MIN, -1)` (which the
/// interpreter does NOT raise on — it wraps — but which traps natively, so the VM has to
/// produce it). Both are immediate bails rather than accumulate-and-store, for the reason
/// the `/` arm records: a tail loop can be infinite, so the error cannot wait.
fn div_guard(b: &mut FunctionBuilder, lv: ClValue, rv: ClValue, poison: Block) {
    let zero = b.ins().iconst(I64, 0);
    let is_zero = b.ins().icmp(IntCC::Equal, rv, zero);
    let min = b.ins().iconst(I64, i64::MIN);
    let neg1 = b.ins().iconst(I64, -1);
    let l_min = b.ins().icmp(IntCC::Equal, lv, min);
    let r_neg1 = b.ins().icmp(IntCC::Equal, rv, neg1);
    let overflow = b.ins().band(l_min, r_neg1);
    let bad = b.ins().bor(is_zero, overflow);
    let cont = b.create_block();
    b.ins().brif(bad, poison, &[], cont, &[]);
    b.switch_to_block(cont);
    b.seal_block(cont);
}

/// Bail to `poison` when a shift count leaves `0..=63`, which the interpreter raises on
/// and where a native shift is undefined.
fn shift_guard(b: &mut FunctionBuilder, rv: ClValue, poison: Block) {
    let zero = b.ins().iconst(I64, 0);
    let below = b.ins().icmp(IntCC::SignedLessThan, rv, zero);
    let sixty_four = b.ins().iconst(I64, 64);
    let above = b.ins().icmp(IntCC::SignedGreaterThanOrEqual, rv, sixty_four);
    let bad = b.ins().bor(below, above);
    let cont = b.create_block();
    b.ins().brif(bad, poison, &[], cont, &[]);
    b.switch_to_block(cont);
    b.seal_block(cont);
}

fn gen_value_env<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &HashMap<&'a str, Variable>,
    env: &HashMap<&'a str, NumKind>,
    module: &mut JITModule,
    tl: &MixedTail,
) -> (ClValue, NumKind) {
    match e {
        Expr::Int(i) => (b.ins().iconst(I64, *i), NumKind::Int),
        Expr::Float(f) => (b.ins().f64const(*f), NumKind::Float),
        Expr::Ident { name, .. } => (b.use_var(vars[name.as_str()]), env[name.as_str()]),
        Expr::Binary { op, left, right, .. } => {
            let (lv, lk) = gen_value_env(b, left, vars, env, module, tl);
            let (rv, rk) = gen_value_env(b, right, vars, env, module, tl);
            // `/` is ALWAYS float division in Helix (`10 / 2 == 5.0`), so it takes the f64
            // branch even for two `Int` operands.
            if lk == NumKind::Int && rk == NumKind::Int && !matches!(op, BinOp::Div) {
                let v = match op {
                    BinOp::Add => b.ins().iadd(lv, rv),
                    BinOp::Sub => b.ins().isub(lv, rv),
                    BinOp::Mul => b.ins().imul(lv, rv),
                    // `%` and `//` are EUCLIDEAN in Helix, not truncating:
                    //   7 % -3 == 1   7 // -3 == -2      -7 % 3 == 2   -7 // 3 == -3
                    // The old lowering was correct only because the gate guaranteed a
                    // POSITIVE constant — adding `rv` back is `rem_euclid` only for
                    // `rv > 0`, and subtracting one is floor only for `rv > 0`. These are
                    // the general forms, matching `i64::rem_euclid` / `div_euclid`
                    // instruction for instruction, wrapping included.
                    BinOp::Mod => {
                        div_guard(b, lv, rv, tl.poison_blk);
                        let zero = b.ins().iconst(I64, 0);
                        let rem = b.ins().srem(lv, rv);
                        // `wrapping_abs`, which is what `rem_euclid` uses: `abs(i64::MIN)`
                        // is not representable and wraps back to itself, and the `iadd`
                        // below wraps with it — so this stays exact for every divisor.
                        let neg_r = b.ins().ineg(rv);
                        let r_pos = b.ins().icmp(IntCC::SignedGreaterThan, rv, zero);
                        let abs_r = b.ins().select(r_pos, rv, neg_r);
                        let fixed = b.ins().iadd(rem, abs_r);
                        let is_neg = b.ins().icmp(IntCC::SignedLessThan, rem, zero);
                        b.ins().select(is_neg, fixed, rem)
                    }
                    BinOp::FloorDiv => {
                        div_guard(b, lv, rv, tl.poison_blk);
                        let zero = b.ins().iconst(I64, 0);
                        let q = b.ins().sdiv(lv, rv);
                        let rem = b.ins().srem(lv, rv);
                        // `div_euclid`: step the quotient AWAY from zero when the
                        // remainder is negative — down for a positive divisor, up for a
                        // negative one. The old `q - 1` was the `rv > 0` half only.
                        let qm1 = b.ins().iadd_imm(q, -1);
                        let qp1 = b.ins().iadd_imm(q, 1);
                        let r_pos = b.ins().icmp(IntCC::SignedGreaterThan, rv, zero);
                        let adj = b.ins().select(r_pos, qm1, qp1);
                        let is_neg = b.ins().icmp(IntCC::SignedLessThan, rem, zero);
                        b.ins().select(is_neg, adj, q)
                    }
                    BinOp::BitAnd => b.ins().band(lv, rv),
                    BinOp::BitOr => b.ins().bor(lv, rv),
                    BinOp::BitXor => b.ins().bxor(lv, rv),
                    // A constant in range keeps the immediate form (no count register,
                    // no guard); anything else is guarded and shifted by the register.
                    BinOp::Shl => match **right {
                        Expr::Int(n) if (0..=63).contains(&n) => b.ins().ishl_imm(lv, n),
                        _ => {
                            shift_guard(b, rv, tl.poison_blk);
                            b.ins().ishl(lv, rv)
                        }
                    },
                    // `>>` on i64 is arithmetic (sign-extending) in Rust -> `sshr`.
                    BinOp::Shr => match **right {
                        Expr::Int(n) if (0..=63).contains(&n) => b.ins().sshr_imm(lv, n),
                        _ => {
                            shift_guard(b, rv, tl.poison_blk);
                            b.ins().sshr(lv, rv)
                        }
                    },
                    _ => unreachable!("ineligible operator reached mixed-env codegen"),
                };
                (v, NumKind::Int)
            } else {
                let lf = if lk == NumKind::Int { b.ins().fcvt_from_sint(F64, lv) } else { lv };
                let rf = if rk == NumKind::Int { b.ins().fcvt_from_sint(F64, rv) } else { rv };
                let v = match op {
                    BinOp::Add => b.ins().fadd(lf, rf),
                    BinOp::Sub => b.ins().fsub(lf, rf),
                    BinOp::Mul => b.ins().fmul(lf, rf),
                    // Any eligible divisor. The interpreter RAISES on a zero divisor while
                    // native `fdiv` would yield inf/nan — so bail IMMEDIATELY to the poison
                    // block, exactly like the NaN-compare bail and for the same reason: a
                    // tail loop can be infinite, so the error cannot wait for an
                    // accumulate-and-store. `rf == 0.0` also catches `-0.0`, matching the
                    // interpreter's `b == 0.0` divisor check bit for bit.
                    BinOp::Div => {
                        let zero = b.ins().f64const(0.0);
                        let is_zero = b.ins().fcmp(FloatCC::Equal, rf, zero);
                        let cont = b.create_block();
                        b.ins().brif(is_zero, tl.poison_blk, &[], cont, &[]);
                        b.switch_to_block(cont);
                        b.seal_block(cont);
                        b.ins().fdiv(lf, rf)
                    }
                    _ => unreachable!("ineligible operator reached mixed-env codegen"),
                };
                (v, NumKind::Float)
            }
        }
        Expr::Unary { op: UnOp::Neg, expr, .. } => {
            let (v, k) = gen_value_env(b, expr, vars, env, module, tl);
            match k {
                NumKind::Int => (b.ins().ineg(v), NumKind::Int),
                NumKind::Float => (b.ins().fneg(v), NumKind::Float),
            }
        }
        Expr::Call { name, args, .. } => {
            if let Some(sig) = tl.sigs.get(name.as_str()) {
                // A mixed sibling: marshal args to the bits ABI (Float → raw bits in
                // i64 slots), pass OUR poison pointer as its trailing slot, call.
                let fref = module.declare_func_in_func(tl.ids[name.as_str()], b.func);
                let mut argv: Vec<ClValue> = Vec::with_capacity(args.len() + 1);
                for (a, &k) in args.iter().zip(&sig.params) {
                    let (v, ak) = gen_value_env(b, a, vars, env, module, tl);
                    debug_assert!(ak == k, "mixed-call arg kind drifted from the callee sig");
                    let _ = k;
                    argv.push(match ak {
                        NumKind::Int => v,
                        NumKind::Float => b.ins().bitcast(I64, MemFlags::new(), v),
                    });
                }
                argv.push(tl.poison_ptr);
                let call = b.ins().call(fref, &argv);
                let raw = b.inst_results(call)[0];
                // The callee shares our poison flag: if it NaN-bailed, bail HERE too —
                // otherwise this body could keep looping on a garbage 0 result where
                // the bytecode re-run would have raised immediately.
                let p = b.ins().load(I8, MemFlags::trusted(), tl.poison_ptr, 0);
                let cont = b.create_block();
                b.ins().brif(p, tl.poison_blk, &[], cont, &[]);
                b.switch_to_block(cont);
                b.seal_block(cont);
                let v = match sig.ret {
                    NumKind::Int => raw,
                    NumKind::Float => b.ins().bitcast(F64, MemFlags::new(), raw),
                };
                return (v, sig.ret);
            }
            match name.as_str() {
                "sqrt" => {
                    let (av, ak) = gen_value_env(b, &args[0], vars, env, module, tl);
                    let af =
                        if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                    (b.ins().sqrt(af), NumKind::Float)
                }
                // `to_float` IS that promotion with nothing applied after it: an `i64` becomes
                // `f64` via `fcvt_from_sint` (the interpreter's `*i as f64`), and an `f64`
                // passes through unchanged.
                "to_float" => {
                    let (av, ak) = gen_value_env(b, &args[0], vars, env, module, tl);
                    let af =
                        if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                    (af, NumKind::Float)
                }
                "abs" => {
                    let (av, ak) = gen_value_env(b, &args[0], vars, env, module, tl);
                    match ak {
                        NumKind::Int => (b.ins().iabs(av), NumKind::Int),
                        NumKind::Float => (b.ins().fabs(av), NumKind::Float),
                    }
                }
                // `to_int`: saturating float->int, the identity on an `Int`. `fcvt_to_sint_sat`
                // matches the interpreter exactly -- NaN to 0, +-inf to the i64 extremes.
                "to_int" => {
                    let (av, ak) = gen_value_env(b, &args[0], vars, env, module, tl);
                    match ak {
                        NumKind::Int => (av, NumKind::Int),
                        NumKind::Float => (b.ins().fcvt_to_sint_sat(I64, av), NumKind::Int),
                    }
                }
                // `sign`: 1 / -1 / 0. Both comparisons are FALSE for NaN, so the selects fall
                // through to 0 -- which is what the interpreter returns for NaN (it compares
                // rather than using `signum`, which would propagate NaN).
                "sign" => {
                    let (av, ak) = gen_value_env(b, &args[0], vars, env, module, tl);
                    let one = b.ins().iconst(I64, 1);
                    let neg = b.ins().iconst(I64, -1);
                    let zero = b.ins().iconst(I64, 0);
                    let (gt, lt) = match ak {
                        NumKind::Int => {
                            let z = b.ins().iconst(I64, 0);
                            (
                                b.ins().icmp(IntCC::SignedGreaterThan, av, z),
                                b.ins().icmp(IntCC::SignedLessThan, av, z),
                            )
                        }
                        NumKind::Float => {
                            let z = b.ins().f64const(0.0);
                            (
                                b.ins().fcmp(FloatCC::GreaterThan, av, z),
                                b.ins().fcmp(FloatCC::LessThan, av, z),
                            )
                        }
                    };
                    let lo = b.ins().select(lt, neg, zero);
                    (b.ins().select(gt, one, lo), NumKind::Int)
                }
                "min" | "max" => {
                    let (av, ak) = gen_value_env(b, &args[0], vars, env, module, tl);
                    let (cv, _ck) = gen_value_env(b, &args[1], vars, env, module, tl);
                    let le = name == "min";
                    let cc =
                        if le { FloatCC::LessThanOrEqual } else { FloatCC::GreaterThanOrEqual };
                    match ak {
                        NumKind::Int => {
                            let af = b.ins().fcvt_from_sint(F64, av);
                            let cf = b.ins().fcvt_from_sint(F64, cv);
                            let keep = b.ins().fcmp(cc, af, cf);
                            (b.ins().select(keep, av, cv), NumKind::Int)
                        }
                        NumKind::Float => {
                            let keep = b.ins().fcmp(cc, av, cv);
                            (b.ins().select(keep, av, cv), NumKind::Float)
                        }
                    }
                }
                _ => unreachable!("ineligible call reached mixed-env codegen"),
            }
        }
        _ => unreachable!("ineligible mixed-env expr reached codegen"),
    }
}

/// Generate a mixed-eligible condition (see [`cond_typed_ok`]): `and`/`or` as
/// `band`/`bor` of sub-conditions (both sides pure, so eager evaluation is
/// observationally identical to short-circuiting for VALUES — and a NaN in an eagerly-
/// evaluated side that the interpreter would have short-circuited past just triggers
/// the poison FALLBACK, which re-runs on bytecode and short-circuits correctly);
/// comparisons pick `icmp`/`fcmp` by their (same-kind, per eligibility) operand kind.
///
/// Every FLOAT comparison is NaN-guarded: the interpreter RAISES on an unordered
/// compare, so `fcmp Unordered` branches to the poison block FIRST — the ordered
/// compare only runs on ordered operands. The bail must be immediate: a tail loop can
/// be infinite, and a NaN inside one must error like the interpreter, not spin.
fn gen_cond_env<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &HashMap<&'a str, Variable>,
    env: &HashMap<&'a str, NumKind>,
    module: &mut JITModule,
    tl: &MixedTail,
) -> ClValue {
    match e {
        Expr::Binary { op: BinOp::And, left, right, .. } => {
            let l = gen_cond_env(b, left, vars, env, module, tl);
            let r = gen_cond_env(b, right, vars, env, module, tl);
            b.ins().band(l, r)
        }
        Expr::Binary { op: BinOp::Or, left, right, .. } => {
            let l = gen_cond_env(b, left, vars, env, module, tl);
            let r = gen_cond_env(b, right, vars, env, module, tl);
            b.ins().bor(l, r)
        }
        Expr::Binary { op, left, right, .. } => {
            let (l, lk) = gen_value_env(b, left, vars, env, module, tl);
            let (r, _rk) = gen_value_env(b, right, vars, env, module, tl);
            match lk {
                NumKind::Int => {
                    let cc = match op {
                        BinOp::Lt => IntCC::SignedLessThan,
                        BinOp::Gt => IntCC::SignedGreaterThan,
                        BinOp::Le => IntCC::SignedLessThanOrEqual,
                        BinOp::Ge => IntCC::SignedGreaterThanOrEqual,
                        BinOp::Eq => IntCC::Equal,
                        BinOp::Ne => IntCC::NotEqual,
                        _ => unreachable!("only comparisons reach mixed cond codegen"),
                    };
                    b.ins().icmp(cc, l, r)
                }
                NumKind::Float => {
                    // NaN bail: unordered operands → poison block (→ VM fallback →
                    // the interpreter's "cannot compare these values (NaN?)" error).
                    let uno = b.ins().fcmp(FloatCC::Unordered, l, r);
                    let ordered = b.create_block();
                    b.ins().brif(uno, tl.poison_blk, &[], ordered, &[]);
                    b.switch_to_block(ordered);
                    b.seal_block(ordered);
                    let cc = match op {
                        BinOp::Lt => FloatCC::LessThan,
                        BinOp::Gt => FloatCC::GreaterThan,
                        BinOp::Le => FloatCC::LessThanOrEqual,
                        BinOp::Ge => FloatCC::GreaterThanOrEqual,
                        BinOp::Eq => FloatCC::Equal,
                        BinOp::Ne => FloatCC::NotEqual,
                        _ => unreachable!("only comparisons reach mixed cond codegen"),
                    };
                    b.ins().fcmp(cc, l, r)
                }
            }
        }
        _ => unreachable!("ineligible condition reached mixed cond codegen"),
    }
}

/// Loop context for a MIXED tail-recursive body ([`build`]'s mixed pass) — the typed
/// sibling of [`TailLoop`]: parameter Variables AND their kinds in declaration order,
/// plus the NaN-poison machinery (see [`MixedFn`]): float comparisons bail to
/// `poison_blk` on an unordered operand, mirroring the interpreter's NaN-compare error.
struct MixedTail<'p> {
    self_name: &'p str,
    params: &'p [Variable],
    param_kinds: &'p [NumKind],
    hdr: Block,
    exit: Block,
    ret: Variable,
    /// Target of the NaN-compare / poisoned-callee bail; it stores 1 through the
    /// poison pointer and returns.
    poison_blk: Block,
    /// This function's poison out-param — passed through to mixed CALLEES (one shared
    /// bail flag for the whole native call chain) and checked after each callee call.
    poison_ptr: ClValue,
    /// The mixed siblings this body may call (all declared before any body is
    /// defined, so a later function can call an earlier one — `escape` → `step`).
    sigs: &'p HashMap<&'p str, MixedSig>,
    /// Their codegen identities, keyed by the same names — parallel to `sigs`, which is
    /// id-free so its inference can also run at bytecode-compile time.
    ids: &'p HashMap<&'p str, FuncId>,
}

/// Generate a mixed tail-recursive body as a native loop — [`gen_tail`]'s typed sibling,
/// with the SAME structure (if branches / let bodies recurse; a tail self-call evaluates
/// ALL argument values before rebinding any parameter Variable; a value position stores
/// the result and jumps to the exit) but per-node kinds via [`gen_value_env`] /
/// [`gen_cond_env`], and `let` threading BOTH the Variable map and the kind env.
fn gen_tail_mixed<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &mut HashMap<&'a str, Variable>,
    env: &mut HashMap<&'a str, NumKind>,
    module: &mut JITModule,
    tl: &MixedTail,
) {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => {
            let then_b = b.create_block();
            let else_b = b.create_block();
            let cv = gen_cond_env(b, cond, vars, env, module, tl);
            b.ins().brif(cv, then_b, &[], else_b, &[]);
            b.switch_to_block(then_b);
            b.seal_block(then_b);
            gen_tail_mixed(b, then_branch, vars, env, module, tl);
            b.switch_to_block(else_b);
            b.seal_block(else_b);
            gen_tail_mixed(b, else_branch, vars, env, module, tl);
        }
        Expr::Let { bindings, body, .. } => {
            let mut saved: Vec<(&'a str, Option<Variable>, Option<NumKind>)> = Vec::new();
            for (n, v) in bindings {
                let (vv, vk) = gen_value_env(b, v, vars, env, module, tl);
                let var = b.declare_var(vk.cl_type());
                b.def_var(var, vv);
                saved.push((
                    n.as_str(),
                    vars.insert(n.as_str(), var),
                    env.insert(n.as_str(), vk),
                ));
            }
            gen_tail_mixed(b, body, vars, env, module, tl);
            for (n, old_var, old_kind) in saved.into_iter().rev() {
                match old_var {
                    Some(o) => {
                        vars.insert(n, o);
                    }
                    None => {
                        vars.remove(n);
                    }
                }
                match old_kind {
                    Some(o) => {
                        env.insert(n, o);
                    }
                    None => {
                        env.remove(n);
                    }
                }
            }
        }
        Expr::Call { name, args, .. } if name == tl.self_name => {
            // Evaluate every argument BEFORE rebinding any parameter (same discipline as
            // `gen_tail`); eligibility (`mixed_tail_ret_kind`) proved each arg's kind
            // equals the annotated param kind — re-asserted here so any drift between
            // the two walkers fails fast instead of emitting a wrongly-typed rebind.
            let argv: Vec<ClValue> = args
                .iter()
                .zip(tl.param_kinds)
                .map(|(a, &k)| {
                    let (v, ak) = gen_value_env(b, a, vars, env, module, tl);
                    debug_assert!(ak == k, "tail-call arg kind drifted from the param kind");
                    let _ = k;
                    v
                })
                .collect();
            for (var, v) in tl.params.iter().zip(argv) {
                b.def_var(*var, v);
            }
            b.ins().jump(tl.hdr, &[]);
        }
        other => {
            let (v, _k) = gen_value_env(b, other, vars, env, module, tl);
            b.def_var(tl.ret, v);
            b.ins().jump(tl.exit, &[]);
        }
    }
}

fn gen_value<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
    kind: NumKind,
) -> ClValue {
    match e {
        Expr::Int(i) => match kind {
            NumKind::Int => b.ins().iconst(I64, *i),
            NumKind::Float => b.ins().f64const(*i as f64),
        },
        Expr::Float(f) => b.ins().f64const(*f),
        Expr::Ident { name, .. } => {
            let var = vars[name.as_str()];
            b.use_var(var)
        }
        Expr::Binary { op, left, right, .. } => {
            let l = gen_value(b, left, vars, fn_ids, module, kind);
            let r = gen_value(b, right, vars, fn_ids, module, kind);
            match (kind, op) {
                (NumKind::Int, BinOp::Add) => b.ins().iadd(l, r),
                (NumKind::Int, BinOp::Sub) => b.ins().isub(l, r),
                (NumKind::Int, BinOp::Mul) => b.ins().imul(l, r),
                (NumKind::Int, BinOp::Mod) => {
                    // `a.rem_euclid(b)` for a positive constant `b` (guaranteed by
                    // `value_eligible`): `r = a % b; if r < 0 { r + b } else { r }`.
                    // `b > 0` also avoids the `srem(i64::MIN, -1)` overflow trap.
                    let rem = b.ins().srem(l, r);
                    let zero = b.ins().iconst(I64, 0);
                    let fixed = b.ins().iadd(rem, r);
                    let is_neg = b.ins().icmp(IntCC::SignedLessThan, rem, zero);
                    b.ins().select(is_neg, fixed, rem)
                }
                (NumKind::Int, BinOp::BitAnd) => b.ins().band(l, r),
                (NumKind::Int, BinOp::BitOr) => b.ins().bor(l, r),
                (NumKind::Int, BinOp::BitXor) => b.ins().bxor(l, r),
                (NumKind::Int, BinOp::Shl) => {
                    // Constant shift in 0..=63 (guaranteed by `value_eligible`); the
                    // immediate form avoids computing a throwaway count value.
                    let n = if let Expr::Int(n) = **right { n } else { unreachable!() };
                    b.ins().ishl_imm(l, n)
                }
                (NumKind::Int, BinOp::Shr) => {
                    // `>>` on i64 is arithmetic (sign-extending) in Rust → `sshr`.
                    let n = if let Expr::Int(n) = **right { n } else { unreachable!() };
                    b.ins().sshr_imm(l, n)
                }
                (NumKind::Int, BinOp::FloorDiv) => {
                    // `a.div_euclid(d)` for a positive constant `d`: `q = a / d`, then
                    // `q - 1` when the remainder is negative (floor for `d > 0`). A
                    // positive constant `d` keeps `sdiv`/`srem` trap-free.
                    let q = b.ins().sdiv(l, r);
                    let rem = b.ins().srem(l, r);
                    let zero = b.ins().iconst(I64, 0);
                    let is_neg = b.ins().icmp(IntCC::SignedLessThan, rem, zero);
                    let qm1 = b.ins().iadd_imm(q, -1);
                    b.ins().select(is_neg, qm1, q)
                }
                (NumKind::Float, BinOp::Add) => b.ins().fadd(l, r),
                (NumKind::Float, BinOp::Sub) => b.ins().fsub(l, r),
                (NumKind::Float, BinOp::Mul) => b.ins().fmul(l, r),
                // `Div` is deliberately not eligible (see `value_eligible`): native
                // `fdiv` returns inf where the interpreter errors on /0.
                _ => unreachable!("ineligible operator reached codegen"),
            }
        }
        Expr::Call { name, args, .. } => {
            // An eligible user function is in `fn_ids` (call it); otherwise eligibility
            // guaranteed a recognized pure scalar builtin (emit it inline). The `fn_ids`
            // lookup is what makes a user function shadowing `min`/`max`/`abs` dispatch to
            // the user's function, never the builtin op.
            if let Some(&fid) = fn_ids.get(name.as_str()) {
                let fref = module.declare_func_in_func(fid, b.func);
                let argv: Vec<ClValue> = args
                    .iter()
                    .map(|a| gen_value(b, a, vars, fn_ids, module, kind))
                    .collect();
                let call = b.ins().call(fref, &argv);
                b.inst_results(call)[0]
            } else if kind == NumKind::Float {
                gen_builtin_f64(b, name, args, vars, fn_ids, module)
            } else {
                gen_builtin_i64(b, name, args, vars, fn_ids, module)
            }
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            let then_b = b.create_block();
            let else_b = b.create_block();
            let merge_b = b.create_block();

            let cv = gen_cond(b, cond, vars, fn_ids, module, kind, None);
            b.ins().brif(cv, then_b, &[], else_b, &[]);

            let rvar = b.declare_var(kind.cl_type());

            b.switch_to_block(then_b);
            b.seal_block(then_b);
            let tv = gen_value(b, then_branch, vars, fn_ids, module, kind);
            b.def_var(rvar, tv);
            b.ins().jump(merge_b, &[]);

            b.switch_to_block(else_b);
            b.seal_block(else_b);
            let ev = gen_value(b, else_branch, vars, fn_ids, module, kind);
            b.def_var(rvar, ev);
            b.ins().jump(merge_b, &[]);

            b.switch_to_block(merge_b);
            b.seal_block(merge_b);
            b.use_var(rvar)
        }
        Expr::Let { bindings, body, .. } => {
            let mut saved: Vec<(&'a str, Option<Variable>)> = Vec::new();
            for (n, v) in bindings {
                let vv = gen_value(b, v, vars, fn_ids, module, kind);
                let var = b.declare_var(kind.cl_type());
                b.def_var(var, vv);
                saved.push((n.as_str(), vars.insert(n.as_str(), var)));
            }
            let r = gen_value(b, body, vars, fn_ids, module, kind);
            for (n, old) in saved.into_iter().rev() {
                match old {
                    Some(o) => {
                        vars.insert(n, o);
                    }
                    None => {
                        vars.remove(n);
                    }
                }
            }
            r
        }
        // Unary negation: `ineg` wraps like the interpreter's `wrapping_neg`; `fneg`
        // is the exact IEEE sign flip of the interpreter's `-f`.
        Expr::Unary { op: UnOp::Neg, expr, .. } => {
            let v = gen_value(b, expr, vars, fn_ids, module, kind);
            match kind {
                NumKind::Int => b.ins().ineg(v),
                NumKind::Float => b.ins().fneg(v),
            }
        }
        Expr::Match { scrutinee, arms, .. } => gen_match(b, scrutinee, arms, vars, fn_ids, module, kind),
        // `arr[counter]` in a reduce kernel: `recv` is an array capture whose base pointer
        // was loaded into `vars` (an i64 slot holding the pointer), `index` is the i64 loop
        // counter. The VM pre-checked the whole counter range is in bounds, so this raw load
        // is safe. Only the reduce-indexed path admits `Index` (map/filter eligibility rejects
        // it), and that path is always `i64` today, so the element load is `I64` (the f64
        // element variant lands with its own codegen). Address = base + idx*8.
        Expr::Index { recv, index, .. } => {
            let name = match &**recv {
                Expr::Ident { name, .. } => name.as_str(),
                _ => unreachable!("ineligible index receiver reached codegen"),
            };
            let base = b.use_var(vars[name]);
            let idx = gen_value(b, index, vars, fn_ids, module, NumKind::Int);
            let off = b.ins().imul_imm(idx, 8);
            let addr = b.ins().iadd(base, off);
            b.ins().load(I64, MemFlags::trusted(), addr, 0)
        }
        _ => unreachable!("ineligible node reached codegen"),
    }
}

/// Lower an `i64`-scrutinee `match` to an if/else chain (arms tried in order; the first
/// whose pattern matches *and* guard holds wins). Eligibility (`match_eligible`) guarantees
/// the final arm is an unguarded catch-all (`_`/binder), so some arm always yields a value
/// — the native code is total, matching every reachable case of the interpreter. A `Bind`
/// pattern binds the scrutinee for that arm's guard and body. Guards are pure `i64`
/// conditions, so evaluating one whose pattern didn't match is harmless (same value).
fn gen_match<'a>(
    b: &mut FunctionBuilder,
    scrutinee: &'a Expr,
    arms: &'a [crate::ast::MatchArm],
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
    kind: NumKind,
) -> ClValue {
    use crate::ast::Pattern;
    let sv = gen_value(b, scrutinee, vars, fn_ids, module, kind);
    let rvar = b.declare_var(kind.cl_type());
    let merge = b.create_block();
    for arm in arms {
        // A `Bind` introduces its name (= the scrutinee value) for this arm's guard + body.
        let saved = if let Pattern::Bind(name) = &arm.pattern {
            let var = b.declare_var(I64);
            b.def_var(var, sv);
            Some((name.as_str(), vars.insert(name.as_str(), var)))
        } else {
            None
        };
        let unconditional =
            arm.guard.is_none() && matches!(arm.pattern, Pattern::Wildcard | Pattern::Bind(_));
        if unconditional {
            let bv = gen_value(b, &arm.body, vars, fn_ids, module, kind);
            b.def_var(rvar, bv);
            b.ins().jump(merge, &[]);
            if let Some((name, prev)) = saved {
                restore_var(vars, name, prev);
            }
            break; // a catch-all is terminal — later arms are unreachable
        }
        let cond = gen_arm_cond(b, sv, &arm.pattern, &arm.guard, vars, fn_ids, module);
        let take = b.create_block();
        let next = b.create_block();
        b.ins().brif(cond, take, &[], next, &[]);
        b.switch_to_block(take);
        b.seal_block(take);
        let bv = gen_value(b, &arm.body, vars, fn_ids, module, kind);
        b.def_var(rvar, bv);
        b.ins().jump(merge, &[]);
        b.switch_to_block(next);
        b.seal_block(next);
        if let Some((name, prev)) = saved {
            restore_var(vars, name, prev);
        }
    }
    b.switch_to_block(merge);
    b.seal_block(merge);
    b.use_var(rvar)
}

fn restore_var<'a>(vars: &mut HashMap<&'a str, Variable>, name: &'a str, prev: Option<Variable>) {
    match prev {
        Some(p) => {
            vars.insert(name, p);
        }
        None => {
            vars.remove(name);
        }
    }
}

/// The boolean condition for taking a (non-catch-all) match arm: the pattern test ANDed
/// with the guard (if any). `Int(n)` → `sv == n`; `Or([n…])` → the OR of those; a guarded
/// `Bind`/`_` has no pattern test, so the condition is the guard alone.
fn gen_arm_cond<'a>(
    b: &mut FunctionBuilder,
    sv: ClValue,
    pattern: &'a crate::ast::Pattern,
    guard: &'a Option<Expr>,
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
) -> ClValue {
    use crate::ast::Pattern;
    let pat = match pattern {
        Pattern::Int(n) => Some(b.ins().icmp_imm(IntCC::Equal, sv, *n)),
        Pattern::Or(alts) => {
            let mut acc: Option<ClValue> = None;
            for p in alts {
                if let Pattern::Int(n) = p {
                    let eq = b.ins().icmp_imm(IntCC::Equal, sv, *n);
                    acc = Some(match acc {
                        Some(a) => b.ins().bor(a, eq),
                        None => eq,
                    });
                }
            }
            acc
        }
        Pattern::Wildcard | Pattern::Bind(_) => None,
        _ => unreachable!("ineligible match pattern reached codegen"),
    };
    let g = guard.as_ref().map(|g| gen_cond(b, g, vars, fn_ids, module, NumKind::Int, None));
    match (pat, g) {
        (Some(p), Some(g)) => b.ins().band(p, g),
        (Some(p), None) => p,
        (None, Some(g)) => g,
        (None, None) => unreachable!("unconditional arm handled in gen_match"),
    }
}

/// Codegen for a **mixed** map body (`Int` element → `Float` result): returns the value
/// and its `NumKind`. Types each node bottom-up by the interpreter's promotion rule —
/// `Int OP Int` emits the wrapping `i64` op (`iadd/isub/imul`, exactly the pure-`i64`
/// kernel's semantics), and as soon as either side is `Float` both are promoted to `f64`
/// via `fcvt_from_sint` and the `f64` op runs. This reproduces the interpreter's
/// "`i64` arithmetic until a float enters, then `as f64`" behavior bit-for-bit, including
/// an `i64` wrap that happens *before* the float promotion. Only the binder (an `i64`
/// element), int/float literals, and `+ - *` reach here (guaranteed by `mixed_map_eligible`).
/// Everything a map kernel needs in order to CALL a mixed specialization: the signatures to
/// marshal by, their codegen identities, and a stack cell to hand the callee as its poison
/// out-param (the kernel's own poison is a register accumulator, and the mixed ABI wants a
/// pointer). Bundled so the walker's signature stays readable.
struct MixedCallCtx<'c> {
    sigs: &'c MixedSigTable,
    ids: &'c HashMap<&'c str, FuncId>,
    poison_cell: cranelift_codegen::ir::StackSlot,
}

/// The two mixed-specialization tables a kernel BUILDER needs in order to hand its codegen a
/// [`MixedCallCtx`]. Separate from that type because the poison cell can only be created once
/// there is a `FunctionBuilder`, which is inside the builder, not at its call site.
struct MixedTables<'c> {
    sigs: &'c MixedSigTable,
    ids: &'c HashMap<&'c str, FuncId>,
}

/// Everything [`gen_value_typed`] threads through its recursion unchanged. Bundled for the
/// same reason [`MixedCallCtx`] is: the walker calls itself fourteen times, and repeating
/// seven identical arguments at each one buries the single argument that actually differs.
struct TypedCtx<'a, 'c> {
    /// Kernel-local Cranelift variables, keyed by the name the body uses.
    vars: &'c HashMap<&'a str, Variable>,
    /// The map/filter binder — an `i64` element, never a capture.
    binder: &'c str,
    /// Captures the prologue loaded as `F64`, so the `Ident` arm types them `Float`.
    f64_scalars: &'c HashSet<&'a str>,
    /// Monomorphized user functions this kernel may call directly.
    fn_ids: &'c HashMap<&'a str, FuncId>,
    module: &'c mut JITModule,
    mixed: &'c MixedCallCtx<'c>,
    /// The kernel's poison accumulator (`i64`, 0 = clean). The raising-rounder arm ORs 1 into
    /// it on any out-of-i64-range result; every kernel declares the variable (a non-raising
    /// body just never touches it) so this needs no `Option` plumbing.
    poison: Variable,
}

fn gen_value_typed<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    cx: &mut TypedCtx<'a, '_>,
) -> (ClValue, NumKind) {
    match e {
        Expr::Int(i) => (b.ins().iconst(I64, *i), NumKind::Int),
        Expr::Float(f) => (b.ins().f64const(*f), NumKind::Float),
        // The twin of `infer_mixed_kind`'s `Neg` arm. `ineg` wraps like the interpreter's
        // `wrapping_neg`; `fneg` is its exact IEEE sign flip, so `-0.0` and a NaN payload
        // behave here as they do everywhere else.
        Expr::Unary { op: UnOp::Neg, expr, .. } => {
            let (v, k) =
                gen_value_typed(b, expr, cx);
            match k {
                NumKind::Int => (b.ins().ineg(v), NumKind::Int),
                NumKind::Float => (b.ins().fneg(v), NumKind::Float),
            }
        }
        Expr::Ident { name, .. } => {
            debug_assert!(
                name == cx.binder || cx.vars.contains_key(name.as_str()),
                "unbound ident reached cx.mixed codegen"
            );
            let var = b.use_var(cx.vars[name.as_str()]);
            // A `ScalarValue` cap rides as `f64` in this kernel (SAXPY's coefficient), so its
            // slot IS a float — the prologue loaded it `F64`. Everything else here (the binder,
            // an index scalar) is an `i64` element.
            if cx.f64_scalars.contains(name.as_str()) {
                (var, NumKind::Float)
            } else {
                (var, NumKind::Int)
            }
        }
        // `a[binder]` / `a[scalar_cap]` reading a captured **f64** array: `vars[recv]` holds
        // the base POINTER (the prologue loads non-Scalar caps as I64 regardless of the
        // kernel's element type), and the VM discharged this access's `IndexBound` before
        // dispatch — see `map_index_caps` — so the raw F64 load is safe. The marshal is the
        // type guard: an `Ints` array declines there, so these 8 bytes are always an `f64`.
        Expr::Index { recv, index, .. } => {
            let name = match &**recv {
                Expr::Ident { name, .. } => name.as_str(),
                _ => unreachable!("ineligible index receiver reached cx.mixed codegen"),
            };
            let base = b.use_var(cx.vars[name]);
            let (idx, ik) = gen_value_typed(b, index, cx);
            debug_assert!(matches!(ik, NumKind::Int), "non-i64 index reached cx.mixed codegen");
            let off = b.ins().imul_imm(idx, 8);
            let addr = b.ins().iadd(base, off);
            (b.ins().load(F64, MemFlags::trusted(), addr, 0), NumKind::Float)
        }
        Expr::Binary { op, left, right, .. } => {
            let (lv, lk) = gen_value_typed(b, left, cx);
            let (rv, rk) = gen_value_typed(b, right, cx);
            // `/` is ALWAYS float division in Helix (`10 / 2 == 5.0`), so it takes the f64
            // branch even for two `Int` operands.
            if lk == NumKind::Int && rk == NumKind::Int && !matches!(op, BinOp::Div) {
                // Integer subexpression — identical codegen to `gen_value`'s i64 arms (euclidean
                // `%`/`//` by a positive const, bitwise, const shifts), so a mixed body's integer
                // part is bit-exact to the interpreter, same as the i64 map/reduce.
                let v = match op {
                    BinOp::Add => b.ins().iadd(lv, rv),
                    BinOp::Sub => b.ins().isub(lv, rv),
                    BinOp::Mul => b.ins().imul(lv, rv),
                    BinOp::Mod => {
                        let rem = b.ins().srem(lv, rv);
                        let zero = b.ins().iconst(I64, 0);
                        let fixed = b.ins().iadd(rem, rv);
                        let is_neg = b.ins().icmp(IntCC::SignedLessThan, rem, zero);
                        b.ins().select(is_neg, fixed, rem)
                    }
                    BinOp::FloorDiv => {
                        let q = b.ins().sdiv(lv, rv);
                        let rem = b.ins().srem(lv, rv);
                        let zero = b.ins().iconst(I64, 0);
                        let is_neg = b.ins().icmp(IntCC::SignedLessThan, rem, zero);
                        let qm1 = b.ins().iadd_imm(q, -1);
                        b.ins().select(is_neg, qm1, q)
                    }
                    BinOp::BitAnd => b.ins().band(lv, rv),
                    BinOp::BitOr => b.ins().bor(lv, rv),
                    BinOp::BitXor => b.ins().bxor(lv, rv),
                    BinOp::Shl => {
                        let n = if let Expr::Int(n) = **right { n } else { unreachable!() };
                        b.ins().ishl_imm(lv, n)
                    }
                    BinOp::Shr => {
                        let n = if let Expr::Int(n) = **right { n } else { unreachable!() };
                        b.ins().sshr_imm(lv, n)
                    }
                    _ => unreachable!("ineligible operator reached cx.mixed codegen"),
                };
                (v, NumKind::Int)
            } else {
                // at least one side is Float → promote the Int side(s) to f64, then fop
                let lf = if lk == NumKind::Int { b.ins().fcvt_from_sint(F64, lv) } else { lv };
                let rf = if rk == NumKind::Int { b.ins().fcvt_from_sint(F64, rv) } else { rv };
                let v = match op {
                    BinOp::Add => b.ins().fadd(lf, rf),
                    BinOp::Sub => b.ins().fsub(lf, rf),
                    BinOp::Mul => b.ins().fmul(lf, rf),
                    // The interpreter RAISES on a zero divisor where native `fdiv` yields
                    // inf/nan — so OR `divisor == 0.0` into the poison accumulator (this is
                    // a MAP body: the loop always terminates, so accumulate-and-store is
                    // sound, unlike the mixed-FUNCTION tail loop whose bail must be
                    // immediate). The VM discards the whole output on poison and the
                    // bytecode loop re-runs to raise the exact error. `body_raises`
                    // counts any `/`, so a dividing kernel always has the poison signature.
                    // `rf == 0.0` also catches `-0.0`, matching the interpreter's check.
                    BinOp::Div => {
                        let zero = b.ins().f64const(0.0);
                        let is_zero = b.ins().fcmp(FloatCC::Equal, rf, zero);
                        let bad = b.ins().uextend(I64, is_zero);
                        let pv = b.use_var(cx.poison);
                        let npv = b.ins().bor(pv, bad);
                        b.def_var(cx.poison, npv);
                        b.ins().fdiv(lf, rf)
                    }
                    _ => unreachable!("ineligible operator reached cx.mixed codegen"),
                };
                (v, NumKind::Float)
            }
        }
        // A USER function the `i64` specialization compiled. Tried first, so a user function
        // shadowing a builtin name dispatches to the user's function — the same precedence
        // `gen_value`'s `fn_ids` lookup establishes. `infer_mixed_kind` admitted this only
        // with every argument typed `Int`, which is exactly the contract the callee's i64
        // specialization was compiled under, so the result is an `i64` and types as `Int`:
        // the enclosing expression then promotes it at the first `Float` exactly where the
        // interpreter does.
        // ONE ARM, MERGED IN LOCKSTEP WITH `infer_mixed_kind`'s. These two must decide the
        // same way for the same call, and until now they could not: both were split into an
        // i64 arm and a mixed arm, guarded by set membership, and a function can be in BOTH
        // sets (`fn f(x: Float) -> Float = x * x` has a Float specialization AND an i64-closed
        // body). The analysis fix admits those calls; if codegen still took its i64 arm first
        // it would emit an `i64` call with `f64` arguments.
        //
        // The selection rule is character-for-character the analysis's: all-Int arguments to a
        // function with an i64 specialization take the direct call; anything else takes the
        // mixed one. This file records that admitting a shape the codegen cannot emit is how
        // this area got reverted three times, which is why both halves are in one commit.
        Expr::Call { name, args, .. }
            if cx.fn_ids.contains_key(name.as_str())
                || cx.mixed.sigs.contains_key(name.as_str()) =>
        {
            // Generate every argument once, and observe its kind — the values are identical
            // either way, only the marshalling differs.
            let argv: Vec<(ClValue, NumKind)> =
                args.iter().map(|a| gen_value_typed(b, a, cx)).collect();
            let all_int = argv.iter().all(|(_, k)| *k == NumKind::Int);

            // A USER function the `i64` specialization compiled. Tried first, so a user
            // function shadowing a builtin name dispatches to the user's — the same
            // precedence `gen_value`'s `fn_ids` lookup establishes. The result is an `i64`
            // and types as `Int`: the enclosing expression promotes it at the first `Float`
            // exactly where the interpreter does.
            if all_int && cx.fn_ids.contains_key(name.as_str()) {
                let fid = cx.fn_ids[name.as_str()];
                let fref = cx.module.declare_func_in_func(fid, b.func);
                let vals: Vec<ClValue> = argv.iter().map(|(v, _)| *v).collect();
                let call = b.ins().call(fref, &vals);
                return (b.inst_results(call)[0], NumKind::Int);
            }

            // A user function with a MIXED specialization. Its ABI is all-`i64` BIT slots
            // plus a trailing `*mut i8` poison pointer (see `MixedFn`), so `Float` arguments
            // are bitcast in and a `Float` result bitcast out. We hand it a stack CELL, then
            // fold that cell into this kernel's poison accumulator: a NaN compare or `/0`
            // inside the callee poisons the whole map, and the VM discards the output and
            // re-runs on bytecode for the exact interpreter error — the same contract as a
            // rounder leaving i64 range.
            let (params, ret) = &cx.mixed.sigs[name.as_str()];
            let fref = cx.module.declare_func_in_func(cx.mixed.ids[name.as_str()], b.func);
            // ORDERING CHANGE, AND IT IS SOUND. The cell used to be zeroed BEFORE the
            // arguments were generated; now the arguments come first, because their kinds
            // decide which call this is. There is one `poison_cell` per kernel, so a nested
            // mixed call inside an argument writes the same cell — but it also folds that
            // write into `cx.poison` immediately after its own call, before returning here,
            // so nothing is lost by re-zeroing afterwards.
            //
            // Zero the cell and read it back through its ADDRESS, not via `stack_store`/
            // `stack_load`: the callee writes through the pointer, which the slot-promotion
            // pass cannot see, so slot-relative accesses can be folded away as
            // "loads what was stored" (0). Explicit memory traffic keeps the write visible.
            let cell = cx.mixed.poison_cell;
            let cell_ptr = b.ins().stack_addr(I64, cell, 0);
            let zero8 = b.ins().iconst(I8, 0);
            b.ins().store(MemFlags::new(), zero8, cell_ptr, 0);
            let mut vals: Vec<ClValue> = Vec::with_capacity(args.len() + 1);
            for ((v, ak), &want) in argv.iter().zip(params) {
                debug_assert!(*ak == want, "mixed-call arg kind drifted from the callee sig");
                vals.push(match ak {
                    NumKind::Int => *v,
                    NumKind::Float => b.ins().bitcast(I64, MemFlags::new(), *v),
                });
            }
            vals.push(cell_ptr);
            let call = b.ins().call(fref, &vals);
            let raw = b.inst_results(call)[0];
            let flag = b.ins().load(I8, MemFlags::new(), cell_ptr, 0);
            let flag64 = b.ins().uextend(I64, flag);
            let pv = b.use_var(cx.poison);
            let npv = b.ins().bor(pv, flag64);
            b.def_var(cx.poison, npv);
            let v = match ret {
                NumKind::Int => raw,
                NumKind::Float => b.ins().bitcast(F64, MemFlags::new(), raw),
            };
            (v, *ret)
        }
        // The pure builtins (eligibility guaranteed the names + arities, and same-kind
        // `min`/`max`): `sqrt` promotes its arg to f64 (fsqrt → Float); `abs` is `iabs`
        // (Int) / `fabs` (Float); `min`/`max` compare-then-select-original, on `i64`
        // (via f64 compare, as the interpreter) or `f64`.
        Expr::Call { name, args, .. } => match name.as_str() {
            "sqrt" => {
                let (av, ak) = gen_value_typed(b, &args[0], cx);
                let af = if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                (b.ins().sqrt(af), NumKind::Float)
            }
            // `to_float` IS that promotion with nothing applied after it: an `i64` becomes
            // `f64` via `fcvt_from_sint` (the interpreter's `*i as f64`), and an `f64`
            // passes through unchanged.
            "to_float" => {
                let (av, ak) = gen_value_typed(b, &args[0], cx);
                let af = if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                (af, NumKind::Float)
            }
            // The RAISING rounders. Float → rounded → range-check → poison-or-convert:
            //
            //   * `floor`/`ceil`/`trunc` lower to the matching hardware op.
            //   * `round` is HALF-AWAY-FROM-ZERO (`round(2.5) = 3`, `round(-2.5) = -3`) —
            //     Rust's `f64::round`, NOT Cranelift's `nearest` (round-to-nearest-EVEN,
            //     which would be silently wrong on every tie). And the textbook
            //     `trunc(x + copysign(0.5, x))` is wrong too: for x = 0.49999999999999994
            //     (the largest f64 below 0.5) the add rounds UP to 1.0 in f64 arithmetic, so
            //     it returns 1 where `f64::round` returns 0. The exact lowering is
            //     `t = trunc(x); r = |x - t| >= 0.5 ? t + copysign(1, x) : t` — the
            //     subtraction is exact for |x| < 2^52, and above 2^52 every f64 is integral
            //     so the fractional part is exactly 0.
            //   * The range check is the interpreter's `round_to_i64` verbatim: accept iff
            //     rounded ∈ [-(2^63), 2^63), a half-open interval whose comparisons also
            //     reject NaN and ±inf for free. Out of range ORs 1 into the poison
            //     accumulator; the conversion below is `fcvt_to_sint_sat` so the (discarded)
            //     lane value is defined either way — a plain `fcvt_to_sint` would TRAP.
            //   * An `Int` argument is the identity (`floor(2) == 2`), matching the
            //     interpreter, and cannot raise.
            "floor" | "ceil" | "round" | "trunc" => {
                let (av, ak) = gen_value_typed(b, &args[0], cx);
                if ak == NumKind::Int {
                    return (av, NumKind::Int);
                }
                let rounded = match name.as_str() {
                    "floor" => b.ins().floor(av),
                    "ceil" => b.ins().ceil(av),
                    "trunc" => b.ins().trunc(av),
                    _ => {
                        let t = b.ins().trunc(av);
                        let d = b.ins().fsub(av, t);
                        let ad = b.ins().fabs(d);
                        let half = b.ins().f64const(0.5);
                        let ge = b.ins().fcmp(FloatCC::GreaterThanOrEqual, ad, half);
                        let one = b.ins().f64const(1.0);
                        let signed_one = b.ins().fcopysign(one, av);
                        let up = b.ins().fadd(t, signed_one);
                        b.ins().select(ge, up, t)
                    }
                };
                let min_c = b.ins().f64const(-9_223_372_036_854_775_808.0);
                let lim_c = b.ins().f64const(9_223_372_036_854_775_808.0);
                let ge_min = b.ins().fcmp(FloatCC::GreaterThanOrEqual, rounded, min_c);
                let lt_lim = b.ins().fcmp(FloatCC::LessThan, rounded, lim_c);
                let in_range = b.ins().band(ge_min, lt_lim);
                let bad_i8 = b.ins().bxor_imm(in_range, 1);
                let bad = b.ins().uextend(I64, bad_i8);
                let pv = b.use_var(cx.poison);
                let npv = b.ins().bor(pv, bad);
                b.def_var(cx.poison, npv);
                (b.ins().fcvt_to_sint_sat(I64, rounded), NumKind::Int)
            }
            "abs" => {
                let (av, ak) = gen_value_typed(b, &args[0], cx);
                match ak {
                    NumKind::Int => (b.ins().iabs(av), NumKind::Int),
                    NumKind::Float => (b.ins().fabs(av), NumKind::Float),
                }
            }
            // `to_int`: saturating float->int, the identity on an `Int`. `fcvt_to_sint_sat`
            // matches the interpreter exactly -- NaN to 0, +-inf to the i64 extremes.
            "to_int" => {
                let (av, ak) = gen_value_typed(b, &args[0], cx);
                match ak {
                    NumKind::Int => (av, NumKind::Int),
                    NumKind::Float => (b.ins().fcvt_to_sint_sat(I64, av), NumKind::Int),
                }
            }
            // `sign`: 1 / -1 / 0. Both comparisons are FALSE for NaN, so the selects fall
            // through to 0 -- which is what the interpreter returns for NaN (it compares
            // rather than using `signum`, which would propagate NaN).
            "sign" => {
                let (av, ak) = gen_value_typed(b, &args[0], cx);
                let one = b.ins().iconst(I64, 1);
                let neg = b.ins().iconst(I64, -1);
                let zero = b.ins().iconst(I64, 0);
                let (gt, lt) = match ak {
                    NumKind::Int => {
                        let z = b.ins().iconst(I64, 0);
                        (
                            b.ins().icmp(IntCC::SignedGreaterThan, av, z),
                            b.ins().icmp(IntCC::SignedLessThan, av, z),
                        )
                    }
                    NumKind::Float => {
                        let z = b.ins().f64const(0.0);
                        (
                            b.ins().fcmp(FloatCC::GreaterThan, av, z),
                            b.ins().fcmp(FloatCC::LessThan, av, z),
                        )
                    }
                };
                let lo = b.ins().select(lt, neg, zero);
                (b.ins().select(gt, one, lo), NumKind::Int)
            }
            "min" | "max" => {
                let (av, ak) = gen_value_typed(b, &args[0], cx);
                let (cv, _ck) = gen_value_typed(b, &args[1], cx);
                let le = name == "min";
                let cc = if le { FloatCC::LessThanOrEqual } else { FloatCC::GreaterThanOrEqual };
                match ak {
                    NumKind::Int => {
                        let af = b.ins().fcvt_from_sint(F64, av);
                        let cf = b.ins().fcvt_from_sint(F64, cv);
                        let keep = b.ins().fcmp(cc, af, cf);
                        (b.ins().select(keep, av, cv), NumKind::Int)
                    }
                    NumKind::Float => {
                        let keep = b.ins().fcmp(cc, av, cv);
                        (b.ins().select(keep, av, cv), NumKind::Float)
                    }
                }
            }
            _ => unreachable!("ineligible call reached cx.mixed codegen"),
        },
        _ => unreachable!("ineligible node reached cx.mixed codegen"),
    }
}

fn gen_cond<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
    kind: NumKind,
    // The f64 FILTER's NaN accumulator. The interpreter RAISES on a NaN operand in an
    // ordering comparison; a kernel cannot raise, so each Float `<`/`<=`/`>`/`>=` ORs an
    // `fcmp Unordered` (either operand NaN) into this variable, the filter returns -1
    // when it is set, and the dispatch falls back to the bytecode loop for the exact
    // error at the exact element. `==`/`!=` are IEEE in both worlds and need no poison.
    // `None` at the `if`-condition call sites, whose analyses admit no Float comparisons.
    poison: Option<Variable>,
) -> ClValue {
    match e {
        // `and`/`or` combine two i1 conditions. Handled before the comparison arm
        // because a nested `and`/`or` is itself an `Expr::Binary` and would otherwise
        // fall into the comparison `match op` and hit its `unreachable!`. Non-short-
        // circuit `band`/`bor` is exact for i64 comparisons; for the f64 filter it is
        // CONSERVATIVE — a NaN in a branch the interpreter might have short-circuited
        // past still sets the poison, which only costs a fall-back to the bytecode loop
        // (the semantics), never a wrong answer.
        Expr::Binary { op: BinOp::And, left, right, .. } => {
            let l = gen_cond(b, left, vars, fn_ids, module, kind, poison);
            let r = gen_cond(b, right, vars, fn_ids, module, kind, poison);
            b.ins().band(l, r)
        }
        Expr::Binary { op: BinOp::Or, left, right, .. } => {
            let l = gen_cond(b, left, vars, fn_ids, module, kind, poison);
            let r = gen_cond(b, right, vars, fn_ids, module, kind, poison);
            b.ins().bor(l, r)
        }
        Expr::Binary { op, left, right, .. } => {
            let l = gen_value(b, left, vars, fn_ids, module, kind);
            let r = gen_value(b, right, vars, fn_ids, module, kind);
            match kind {
                NumKind::Int => {
                    let cc = match op {
                        BinOp::Lt => IntCC::SignedLessThan,
                        BinOp::Gt => IntCC::SignedGreaterThan,
                        BinOp::Le => IntCC::SignedLessThanOrEqual,
                        BinOp::Ge => IntCC::SignedGreaterThanOrEqual,
                        BinOp::Eq => IntCC::Equal,
                        BinOp::Ne => IntCC::NotEqual,
                        _ => unreachable!("only comparisons reach cond codegen"),
                    };
                    b.ins().icmp(cc, l, r)
                }
                NumKind::Float => {
                    let cc = match op {
                        BinOp::Lt => FloatCC::LessThan,
                        BinOp::Gt => FloatCC::GreaterThan,
                        BinOp::Le => FloatCC::LessThanOrEqual,
                        BinOp::Ge => FloatCC::GreaterThanOrEqual,
                        // Ordered equal (NaN == x is false) and unordered-or-unequal
                        // (NaN != x is true) — exactly the interpreter's IEEE `==`/`!=`.
                        BinOp::Eq => FloatCC::Equal,
                        BinOp::Ne => FloatCC::NotEqual,
                        _ => unreachable!("only comparisons reach cond codegen"),
                    };
                    if let Some(pv) = poison
                        && matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge)
                    {
                        let uno = b.ins().fcmp(FloatCC::Unordered, l, r);
                        let uno64 = b.ins().uextend(I64, uno);
                        let old = b.use_var(pv);
                        let np = b.ins().bor(old, uno64);
                        b.def_var(pv, np);
                    }
                    b.ins().fcmp(cc, l, r)
                }
            }
        }
        _ => unreachable!("ineligible condition reached codegen"),
    }
}


/// Typed codegen for a **multi-binder f64** body: each binder maps to its `(Variable, kind)`
/// in `binders`. Integer subexpressions wrap as `i64`, promoting to `f64` at the first float
/// operand — the interpreter's `arith` rule, the N-binder twin of [`gen_reduce_f64_mixed`].
/// Returns the value and its kind; eligibility ([`infer_f64_typed`]) guarantees presence.
/// Everything [`gen_f64_typed`] threads through its recursion unchanged. Bundled for the
/// same reason [`TypedCtx`] is: the walker calls itself thirteen times, and repeating six
/// identical arguments at each one buries the single argument that actually differs.
struct F64Ctx<'a, 'c> {
    /// The kernel's binders — the accumulator, the i64 counter, any scalar captures,
    /// and (scoped, via the `Let` arm's save/restore) any `let` locals. `&mut` and
    /// two-lifetimed exactly like [`TypedCtx`]'s `vars`, for the same reason: a `let`
    /// local's name is borrowed from the BODY being lowered, not from the kernel's
    /// capture tables.
    binders: &'c mut HashMap<&'a str, (Variable, NumKind)>,
    /// `ArrayF64` capture bases, for `arr[counter]` in the dot-product kernel.
    arrays: &'c HashMap<&'a str, Variable>,
    /// The kernel's `i8` poison accumulator, when it carries one.
    poison: Option<Variable>,
    /// Monomorphized `i64` user functions this kernel may call directly.
    fn_ids: &'c HashMap<&'a str, FuncId>,
    module: &'c mut JITModule,
    /// Present only when the kernel carries poison — see the call arm.
    mixed: Option<&'c MixedCallCtx<'c>>,
}

fn gen_f64_typed<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    cx: &mut F64Ctx<'a, '_>,
) -> (ClValue, NumKind) {
    match e {
        Expr::Int(i) => (b.ins().iconst(I64, *i), NumKind::Int),
        // A `let` scope: inits evaluate left-to-right (each seeing the bindings before
        // it — the walker's sequential semantics), each binds a fresh Cranelift
        // variable of its inferred kind, and the body lowers against the extended
        // binder map. Save/restore makes the extension a true SCOPE: shadowing an
        // outer binder or capture restores it when the scope closes — the same
        // choreography the walker's env uses. The analyses guarantee `pa`/`pb` are
        // never rebound here.
        Expr::Let { bindings, body, .. } => {
            let mut saved: Vec<(&'a str, Option<(Variable, NumKind)>)> =
                Vec::with_capacity(bindings.len());
            for (n, vexpr) in bindings {
                let (val, kind) = gen_f64_typed(b, vexpr, cx);
                let var = b.declare_var(kind.cl_type());
                b.def_var(var, val);
                saved.push((n.as_str(), cx.binders.insert(n.as_str(), (var, kind))));
            }
            let out = gen_f64_typed(b, body, cx);
            for (n, old) in saved.into_iter().rev() {
                match old {
                    Some(prev) => {
                        cx.binders.insert(n, prev);
                    }
                    None => {
                        cx.binders.remove(n);
                    }
                }
            }
            out
        }
        // A USER function call — ONE arm for both specializations, the twin of
        // `gen_value_typed`'s. Merged rather than split for the reason recorded there: `fns`
        // means "i64-closed BODY", not "Int parameters", so a callee can be in both tables and
        // a name-guarded i64 arm would claim the call site and starve the cx.mixed one.
        Expr::Call { name, args, .. }
            if cx.fn_ids.contains_key(name.as_str())
                || cx.mixed.is_some_and(|m| m.sigs.contains_key(name.as_str())) =>
        {
            // Generated once, kinds observed — the values are identical either way, only the
            // marshalling differs.
            let argv: Vec<(ClValue, NumKind)> = args
                .iter()
                .map(|a| gen_f64_typed(b, a, cx))
                .collect();
            let all_int = argv.iter().all(|(_, k)| *k == NumKind::Int);

            // The `i64` specialization. Tried first, so a user function shadowing a builtin
            // name dispatches to the user's. Its arguments are `i64` and its result is an
            // `i64`, typed `Int`; the enclosing expression promotes at the first `Float`
            // exactly where the interpreter does.
            if all_int && cx.fn_ids.contains_key(name.as_str()) {
                let fid = cx.fn_ids[name.as_str()];
                let fref = cx.module.declare_func_in_func(fid, b.func);
                let vals: Vec<ClValue> = argv.iter().map(|(v, _)| *v).collect();
                let call = b.ins().call(fref, &vals);
                return (b.inst_results(call)[0], NumKind::Int);
            }

            // The MIXED specialization. Its ABI is all-`i64` BIT slots plus a trailing
            // `*mut i8` cx.poison pointer (see `MixedFn`), so `Float` arguments bitcast in and a
            // `Float` result bitcasts out. The callee's flag is folded into THIS kernel's
            // cx.poison accumulator, so a `/0` or NaN compare inside the callee bails the whole
            // reduce and the VM re-runs on bytecode for the exact interpreter error — the same
            // contract a rounder leaving i64 range already has.
            //
            // Reaching here with `cx.mixed == None` would be analysis/codegen drift. It cannot
            // happen, and the proof is that all three analyses feeding this codegen exclude it:
            //
            //   * capture-free scalar (`infer_reduce_f64_kind`) — a non-all-`Int` user call is
            //     admitted ONLY via `msigs`, and `body_raises` counts a cx.mixed call, so
            //     `raises` and therefore `needs_poison` and therefore `cx.mixed` are all `Some`.
            //   * captured / dot-product (`infer_f64_indexed`) — admits a user call only with
            //     every argument typed `Int`, so `all_int` is true and the arm returned above.
            //   * tuple (`infer_f64_typed`) — its call arm is guarded `!user_fns.contains`, so
            //     it admits no user call at all.
            //
            // `unreachable!` rather than a returned error because this function is total by
            // construction and has no error channel — the same contract, and the same macro,
            // as the ten sibling arms in it ("ineligible … reached codegen").
            let Some(mx) = cx.mixed else {
                unreachable!("cx.mixed call reached f64 reduce codegen without a cx.poison cell")
            };
            let (params, ret) = &mx.sigs[name.as_str()];
            let fref = cx.module.declare_func_in_func(mx.ids[name.as_str()], b.func);
            // Zero the cell and read it back through its ADDRESS, not `stack_store`/
            // `stack_load`: the callee writes through the pointer, which slot promotion cannot
            // see, so slot-relative accesses could be folded away as "loads what was stored".
            let cell_ptr = b.ins().stack_addr(I64, mx.poison_cell, 0);
            let zero8 = b.ins().iconst(I8, 0);
            b.ins().store(MemFlags::new(), zero8, cell_ptr, 0);
            let mut vals: Vec<ClValue> = Vec::with_capacity(args.len() + 1);
            for ((v, ak), &want) in argv.iter().zip(params) {
                debug_assert!(*ak == want, "cx.mixed-call arg kind drifted from the callee sig");
                vals.push(match ak {
                    NumKind::Int => *v,
                    NumKind::Float => b.ins().bitcast(I64, MemFlags::new(), *v),
                });
            }
            vals.push(cell_ptr);
            let call = b.ins().call(fref, &vals);
            let raw = b.inst_results(call)[0];
            let flag = b.ins().load(I8, MemFlags::new(), cell_ptr, 0);
            if let Some(p) = cx.poison {
                let pv = b.use_var(p);
                let npv = b.ins().bor(pv, flag);
                b.def_var(p, npv);
            }
            let v = match ret {
                NumKind::Int => raw,
                NumKind::Float => b.ins().bitcast(F64, MemFlags::new(), raw),
            };
            (v, *ret)
        }
        Expr::Float(f) => (b.ins().f64const(*f), NumKind::Float),
        Expr::Ident { name, .. } => {
            let (var, kind) = cx.binders[name.as_str()];
            (b.use_var(var), kind)
        }
        // `arr[counter]` reading a captured `f64` array (float dot-product): `recv` is bound in
        // `cx.arrays` to the packed base pointer, `index` is the i64 counter. The VM pre-checked
        // the whole counter range is in bounds, so this raw `f64` load is safe. Only the
        // scalar-with-`ArrayF64`-caps path populates `cx.arrays` (empty for tuple/record reduces).
        Expr::Index { recv, index, .. } => {
            let name = match &**recv {
                Expr::Ident { name, .. } => name.as_str(),
                _ => unreachable!("ineligible f64 index receiver reached codegen"),
            };
            let base = b.use_var(cx.arrays[name]);
            let (idx, _) = gen_f64_typed(b, index, cx);
            let off = b.ins().imul_imm(idx, 8);
            let addr = b.ins().iadd(base, off);
            (b.ins().load(F64, MemFlags::trusted(), addr, 0), NumKind::Float)
        }
        Expr::Binary { op, left, right, .. } => {
            let (lv, lk) = gen_f64_typed(b, left, cx);
            let (rv, rk) = gen_f64_typed(b, right, cx);
            // `/` is always float division in Helix, so it forces the f64 path even for `Int/Int`
            // (matching the interpreter); `+ - *` stay `i64` when both operands are `Int`.
            if lk == NumKind::Int && rk == NumKind::Int && !matches!(op, BinOp::Div) {
                let v = match op {
                    BinOp::Add => b.ins().iadd(lv, rv),
                    BinOp::Sub => b.ins().isub(lv, rv),
                    BinOp::Mul => b.ins().imul(lv, rv),
                    _ => unreachable!("ineligible operator reached f64 tuple codegen"),
                };
                (v, NumKind::Int)
            } else {
                let lf = if lk == NumKind::Int { b.ins().fcvt_from_sint(F64, lv) } else { lv };
                let rf = if rk == NumKind::Int { b.ins().fcvt_from_sint(F64, rv) } else { rv };
                let v = match op {
                    BinOp::Add => b.ins().fadd(lf, rf),
                    BinOp::Sub => b.ins().fsub(lf, rf),
                    BinOp::Mul => b.ins().fmul(lf, rf),
                    // Native `fdiv` yields inf/nan on a zero divisor where the interpreter RAISES.
                    // Record it: OR `divisor == 0.0` into the cx.poison flag (accumulated across all
                    // iterations), which the VM checks after the loop and, if set, falls back to
                    // the exact-erroring bytecode loop. `rf == 0.0` is bit-identical to the
                    // interpreter's `b == 0.0` divisor check (and catches −0.0 too), so the
                    // fallback fires on exactly the `/0` the interpreter reports — regardless of
                    // whether a later op or iteration would "rescue" the resulting inf/nan.
                    BinOp::Div => {
                        if let Some(p) = cx.poison {
                            let zero = b.ins().f64const(0.0);
                            let is_zero = b.ins().fcmp(FloatCC::Equal, rf, zero);
                            let cur = b.use_var(p);
                            let next = b.ins().bor(cur, is_zero);
                            b.def_var(p, next);
                        }
                        b.ins().fdiv(lf, rf)
                    }
                    _ => unreachable!("ineligible operator reached f64 tuple codegen"),
                };
                (v, NumKind::Float)
            }
        }
        Expr::Call { name, args, .. } => match name.as_str() {
            "sqrt" => {
                let (av, ak) = gen_f64_typed(b, &args[0], cx);
                let af = if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                (b.ins().sqrt(af), NumKind::Float)
            }
            // `to_float` IS that promotion with nothing applied after it: an `i64` becomes
            // `f64` via `fcvt_from_sint` (the interpreter's `*i as f64`), and an `f64`
            // passes through unchanged.
            "to_float" => {
                let (av, ak) = gen_f64_typed(b, &args[0], cx);
                let af = if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                (af, NumKind::Float)
            }
            "abs" => {
                let (av, ak) = gen_f64_typed(b, &args[0], cx);
                match ak {
                    NumKind::Int => (b.ins().iabs(av), NumKind::Int),
                    NumKind::Float => (b.ins().fabs(av), NumKind::Float),
                }
            }
            // `to_int`: saturating float->int, the identity on an `Int`. `fcvt_to_sint_sat`
            // matches the interpreter exactly -- NaN to 0, +-inf to the i64 extremes.
            "to_int" => {
                let (av, ak) = gen_f64_typed(b, &args[0], cx);
                match ak {
                    NumKind::Int => (av, NumKind::Int),
                    NumKind::Float => (b.ins().fcvt_to_sint_sat(I64, av), NumKind::Int),
                }
            }
            // `sign`: 1 / -1 / 0. Both comparisons are FALSE for NaN, so the selects fall
            // through to 0 -- which is what the interpreter returns for NaN (it compares
            // rather than using `signum`, which would propagate NaN).
            "sign" => {
                let (av, ak) = gen_f64_typed(b, &args[0], cx);
                let one = b.ins().iconst(I64, 1);
                let neg = b.ins().iconst(I64, -1);
                let zero = b.ins().iconst(I64, 0);
                let (gt, lt) = match ak {
                    NumKind::Int => {
                        let z = b.ins().iconst(I64, 0);
                        (
                            b.ins().icmp(IntCC::SignedGreaterThan, av, z),
                            b.ins().icmp(IntCC::SignedLessThan, av, z),
                        )
                    }
                    NumKind::Float => {
                        let z = b.ins().f64const(0.0);
                        (
                            b.ins().fcmp(FloatCC::GreaterThan, av, z),
                            b.ins().fcmp(FloatCC::LessThan, av, z),
                        )
                    }
                };
                let lo = b.ins().select(lt, neg, zero);
                (b.ins().select(gt, one, lo), NumKind::Int)
            }
            "min" | "max" => {
                let (av, ak) = gen_f64_typed(b, &args[0], cx);
                let (cv, _ck) = gen_f64_typed(b, &args[1], cx);
                let le = name == "min";
                let cc = if le { FloatCC::LessThanOrEqual } else { FloatCC::GreaterThanOrEqual };
                match ak {
                    NumKind::Int => {
                        let af = b.ins().fcvt_from_sint(F64, av);
                        let cf = b.ins().fcvt_from_sint(F64, cv);
                        let keep = b.ins().fcmp(cc, af, cf);
                        (b.ins().select(keep, av, cv), NumKind::Int)
                    }
                    NumKind::Float => {
                        let keep = b.ins().fcmp(cc, av, cv);
                        (b.ins().select(keep, av, cv), NumKind::Float)
                    }
                }
            }
            _ => unreachable!("ineligible call reached f64 tuple codegen"),
        },
        _ => unreachable!("ineligible node reached f64 tuple codegen"),
    }
}

/// Emit a recognized pure float builtin over `f64` operands (only reached for `Float`-kind
/// codegen). Mirrors the interpreter exactly.
fn gen_builtin_f64<'a>(
    b: &mut FunctionBuilder,
    name: &str,
    args: &'a [Expr],
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
) -> ClValue {
        match name {
            "sqrt" => {
                let x = gen_value(b, &args[0], vars, fn_ids, module, NumKind::Float);
                b.ins().sqrt(x)
            }
        // Every value in this kernel is already `f64`, so the conversion is the identity.
        "to_float" => gen_value(b, &args[0], vars, fn_ids, module, NumKind::Float),
        "abs" => {
            let x = gen_value(b, &args[0], vars, fn_ids, module, NumKind::Float);
            b.ins().fabs(x)
        }
        "min" | "max" => {
            let a = gen_value(b, &args[0], vars, fn_ids, module, NumKind::Float);
            let c = gen_value(b, &args[1], vars, fn_ids, module, NumKind::Float);
            // `min` keeps the first iff `a <= b`; `max` iff `a >= b`. An `fcmp` is false
            // when either operand is NaN, so `select` picks the second — matching the
            // interpreter's `a <= b ? a : b` over `f64` (NaN-propagating identically).
            let cc = if name == "min" { FloatCC::LessThanOrEqual } else { FloatCC::GreaterThanOrEqual };
            let keep_a = b.ins().fcmp(cc, a, c);
            b.ins().select(keep_a, a, c)
        }
        _ => unreachable!("unrecognized JIT float builtin `{name}`"),
    }
}

/// Emit a recognized pure scalar builtin over `i64` operands (only reached for `Int`-kind
/// codegen; the `f64` subset excludes calls). Mirrors the interpreter exactly.
fn gen_builtin_i64<'a>(
    b: &mut FunctionBuilder,
    name: &str,
    args: &'a [Expr],
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
) -> ClValue {
    match name {
        "abs" => {
            let x = gen_value(b, &args[0], vars, fn_ids, module, NumKind::Int);
            b.ins().iabs(x) // wraps i64::MIN to itself, matching `wrapping_abs`
        }
        // On an `Int` these are trivial: `to_int` is the identity and `sign` is a compare pair.
        "to_int" => gen_value(b, &args[0], vars, fn_ids, module, NumKind::Int),
        "sign" => {
            let x = gen_value(b, &args[0], vars, fn_ids, module, NumKind::Int);
            let z = b.ins().iconst(I64, 0);
            let one = b.ins().iconst(I64, 1);
            let neg = b.ins().iconst(I64, -1);
            let zero = b.ins().iconst(I64, 0);
            let gt = b.ins().icmp(IntCC::SignedGreaterThan, x, z);
            let lt = b.ins().icmp(IntCC::SignedLessThan, x, z);
            let lo = b.ins().select(lt, neg, zero);
            b.ins().select(gt, one, lo)
        }
        "min" | "max" => {
            let a = gen_value(b, &args[0], vars, fn_ids, module, NumKind::Int);
            let c = gen_value(b, &args[1], vars, fn_ids, module, NumKind::Int);
            // The interpreter compares via `as_f64()` and returns the ORIGINAL operand:
            // `min` keeps the first iff `a_f64 <= b_f64`; `max` iff `a_f64 >= b_f64`.
            let af = b.ins().fcvt_from_sint(F64, a);
            let cf = b.ins().fcvt_from_sint(F64, c);
            let cc = if name == "min" { FloatCC::LessThanOrEqual } else { FloatCC::GreaterThanOrEqual };
            let keep_a = b.ins().fcmp(cc, af, cf);
            b.ins().select(keep_a, a, c)
        }
        _ => unreachable!("unrecognized JIT scalar builtin `{name}`"),
    }
}

impl NumKind {
    /// The Cranelift type for this kind — codegen-side, so the ungated `NumKind`
    /// never names a cranelift type.
    fn cl_type(self) -> Type {
        match self {
            NumKind::Int => I64,
            NumKind::Float => F64,
        }
    }
}

//! Cranelift JIT — Track B of the performance roadmap.
//!
//! Compiles the numeric-recursion core of a program to **native machine code**,
//! specialized per concrete type (the Julia recipe: monomorphization). The VM
//! dispatches to a native version when the argument types match, falling back to
//! bytecode otherwise. Once execution enters native code, all internal recursion
//! stays native, which is where the speed comes from.
//!
//! **Currently only the `i64` specialization is emitted** (see [`build`]): with
//! all-`Int` args every op yields `Int`, exactly matching the interpreter. The
//! `f64` codegen below is complete but **dormant** — a float-arg function can
//! still return an `Int` (a literal, or an Int-only subexpression), so emitting it
//! would diverge from the interpreter on result type; float functions run on the
//! VM instead. So the IEEE-754 NaN-comparison edge the f64 path would introduce
//! (a NaN compare is `false`, where the interpreter raises) is **latent, not
//! active** — there is no live JIT/interpreter divergence today.
//!
//! A function is eligible (for a given numeric kind) when its body uses only
//! constructs that lower to that kind: literals, params/`let` locals, the kind's
//! arithmetic (`+ - *` for both; `/` additionally for `f64`), an `if` with a
//! comparison condition, and calls to other functions eligible in the same kind.
//! Eligibility is a fixpoint (a fn calling an ineligible fn is ineligible) and
//! caps arity at 4.
//!
//! Semantics match the interpreter's *release* behaviour: integer arithmetic
//! wraps on overflow.
//!
//! SAFETY: calling generated code is inherently `unsafe`. Every `unsafe` block in the
//! JIT lives in the [`ffi`] submodule (`jit/ffi.rs`) — the FFI trampolines that
//! transmute a finalized code pointer to its `extern "C"` type and call it — each
//! guarded by the VM's type/arity check so the native ABI contract always holds. The
//! rest of this file is safe analysis + Cranelift codegen. The JIT deals only in
//! scalar `i64`/`f64` — no heap, no `Rc` — so it adds no leak surface.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F64, I8, I64};
use cranelift_codegen::ir::{AbiParam, Block, InstBuilder, MemFlags, Type, Value as ClValue};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::ast::{BinOp, Expr, Stmt, TypeAnn};
use crate::bytecode::{Capture, CaptureKind, IndexBound};

// The JIT's only `unsafe`: the FFI trampolines that call finalized native code. Kept in
// their own file so that boundary is a single auditable unit; re-exported so callers
// still use `crate::jit::call_i64`, etc.
mod ffi;
pub use ffi::*;

const MAX_ARITY: usize = 6;

/// The two scalar specializations.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum NumKind {
    Int,
    Float,
}

impl NumKind {
    fn cl_type(self) -> Type {
        match self {
            NumKind::Int => I64,
            NumKind::Float => F64,
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            NumKind::Int => "i",
            NumKind::Float => "f",
        }
    }
}

/// The **mixed** (per-parameter `Int`/`Float`, from explicit annotations) tail-loop
/// specialization of a user function. Everything crosses the FFI boundary as `i64` BIT
/// PATTERNS so the existing [`call_i64`] trampolines work for any parameter mix: a
/// `Float` parameter is passed as `f64::to_bits` and bitcast back in the prologue; a
/// `Float` result is bitcast to bits in the epilogue and `f64::from_bits` at the VM.
/// Pure bit moves — no numeric conversion, bit-exact.
///
/// The native signature carries ONE extra trailing slot: a `*mut i8` POISON pointer.
/// The interpreter RAISES on a NaN comparison ("cannot compare these values"), where
/// native `fcmp` would silently order it — so every float comparison in the loop first
/// checks `fcmp Unordered` and, on NaN, bails immediately to a poison block (store 1,
/// return). The VM then DISCARDS the native result and falls through to the bytecode
/// path, which re-runs the call and raises the exact interpreter error. The bail must
/// be immediate (not accumulate-and-store like the fdiv poison in bounded reduce
/// loops): a tail loop can be infinite, and a NaN inside one must error like the
/// interpreter, not spin natively.
#[derive(Clone, Copy)]
pub struct MixedFn {
    pub ptr: *const u8,
    /// Bit `j` set ⇔ parameter `j` is `Float` (arity ≤ [`MAX_ARITY`] ≤ 16 bits). The VM
    /// dispatches this specialization only when every argument's runtime type matches.
    pub float_mask: u16,
    /// Whether the result is `Float` (returned as raw bits).
    pub ret_float: bool,
}

/// The native entry points for one user function (whichever specializations
/// compiled), plus its arity. `Copy` so the VM can pull it out cheaply.
#[derive(Clone, Copy)]
pub struct NativeFn {
    pub i64_ptr: Option<*const u8>,
    pub f64_ptr: Option<*const u8>,
    /// The annotated mixed-parameter tail-loop specialization, if one compiled.
    pub mixed: Option<MixedFn>,
    pub arity: usize,
}

/// Owns the JIT module (and thus the executable code) plus the name → entry-point
/// table. Must outlive every native call.
pub struct Jit {
    _module: JITModule,
    by_name: HashMap<String, NativeFn>,
    /// Native `extern "C" fn(i64,i64,i64)->i64` reduce loops, indexed by the
    /// `loop_idx` of [`crate::bytecode::Op::TryJitReduce`]. `None` for a site that
    /// the JIT declined (kept as a slot so indices stay aligned with the compiler).
    reduce_ptrs: Vec<Option<*const u8>>,
    /// Native `extern "C" fn(*const i64 src, *mut i64 dst, i64 len, *const i64 caps)` map
    /// kernels (Int source), indexed by [`crate::bytecode::Op::TryJitMap`]'s `kernel_idx`.
    map_ptrs: Vec<Option<*const u8>>,
    /// The `f64` specialization of each map kernel (Float source): `extern "C"
    /// fn(*const f64, *mut f64, i64, *const f64)`. Same index as `map_ptrs`; the VM picks
    /// by the receiver array's element type.
    map_ptrs_f64: Vec<Option<*const u8>>,
    /// The **mixed** specialization (Int source, float body): `extern "C"
    /// fn(*const i64, *mut f64, i64, *const i64)` — reads `i64`, writes `f64`, no captures.
    /// Same index as `map_ptrs`; taken for an `Int` array when no plain `i64` kernel exists.
    map_ptrs_mixed: Vec<Option<*const u8>>,
    /// Native `extern "C" fn(*const i64 src, *mut i64 dst, i64 len) -> i64` (kept count)
    /// filter kernels, indexed by [`crate::bytecode::Op::TryJitFilter`]'s `kernel_idx`.
    filter_ptrs: Vec<Option<*const u8>>,
    /// Native fused-pipeline kernels (one of three signatures by shape — see
    /// [`define_fused_kernel`]), indexed by [`crate::bytecode::Op::TryJitFused`]'s
    /// `kernel_idx`.
    fused_ptrs: Vec<Option<*const u8>>,
}

impl Jit {
    pub fn lookup(&self, name: &str) -> Option<NativeFn> {
        self.by_name.get(name).copied()
    }
    /// The native reduce loop for site `idx`, if one compiled.
    pub fn reduce_loop(&self, idx: usize) -> Option<*const u8> {
        self.reduce_ptrs.get(idx).copied().flatten()
    }
    /// The native `i64` map kernel for site `idx`, if one compiled.
    pub fn map_kernel(&self, idx: usize) -> Option<*const u8> {
        self.map_ptrs.get(idx).copied().flatten()
    }
    /// The native `f64` map kernel for site `idx`, if one compiled.
    pub fn map_kernel_f64(&self, idx: usize) -> Option<*const u8> {
        self.map_ptrs_f64.get(idx).copied().flatten()
    }
    /// The native **mixed** map kernel (Int source → Float result) for site `idx`.
    pub fn map_kernel_mixed(&self, idx: usize) -> Option<*const u8> {
        self.map_ptrs_mixed.get(idx).copied().flatten()
    }
    /// The native filter kernel for site `idx`, if one compiled.
    pub fn filter_kernel(&self, idx: usize) -> Option<*const u8> {
        self.filter_ptrs.get(idx).copied().flatten()
    }
    /// The native fused-pipeline kernel for site `idx`, if one compiled.
    pub fn fused_kernel(&self, idx: usize) -> Option<*const u8> {
        self.fused_ptrs.get(idx).copied().flatten()
    }
}

struct FnDef<'a> {
    name: &'a str,
    params: &'a [(String, Option<TypeAnn>)],
    body: &'a Expr,
}

/// Compile every eligible numeric function — and every JIT-eligible `reduce` loop
/// the compiler flagged — to native code, or `None` if nothing is eligible (so the
/// caller skips JIT setup).
pub fn build(
    program: &[Stmt],
    reduce_loops: &[crate::bytecode::ReduceLoop],
    map_kernels: &[crate::bytecode::ArrayKernel],
    filter_kernels: &[crate::bytecode::ArrayKernel],
    fused_kernels: &[crate::bytecode::FusedKernel],
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

    // MIXED (per-parameter Int/Float, from explicit annotations) tail-loop
    // specializations — the mandelbrot-class scalar loops whose state is f64 but whose
    // counter/result is i64, dispatched ONLY from the VM's `CallFn` (never from kernels
    // or other natives, so they live outside `fn_ids` / `int_eligible` and cannot
    // interfere with any existing path). The external signature is uniformly all-`i64`
    // (bits ABI — see [`MixedFn`]); the prologue bitcasts Float params, the epilogue
    // bitcasts a Float result.
    let mut compiled_mixed: Vec<(String, FuncId, u16, bool, usize)> = Vec::new();
    {
        let mut ctx = module.make_context();
        let mut bctx = FunctionBuilderContext::new();
        for f in &funcs {
            let Some((mask, param_kinds, ret_kind)) = mixed_tail_sig(f, &tail_loop, &int_eligible)
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
            };
            gen_tail_mixed(&mut builder, f.body, &mut vars, &mut env, &tl);
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
            if !reduce_bodies_eligible(rl, &int_eligible, &user_fns) {
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
            match define_reduce_loop(&mut module, &mut ctx, &mut bctx, id, rl, &fn_ids) {
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
        &mut module, map_kernels, "map", false, &fn_ids, &int_eligible, &user_fns, NumKind::Int, false,
    );
    let map_f64_ids = define_array_kernels(
        &mut module, map_kernels, "mapf", false, &fn_ids, &int_eligible, &user_fns, NumKind::Float, false,
    );
    // The mixed `Int`-source → `Float` specialization (`range.map(j => j*0.001)`): reads
    // `i64`, writes `f64`. `elem_kind` is ignored when `mixed` (the body is typed per node).
    let map_mixed_ids = define_array_kernels(
        &mut module, map_kernels, "mapm", false, &fn_ids, &int_eligible, &user_fns, NumKind::Int, true,
    );
    let filter_ids = define_array_kernels(
        &mut module, filter_kernels, "filter", true, &fn_ids, &int_eligible, &user_fns, NumKind::Int, false,
    );
    let fused_ids = define_fused_kernels(&mut module, fused_kernels, &fn_ids, &int_eligible, &user_fns);

    if compiled.is_empty()
        && compiled_mixed.is_empty()
        && reduce_ids.iter().all(|r| r.is_none())
        && map_ids.iter().all(|r| r.is_none())
        && map_f64_ids.iter().all(|r| r.is_none())
        && map_mixed_ids.iter().all(|r| r.is_none())
        && filter_ids.iter().all(|r| r.is_none())
        && fused_ids.iter().all(|r| r.is_none())
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

    let finalize = |ids: Vec<Option<FuncId>>, module: &JITModule| -> Vec<Option<*const u8>> {
        ids.into_iter().map(|id| id.map(|id| module.get_finalized_function(id))).collect()
    };
    let reduce_ptrs = finalize(reduce_ids, &module);
    let map_ptrs = finalize(map_ids, &module);
    let map_ptrs_f64 = finalize(map_f64_ids, &module);
    let map_ptrs_mixed = finalize(map_mixed_ids, &module);
    let filter_ptrs = finalize(filter_ids, &module);
    let fused_ptrs = finalize(fused_ids, &module);

    Some(Jit {
        _module: module,
        by_name,
        reduce_ptrs,
        map_ptrs,
        map_ptrs_f64,
        map_ptrs_mixed,
        filter_ptrs,
        fused_ptrs,
    })
}

/// All stages and the reduce sink of a fused pipeline must be JIT-eligible.
fn fusion_eligible(k: &crate::bytecode::FusedKernel, fns: &HashSet<&str>, user_fns: &HashSet<&str>) -> bool {
    use crate::bytecode::{FusionSink, FusionStage};
    k.stages.iter().all(|s| match s {
        FusionStage::Map { binder, body } => map_kernel_eligible(body, binder, fns),
        FusionStage::Filter { binder, body } => filter_kernel_eligible(body, binder, fns),
    }) && match &k.sink {
        FusionSink::Collect | FusionSink::Count => true,
        // A float reduce is checked against the f64 subset (using `user_fns` to exclude a
        // user-shadowed `sqrt`/`min`, exactly like the f64 map path); the i64 path uses
        // `bodies_eligible`. Scalar (1 body) over `{pa, pb}`; tuple (N>1) over the f64 slots
        // `{$acc0…, pb}` (the array element `pb` is `f64`).
        FusionSink::Reduce { pa, pb, bodies, float: true } if bodies.len() == 1 => {
            float_reduce_body_eligible(&bodies[0], pa, pb, user_fns)
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
    mixed: bool,
) -> Vec<Option<FuncId>> {
    let mut ids: Vec<Option<FuncId>> = Vec::with_capacity(kernels.len());
    let mut ctx = module.make_context();
    let mut bctx = FunctionBuilderContext::new();
    for (i, k) in kernels.iter().enumerate() {
        // Eligibility per kind: filter (Int comparison), mixed map (Int source, float body
        // — `mixed_map_eligible`), Float map (the safe `+ - *` subset over a Floats source
        // — `map_kernel_captures_f64`), or Int map (capture-aware, body re-checked so a
        // captured-var body compiles).
        let ok = if is_filter {
            filter_kernel_eligible(&k.body, &k.binder, eligible)
        } else if mixed {
            mixed_map_eligible(&k.body, &k.binder, user_fns)
        } else if matches!(elem_kind, NumKind::Float) {
            map_kernel_captures_f64(&k.body, &k.binder, user_fns).is_some()
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
        if is_filter {
            sig.returns.push(AbiParam::new(I64)); // kept count
        } else {
            sig.params.push(AbiParam::new(I64)); // map: caps ptr (loop-invariant captures)
        }
        let id = match module.declare_function(&format!("{tag}${i}"), Linkage::Local, &sig) {
            Ok(id) => id,
            Err(_) => {
                ids.push(None);
                continue;
            }
        };
        let done = define_array_kernel(
            module, &mut ctx, &mut bctx, id, k, is_filter, fn_ids, elem_kind, mixed,
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

// ---------- eligibility ----------

/// True if `body` is a pure `i64` expression over `{pa, pb}` the JIT can compile
/// into a native reduce loop: integer literals, the two binders, `+ - *`,
/// comparisons inside an `if`, and `let` — but **no** floats, division, function
/// calls, or other free identifiers. The bytecode compiler calls this to decide
/// whether to emit a `TryJitReduce` guard, so it is the single source of truth for
/// reduce-loop eligibility (and is platform-independent — the native code is only
/// emitted by `build`, which is gated to x86-64 Linux).
/// The names of user functions the JIT can compile as pure `i64` natives — so a kernel
/// or reduce body may *call* them. Computed identically at bytecode-compile time (to
/// decide whether to emit a guard) and at JIT-build time (to compile the kernel), so the
/// two always agree. Platform-independent (only codegen is x86-64-gated).
pub fn int_eligible_fns(program: &[Stmt]) -> std::collections::HashSet<String> {
    let funcs: Vec<FnDef> = program
        .iter()
        .filter_map(|s| match s {
            Stmt::Func { name, params, body, .. } => Some(FnDef { name, params, body }),
            _ => None,
        })
        .collect();
    eligible_set(&funcs, NumKind::Int).into_iter().map(str::to_string).collect()
}

/// True if `body` is a pure `i64` value expression over `{pa, pb}` and calls only the
/// JIT-eligible functions in `fns` — what `define_reduce_loop`/`define_fused_kernel` can
/// lower. `fns` is empty for a self-contained body.
pub fn reduce_loop_eligible(body: &Expr, pa: &str, pb: &str, fns: &HashSet<&str>) -> bool {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(pa);
    locals.insert(pb);
    value_eligible(body, fns, &locals, NumKind::Int)
}

/// Like [`reduce_loop_eligible`], but a **scalar** body referencing free (captured)
/// variables is still eligible — each free variable is recorded (in first-appearance
/// order) as a [`Capture`] and passed to the kernel as a loop-invariant `caps[i]`. Two
/// capture shapes: a bare free `i64` variable (the nested-fold case: an inner
/// `range(..).reduce(..)` reading the outer `map` variable → [`CaptureKind::Scalar`]), and
/// a free array indexed by the loop counter `pb` (`arr[pb]`, the dot-product case →
/// [`CaptureKind::ArrayI64`]). Returns the ordered captures (possibly empty), or `None` if
/// the body is ineligible, captures more than [`MAX_CAPTURES`], or uses a name both bare
/// and indexed (a contradictory kind). Same i64-closed rules as `value_eligible(Int)`.
pub fn reduce_loop_captures(
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
) -> Option<(Vec<Capture>, Vec<IndexBound>)> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(pa);
    locals.insert(pb);
    let mut caps: Vec<Capture> = Vec::new();
    let mut bounds: Vec<IndexBound> = Vec::new();
    if value_eligible_cap_indexed(body, fns, &locals, pb, &mut caps, &mut bounds) && caps.len() <= MAX_CAPTURES {
        Some((caps, bounds))
    } else {
        None
    }
}

/// Record capture `name` with `kind` in first-appearance order, deduping, returning its slot
/// position — or `None` if `name` was already recorded with a *different* kind (a body that
/// reads `a` both bare and as `a[…]` is contradictory: scalar or array? → fall back rather than
/// guess). Positions are what [`IndexBound`] obligations reference, so codegen and the VM stay
/// driven by one unambiguous ordered list.
fn record_cap_pos(caps: &mut Vec<Capture>, name: &str, kind: CaptureKind) -> Option<usize> {
    if let Some(pos) = caps.iter().position(|c| c.name == name) {
        return if caps[pos].kind == kind { Some(pos) } else { None };
    }
    caps.push(Capture { name: name.to_string(), kind });
    Some(caps.len() - 1)
}

/// `record_cap_pos`, discarding the position — for the bare-scalar case that needs no bound.
fn record_cap(caps: &mut Vec<Capture>, name: &str, kind: CaptureKind) -> bool {
    record_cap_pos(caps, name, kind).is_some()
}

/// Append a bounds obligation, deduping (a repeated `arr[j]` needs only one range check).
fn push_bound(bounds: &mut Vec<IndexBound>, b: IndexBound) {
    if !bounds.contains(&b) {
        bounds.push(b);
    }
}

/// Reduce-only twin of [`value_eligible_cap`] that additionally accepts `arr[pb]` — a free
/// array indexed by exactly the loop counter — recording it as a [`CaptureKind::ArrayI64`].
/// A bare free ident is a [`CaptureKind::Scalar`] cap (as before). `pb` is threaded so the
/// index shape can be checked. NOT shared with the map kernel (whose `value_eligible_cap`
/// still rejects `Index`), so array-indexing stays scoped to the reduce path until the map
/// variant lands. i64-closed subset — identical operator rules to `value_eligible_cap`.
fn value_eligible_cap_indexed(
    e: &Expr,
    eligible: &HashSet<&str>,
    locals: &HashSet<&str>,
    pb: &str,
    caps: &mut Vec<Capture>,
    bounds: &mut Vec<IndexBound>,
) -> bool {
    match e {
        Expr::Int(_) => true,
        Expr::Float(_) => false,
        Expr::Ident { name, .. } => {
            if locals.contains(name.as_str()) {
                true
            } else {
                record_cap(caps, name, CaptureKind::Scalar)
            }
        }
        Expr::Index { recv, index, .. } => match (&**recv, &**index) {
            // `arr[pb]`: a free array read by exactly the loop counter → a Counter bound (the
            // VM range-checks `[start,end) ⊆ [0,len)`; the counter's values are exactly that).
            (Expr::Ident { name: arr, .. }, Expr::Ident { name: idx, .. })
                if !locals.contains(arr.as_str()) && idx == pb =>
            {
                match record_cap_pos(caps, arr, CaptureKind::ArrayI64) {
                    Some(ap) => {
                        push_bound(bounds, IndexBound::Counter { array: ap as u32 });
                        true
                    }
                    None => false,
                }
            }
            // `arr[i]`: a free array indexed by a free SCALAR capture (not the counter, not a
            // local) — the all-pairs shape (`codes[i]` with the outer binder `i`). Records `arr`
            // as an array cap and `i` as a scalar cap, and a Scalar (point) bound the VM checks
            // as `0 <= i < len(arr)`. `idx != arr` rules out `a[a]`.
            (Expr::Ident { name: arr, .. }, Expr::Ident { name: idx, .. })
                if !locals.contains(arr.as_str())
                    && !locals.contains(idx.as_str())
                    && idx != arr =>
            {
                let ap = match record_cap_pos(caps, arr, CaptureKind::ArrayI64) {
                    Some(p) => p,
                    None => return false,
                };
                let sp = match record_cap_pos(caps, idx, CaptureKind::Scalar) {
                    Some(p) => p,
                    None => return false,
                };
                push_bound(bounds, IndexBound::Scalar { array: ap as u32, scalar: sp as u32 });
                true
            }
            _ => false,
        },
        Expr::Binary { op, left, right, .. } => {
            let op_ok = match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => true,
                BinOp::Mod => matches!(**right, Expr::Int(n) if n > 0),
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => true,
                BinOp::Shl | BinOp::Shr => matches!(**right, Expr::Int(n) if (0..=63).contains(&n)),
                BinOp::FloorDiv => matches!(**right, Expr::Int(n) if n > 0),
                _ => false,
            };
            op_ok
                && value_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds)
                && value_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds)
        }
        Expr::Call { name, args, .. } => {
            eligible.contains(name.as_str())
                && jit_builtin_arity_ok(name, args.len())
                && args.iter().all(|a| value_eligible_cap_indexed(a, eligible, locals, pb, caps, bounds))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            cond_eligible_cap_indexed(cond, eligible, locals, pb, caps, bounds)
                && value_eligible_cap_indexed(then_branch, eligible, locals, pb, caps, bounds)
                && value_eligible_cap_indexed(else_branch, eligible, locals, pb, caps, bounds)
        }
        Expr::Let { bindings, body } => {
            let mut locals2 = locals.clone();
            for (n, v) in bindings {
                // A `let` that REBINDS the loop counter `pb` breaks the invariant the `Index`
                // arm relies on: `arr[pb]` no longer means `arr[counter]` — codegen would emit
                // an UNCHECKED load at the let-bound index, past what the VM's counter-range
                // pre-check validated → an out-of-bounds native read. It also can't shadow a
                // captured scalar index without changing what a `Scalar` bound refers to.
                // Refuse to JIT any `let` that rebinds `pb` OR a name already used as a scalar
                // index; the VM/tree-walker evaluate such a body correctly.
                if n.as_str() == pb
                    || bounds.iter().any(|b| {
                        matches!(b, IndexBound::Scalar { scalar, .. }
                            if caps.get(*scalar as usize).is_some_and(|c| c.name == *n))
                    })
                {
                    return false;
                }
                if !value_eligible_cap_indexed(v, eligible, &locals2, pb, caps, bounds) {
                    return false;
                }
                locals2.insert(n.as_str());
            }
            value_eligible_cap_indexed(body, eligible, &locals2, pb, caps, bounds)
        }
        _ => false,
    }
}

/// Condition twin of [`value_eligible_cap_indexed`] — comparisons/`and`/`or` whose operands
/// may index a captured array by the loop counter.
fn cond_eligible_cap_indexed(
    e: &Expr,
    eligible: &HashSet<&str>,
    locals: &HashSet<&str>,
    pb: &str,
    caps: &mut Vec<Capture>,
    bounds: &mut Vec<IndexBound>,
) -> bool {
    match e {
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds)
                && cond_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds)
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne)
                && value_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds)
                && value_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds)
        }
        _ => false,
    }
}

/// Is `body` a pure-`f64` reduce body over exactly `{pa, pb}` (accumulator and element)?
/// The same safe subset as the f64 map kernel — `+ - *`, the inline float builtins
/// (`sqrt`/`abs`/`min`/`max`), int/float literals — but BOTH binders are allowed and NO
/// free (captured) variable is (a capture's runtime type is unknown, so it can't be folded
/// as `f64`). `.reduce` is naive left-to-right, so the kernel's `fadd`/`fmul` in this order
/// is bit-exact to the interpreter (the property `differential_float_reduce_oracle` locks).
fn float_reduce_body_eligible(e: &Expr, pa: &str, pb: &str, user_fns: &HashSet<&str>) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) => true,
        Expr::Ident { name, .. } => name == pa || name == pb,
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                && float_reduce_body_eligible(left, pa, pb, user_fns)
                && float_reduce_body_eligible(right, pa, pb, user_fns)
        }
        Expr::Call { name, args, .. } => {
            jit_float_builtin_arity(name) == Some(args.len())
                && !user_fns.contains(name.as_str())
                && args.iter().all(|a| float_reduce_body_eligible(a, pa, pb, user_fns))
        }
        _ => false,
    }
}

/// Decide whether `reduce(init, (pa, pb) => body)` can JIT as a **scalar `f64`** fold — a
/// `Float`-literal init (so the accumulator is `f64`) and a pure-`f64` body over `{pa, pb}`.
/// Returns the body, or `None`. (The source must be a `Float` array; the VM checks that at
/// dispatch and falls back otherwise.)
pub fn reduce_jit_f64_body(init: &Expr, body: &Expr, pa: &str, pb: &str, user_fns: &HashSet<&str>) -> Option<Expr> {
    if matches!(init, Expr::Float(_)) && float_reduce_body_eligible(body, pa, pb, user_fns) {
        Some(body.clone())
    } else {
        None
    }
}

/// Bottom-up kind of a **mixed f64-range-reduce** body node: `pa` (the accumulator) is
/// `f64`, `pb` (the `i64` range counter) is `i64`. `None` if the node falls outside the
/// eligible shape. Mirrors [`gen_reduce_f64_mixed`] and the interpreter's `arith` exactly —
/// `+ - *` (Int OP Int stays `i64`/wrapping, mixed → `f64`), `sqrt`→Float, `abs` preserves
/// kind, `min`/`max` require both args the SAME kind (a mixed `min(float,int)` returns
/// whichever original operand wins → runtime-dependent type → rejected). No captures.
fn infer_reduce_f64_kind(e: &Expr, pa: &str, pb: &str, user_fns: &HashSet<&str>) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Ident { name, .. } => {
            if name == pa {
                Some(NumKind::Float) // the f64 accumulator
            } else if name == pb {
                Some(NumKind::Int) // the i64 range counter
            } else {
                None // captures excluded — a free var's runtime type is unknown
            }
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_reduce_f64_kind(left, pa, pb, user_fns)?;
            let rk = infer_reduce_f64_kind(right, pa, pb, user_fns)?;
            Some(if lk == NumKind::Float || rk == NumKind::Float {
                NumKind::Float
            } else {
                NumKind::Int
            })
        }
        // `/` is ALWAYS float division in Helix (even `Int / Int`), matching the interpreter's
        // `Div`. Both operands must be eligible; the result is `f64`. The interpreter RAISES on a
        // zero divisor while native `fdiv` yields inf/nan — so this only JITs under the caller's
        // `min`/`max` exclusion (see `f64_range_body_eligible`) + the VM's `is_finite` guard, which
        // together make a division-by-zero fall back to the exact-erroring bytecode loop.
        Expr::Binary { op: BinOp::Div, left, right, .. } => {
            infer_reduce_f64_kind(left, pa, pb, user_fns)?;
            infer_reduce_f64_kind(right, pa, pb, user_fns)?;
            Some(NumKind::Float)
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_reduce_f64_kind(&args[0], pa, pb, user_fns)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_reduce_f64_kind(&args[0], pa, pb, user_fns),
                ("min" | "max", 2) => {
                    let ka = infer_reduce_f64_kind(&args[0], pa, pb, user_fns)?;
                    let kb = infer_reduce_f64_kind(&args[1], pa, pb, user_fns)?;
                    if ka == kb { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Decide whether `range(..).reduce(init, (pa, pb) => body)` can JIT as a **scalar `f64`**
/// fold over the `i64` range counter: a `Float`-literal init and a mixed body whose inferred
/// root type is `Float` (so the result stores into the `f64` accumulator). Capture-free.
/// Returns the body, or `None`. (Unlike the array f64 reduce — where the element is itself
/// `f64` — here `pb` is the `i64` counter, so the body is lowered per-node, not pure-`f64`.)
pub fn reduce_jit_f64_range_body(init: &Expr, body: &Expr, pa: &str, pb: &str, user_fns: &HashSet<&str>) -> Option<Expr> {
    if matches!(init, Expr::Float(_)) && f64_range_body_eligible(body, pa, pb, user_fns) {
        Some(body.clone())
    } else {
        None
    }
}

/// Whether a scalar `f64` range-reduce body is JIT-eligible: root type `Float` (per
/// [`infer_reduce_f64_kind`], which now admits `/`). No restriction on `min`/`max` or nested
/// division is needed — the codegen threads a **poison flag** that records a zero divisor at the
/// division site itself (see [`gen_f64_typed`]), so the VM falls back on the exact `/0` the
/// interpreter raises on, regardless of whether a later op or iteration would "rescue" the inf.
/// Shared by the compile gate and the build re-gate so the two never drift.
fn f64_range_body_eligible(body: &Expr, pa: &str, pb: &str, user_fns: &HashSet<&str>) -> bool {
    infer_reduce_f64_kind(body, pa, pb, user_fns) == Some(NumKind::Float)
}

/// Whether `e` contains a `/` (float division) anywhere — a dividing reduce kernel carries a
/// poison out-param (see [`reduce_body_divides`] / [`gen_f64_typed`]).
pub fn expr_has_div(e: &Expr) -> bool {
    match e {
        Expr::Binary { op: BinOp::Div, .. } => true,
        Expr::Binary { left, right, .. } => expr_has_div(left) || expr_has_div(right),
        Expr::Unary { expr, .. } => expr_has_div(expr),
        Expr::Index { recv, index, .. } => expr_has_div(recv) || expr_has_div(index),
        Expr::Call { args, .. } => args.iter().any(expr_has_div),
        _ => false,
    }
}

/// Whether a reduce loop's (single) body contains a float division. A dividing scalar f64 reduce
/// kernel takes an extra `*mut i8` **poison** out-param that the codegen sets on any zero divisor;
/// the VM passes a poison cell, and on a set flag falls back to the exact-erroring bytecode loop
/// (native `fdiv` yields inf/nan where the interpreter raises on `/0`).
pub fn reduce_body_divides(rl: &crate::bytecode::ReduceLoop) -> bool {
    rl.bodies.len() == 1 && expr_has_div(&rl.bodies[0])
}

/// Whether `e` reads the identifier `name` anywhere. Used to prove a multi-accumulator `term` is
/// FREE of the accumulator. Literals plainly reference nothing; the arithmetic/index/call nodes
/// recurse; and CRUCIALLY any OTHER node shape (`let`, `if`, `match`, …) is conservatively assumed
/// to reference `name` — so a term built from an unrecognised shape declines the multi-acc transform
/// (`_ => true`). Under-approximating here (returning `false` for a node that DOES use the
/// accumulator, as a bare `_ => false` did for `let … in acc`) would wrongly enable multi-acc and
/// then panic in codegen (the accumulator is intentionally absent from the partials' `vars`).
fn expr_uses_ident(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Ident { name: n, .. } => n == name,
        Expr::Int(_) | Expr::Float(_) => false,
        Expr::Binary { left, right, .. } => expr_uses_ident(left, name) || expr_uses_ident(right, name),
        Expr::Unary { expr, .. } => expr_uses_ident(expr, name),
        Expr::Index { recv, index, .. } => expr_uses_ident(recv, name) || expr_uses_ident(index, name),
        Expr::Call { args, .. } => args.iter().any(|a| expr_uses_ident(a, name)),
        _ => true,
    }
}

/// The per-element `term` of a **multi-accumulator-eligible i64 SUM reduce**, or `None`. Eligible
/// when the scalar body is `acc + term` (or `term + acc`) — a top-level `+` with the accumulator
/// binder `pa` as EXACTLY one operand and `term` (the other operand) FREE of `pa`. The fold is then
/// a plain associative sum `init + Σ term(pb)`, which K independent partial accumulators compute
/// BIT-IDENTICALLY (integer add is associative + commutative) while breaking the single-accumulator
/// latency-bound dependency chain (~2.3× per core). i64 ONLY — f64 reassociation changes rounding
/// (non-associative), so a float reduce is never eligible.
fn reduce_multiacc_term(rl: &crate::bytecode::ReduceLoop) -> Option<&Expr> {
    if rl.float || rl.bodies.len() != 1 {
        return None;
    }
    let pa = rl.pa.as_str();
    if let Expr::Binary { op: BinOp::Add, left, right, .. } = &rl.bodies[0] {
        let l_acc = matches!(&**left, Expr::Ident { name, .. } if name == pa);
        let r_acc = matches!(&**right, Expr::Ident { name, .. } if name == pa);
        if l_acc && !r_acc && !expr_uses_ident(right, pa) {
            return Some(right);
        }
        if r_acc && !l_acc && !expr_uses_ident(left, pa) {
            return Some(left);
        }
    }
    None
}

/// Bottom-up kind of a **scalar f64 reduce body that indexes captured `f64` arrays by the
/// loop counter** (the float dot-product / weighted-sum case): `pa` is the `f64` accumulator,
/// `pb` the `i64` counter, and `arr[pb]` for a free array `arr` is an `f64` element →
/// records `arr` as a [`CaptureKind::ArrayF64`] capture (first-appearance order). A bare free
/// var is still rejected (its runtime type is unknown — no scalar f64 captures in v1b). Same
/// promotion rules as [`infer_reduce_f64_kind`]; the VM pre-checks each array's bounds before
/// the kernel does raw `f64` loads. `None` outside the eligible shape.
fn infer_f64_indexed(
    e: &Expr,
    pa: &str,
    pb: &str,
    caps: &mut Vec<Capture>,
    user_fns: &HashSet<&str>,
) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Ident { name, .. } => {
            if name == pa {
                Some(NumKind::Float)
            } else if name == pb {
                Some(NumKind::Int)
            } else {
                None // bare free var: unknown runtime type (only indexed array caps allowed)
            }
        }
        // `arr[pb]`: a free `f64` array read by exactly the counter → an `f64` element.
        Expr::Index { recv, index, .. } => match (&**recv, &**index) {
            (Expr::Ident { name: arr, .. }, Expr::Ident { name: idx, .. })
                if arr != pa && arr != pb && idx == pb =>
            {
                if record_cap(caps, arr, CaptureKind::ArrayF64) {
                    Some(NumKind::Float)
                } else {
                    None
                }
            }
            _ => None,
        },
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_f64_indexed(left, pa, pb, caps, user_fns)?;
            let rk = infer_f64_indexed(right, pa, pb, caps, user_fns)?;
            Some(if lk == NumKind::Float || rk == NumKind::Float {
                NumKind::Float
            } else {
                NumKind::Int
            })
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_f64_indexed(&args[0], pa, pb, caps, user_fns)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_f64_indexed(&args[0], pa, pb, caps, user_fns),
                ("min" | "max", 2) => {
                    let ka = infer_f64_indexed(&args[0], pa, pb, caps, user_fns)?;
                    let kb = infer_f64_indexed(&args[1], pa, pb, caps, user_fns)?;
                    if ka == kb { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Decide whether `range(..).reduce(0.0, (pa, pb) => body)` can JIT as a **scalar `f64` fold
/// that indexes captured `f64` arrays by the counter** — the float dot-product. A `Float`-
/// literal init, a body whose root infers `Float`, and **at least one** `ArrayF64` capture
/// (so this never competes with the capture-free [`reduce_jit_f64_range_body`]). Returns the
/// body + the ordered captures, or `None`. (The VM confirms each capture is a `Floats` array
/// and pre-checks its bounds at dispatch, falling back otherwise.)
pub fn reduce_jit_f64_range_captures(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    user_fns: &HashSet<&str>,
) -> Option<(Expr, Vec<Capture>)> {
    if !matches!(init, Expr::Float(_)) {
        return None;
    }
    let mut caps: Vec<Capture> = Vec::new();
    if infer_f64_indexed(body, pa, pb, &mut caps, user_fns) == Some(NumKind::Float)
        && !caps.is_empty()
        && caps.len() <= MAX_CAPTURES
    {
        Some((body.clone(), caps))
    } else {
        None
    }
}

/// Bottom-up kind of a node in a **multi-binder f64** body, given each binder's kind in
/// `binders` (the `f64` accumulator slots `$acc0…` plus the element/counter `pb`), or `None`
/// if it falls outside the eligible shape. The N-binder generalization of
/// [`infer_reduce_f64_kind`] — same promotion rules; used for f64 tuple/record accumulators.
fn infer_f64_typed(e: &Expr, binders: &HashMap<&str, NumKind>, user_fns: &HashSet<&str>) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Ident { name, .. } => binders.get(name.as_str()).copied(), // None = unknown var (no captures)
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_f64_typed(left, binders, user_fns)?;
            let rk = infer_f64_typed(right, binders, user_fns)?;
            Some(if lk == NumKind::Float || rk == NumKind::Float {
                NumKind::Float
            } else {
                NumKind::Int
            })
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_f64_typed(&args[0], binders, user_fns)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_f64_typed(&args[0], binders, user_fns),
                ("min" | "max", 2) => {
                    let ka = infer_f64_typed(&args[0], binders, user_fns)?;
                    let kb = infer_f64_typed(&args[1], binders, user_fns)?;
                    if ka == kb { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Typed codegen for a **multi-binder f64** body: each binder maps to its `(Variable, kind)`
/// in `binders`. Integer subexpressions wrap as `i64`, promoting to `f64` at the first float
/// operand — the interpreter's `arith` rule, the N-binder twin of [`gen_reduce_f64_mixed`].
/// Returns the value and its kind; eligibility ([`infer_f64_typed`]) guarantees presence.
fn gen_f64_typed(
    b: &mut FunctionBuilder,
    e: &Expr,
    binders: &HashMap<&str, (Variable, NumKind)>,
    arrays: &HashMap<&str, Variable>,
    poison: Option<Variable>,
) -> (ClValue, NumKind) {
    match e {
        Expr::Int(i) => (b.ins().iconst(I64, *i), NumKind::Int),
        Expr::Float(f) => (b.ins().f64const(*f), NumKind::Float),
        Expr::Ident { name, .. } => {
            let (var, kind) = binders[name.as_str()];
            (b.use_var(var), kind)
        }
        // `arr[counter]` reading a captured `f64` array (float dot-product): `recv` is bound in
        // `arrays` to the packed base pointer, `index` is the i64 counter. The VM pre-checked
        // the whole counter range is in bounds, so this raw `f64` load is safe. Only the
        // scalar-with-`ArrayF64`-caps path populates `arrays` (empty for tuple/record reduces).
        Expr::Index { recv, index, .. } => {
            let name = match &**recv {
                Expr::Ident { name, .. } => name.as_str(),
                _ => unreachable!("ineligible f64 index receiver reached codegen"),
            };
            let base = b.use_var(arrays[name]);
            let (idx, _) = gen_f64_typed(b, index, binders, arrays, poison);
            let off = b.ins().imul_imm(idx, 8);
            let addr = b.ins().iadd(base, off);
            (b.ins().load(F64, MemFlags::trusted(), addr, 0), NumKind::Float)
        }
        Expr::Binary { op, left, right, .. } => {
            let (lv, lk) = gen_f64_typed(b, left, binders, arrays, poison);
            let (rv, rk) = gen_f64_typed(b, right, binders, arrays, poison);
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
                    // Record it: OR `divisor == 0.0` into the poison flag (accumulated across all
                    // iterations), which the VM checks after the loop and, if set, falls back to
                    // the exact-erroring bytecode loop. `rf == 0.0` is bit-identical to the
                    // interpreter's `b == 0.0` divisor check (and catches −0.0 too), so the
                    // fallback fires on exactly the `/0` the interpreter reports — regardless of
                    // whether a later op or iteration would "rescue" the resulting inf/nan.
                    BinOp::Div => {
                        if let Some(p) = poison {
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
                let (av, ak) = gen_f64_typed(b, &args[0], binders, arrays, poison);
                let af = if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                (b.ins().sqrt(af), NumKind::Float)
            }
            "abs" => {
                let (av, ak) = gen_f64_typed(b, &args[0], binders, arrays, poison);
                match ak {
                    NumKind::Int => (b.ins().iabs(av), NumKind::Int),
                    NumKind::Float => (b.ins().fabs(av), NumKind::Float),
                }
            }
            "min" | "max" => {
                let (av, ak) = gen_f64_typed(b, &args[0], binders, arrays, poison);
                let (cv, _ck) = gen_f64_typed(b, &args[1], binders, arrays, poison);
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

/// `true` if `init` makes the reduce accumulator `f64`: a `Float` literal (scalar), or a
/// `Tuple`/`Record` of all-`Float` literals (multi-slot). The compiler routes these to the
/// f64 reduce paths instead of the i64 ones (which key only off body shape).
pub fn is_float_acc_init(init: &Expr) -> bool {
    match init {
        Expr::Float(_) => true,
        Expr::Tuple(items) if items.len() >= 2 => items.iter().all(|e| matches!(e, Expr::Float(_))),
        Expr::Record(fields) if fields.len() >= 2 => {
            fields.iter().all(|(_, e)| matches!(e, Expr::Float(_)))
        }
        _ => false,
    }
}

/// A tuple accumulator may have at most this many `i64` slots; a wider one runs on the
/// bytecode loop. The reduce kernel keeps every slot in a register.
pub const MAX_ACC_SLOTS: usize = 4;

/// The synthetic slot identifiers (`$acc0…`), as `'static` strings so the codegen's
/// `vars` map (keyed by the kernel lifetime) can hold them without lifetime juggling.
/// `$` can't appear in user source, so they never collide. Length == `MAX_ACC_SLOTS`.
const ACC_IDENTS: [&str; MAX_ACC_SLOTS] = ["$acc0", "$acc1", "$acc2", "$acc3"];

/// The identifier bound to accumulator slot `k`. A tuple body's `pa[k]` is rewritten to
/// this so the existing `i64` codegen handles it unchanged.
pub fn acc_ident(k: usize) -> String {
    ACC_IDENTS[k].to_string()
}

/// Rewrite an accumulator slot access — `pa[k]` (tuple) or `pa.field` (record, mapped to
/// its position in `fields`) — to the slot ident `$acc{k}` throughout `e`. Only the
/// `i64`-eligible forms are recursed into; any other form is cloned as-is (so an
/// unsubstituted `pa[..]`/`pa.x` stays and fails eligibility — a safe fallback, never a
/// miscompile).
fn subst_acc(e: &Expr, pa: &str, n: usize, fields: &[String]) -> Expr {
    if let Expr::Index { recv, index, line, col } = e
        && let Expr::Ident { name, .. } = recv.as_ref()
        && name == pa
        && let Expr::Int(k) = index.as_ref()
        && *k >= 0
        && (*k as usize) < n
    {
        return Expr::Ident { name: acc_ident(*k as usize), line: *line, col: *col };
    }
    if let Expr::Field { recv, name, line, col } = e
        && let Expr::Ident { name: rn, .. } = recv.as_ref()
        && rn == pa
        && let Some(k) = fields.iter().position(|f| f == name)
    {
        return Expr::Ident { name: acc_ident(k), line: *line, col: *col };
    }
    let s = |c: &Expr| Box::new(subst_acc(c, pa, n, fields));
    match e {
        Expr::Binary { op, left, right, line, col } => Expr::Binary {
            op: op.clone(),
            left: s(left),
            right: s(right),
            line: *line,
            col: *col,
        },
        Expr::Unary { op, expr, line, col } => Expr::Unary {
            op: op.clone(),
            expr: s(expr),
            line: *line,
            col: *col,
        },
        Expr::Call { name, args, line, col } => Expr::Call {
            name: name.clone(),
            args: args.iter().map(|a| subst_acc(a, pa, n, fields)).collect(),
            line: *line,
            col: *col,
        },
        Expr::If { cond, then_branch, else_branch, line, col } => Expr::If {
            cond: s(cond),
            then_branch: s(then_branch),
            else_branch: s(else_branch),
            line: *line,
            col: *col,
        },
        Expr::Let { bindings, body } => Expr::Let {
            bindings: bindings
                .iter()
                .map(|(nm, v)| (nm.clone(), subst_acc(v, pa, n, fields)))
                .collect(),
            body: s(body),
        },
        other => other.clone(),
    }
}

/// Substitute the slot accesses in each component (already in slot order) and keep them
/// only if every one is `i64`-eligible over `{$acc0.., pb}`.
fn check_slot_bodies(
    comps: &[&Expr],
    pa: &str,
    pb: &str,
    fields: &[String],
    fns: &HashSet<&str>,
) -> Option<Vec<Expr>> {
    let n = comps.len();
    let names: Vec<String> = (0..n).map(acc_ident).collect();
    let mut locals: HashSet<&str> = HashSet::new();
    for nm in &names {
        locals.insert(nm.as_str());
    }
    locals.insert(pb);
    let bodies: Vec<Expr> = comps.iter().map(|c| subst_acc(c, pa, n, fields)).collect();
    bodies
        .iter()
        .all(|c| value_eligible(c, fns, &locals, NumKind::Int))
        .then_some(bodies)
}

/// Like [`check_slot_bodies`], but for an **all-`f64`** tuple/record accumulator: each
/// component is substituted (`pa[k]`/`pa.field` → `$acc{k}`) and kept only if it is
/// `f64`-eligible over `{$acc0…(Float), pb}` with root `Float`. `pb_kind` is `Int` for a
/// range counter, `Float` for a `Float`-array element.
fn check_slot_bodies_f64(
    comps: &[&Expr],
    pa: &str,
    pb: &str,
    pb_kind: NumKind,
    fields: &[String],
    user_fns: &HashSet<&str>,
) -> Option<Vec<Expr>> {
    let n = comps.len();
    let bodies: Vec<Expr> = comps.iter().map(|c| subst_acc(c, pa, n, fields)).collect();
    let mut binders: HashMap<&str, NumKind> = HashMap::new();
    for &slot in ACC_IDENTS.iter().take(n) {
        binders.insert(slot, NumKind::Float);
    }
    binders.insert(pb, pb_kind);
    bodies
        .iter()
        .all(|c| infer_f64_typed(c, &binders, user_fns) == Some(NumKind::Float))
        .then_some(bodies)
}

/// Decide whether a `reduce(init, (pa, pb) => body)` with an **all-`Float`** tuple/record
/// init can JIT as a multi-slot `f64` fold, returning the substituted component bodies
/// (`$acc0…`). `pb_is_int` is `true` for a range counter, `false` for a `Float`-array
/// element. `None` → not eligible (run the bytecode loop). Mirrors [`reduce_jit_bodies`]'s
/// tuple/record branches, but every slot is `f64` and the components are typed per-node.
pub fn reduce_jit_f64_tuple_bodies(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    pb_is_int: bool,
    user_fns: &HashSet<&str>,
) -> Option<Vec<Expr>> {
    let pb_kind = if pb_is_int { NumKind::Int } else { NumKind::Float };
    if let (Expr::Tuple(inits), Expr::Tuple(comps)) = (init, body) {
        let n = comps.len();
        if n != inits.len()
            || !(2..=MAX_ACC_SLOTS).contains(&n)
            || !inits.iter().all(|e| matches!(e, Expr::Float(_)))
        {
            return None;
        }
        let refs: Vec<&Expr> = comps.iter().collect();
        return check_slot_bodies_f64(&refs, pa, pb, pb_kind, &[], user_fns);
    }
    if let (Expr::Record(inits), Expr::Record(comps)) = (init, body) {
        let n = inits.len();
        if comps.len() != n
            || !(2..=MAX_ACC_SLOTS).contains(&n)
            || !inits.iter().all(|(_, e)| matches!(e, Expr::Float(_)))
        {
            return None;
        }
        // Same field-order requirement as the i64 path: components map to the init's order.
        let fields: Vec<String> = inits.iter().map(|(k, _)| k.clone()).collect();
        if comps.iter().map(|(k, _)| k).ne(fields.iter()) {
            return None;
        }
        let ordered: Vec<&Expr> = comps.iter().map(|(_, e)| e).collect();
        return check_slot_bodies_f64(&ordered, pa, pb, pb_kind, &fields, user_fns);
    }
    None
}

/// Decide whether a `reduce(init, (pa, pb) => body)` can JIT, and if so return its
/// component bodies (slot accesses already substituted to `$acc0…`). `Some([body])` for a
/// scalar `i64` accumulator; `Some([e0, e1, …])` for a 2..=MAX_ACC_SLOTS **tuple** (`a[k]`)
/// or **record** (`a.field`) accumulator whose every component is `i64`-eligible. A record
/// body's components are reordered to the init record's field order (so component `k` is
/// always slot `k`). `None` → run the bytecode loop.
pub fn reduce_jit_bodies(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
) -> Option<Vec<Expr>> {
    if reduce_loop_eligible(body, pa, pb, fns) {
        return Some(vec![body.clone()]);
    }
    if let (Expr::Tuple(inits), Expr::Tuple(comps)) = (init, body) {
        let n = comps.len();
        if n != inits.len() || !(2..=MAX_ACC_SLOTS).contains(&n) {
            return None;
        }
        let refs: Vec<&Expr> = comps.iter().collect();
        return check_slot_bodies(&refs, pa, pb, &[], fns);
    }
    if let (Expr::Record(inits), Expr::Record(comps)) = (init, body) {
        let n = inits.len();
        if comps.len() != n || !(2..=MAX_ACC_SLOTS).contains(&n) {
            return None;
        }
        // Require the body's fields to be in the SAME order as the init's: the slots map
        // to that order, and the tree-walker's result record carries the body's field
        // order — matching them keeps the JIT result byte-identical (a reordered body
        // would still be value-equal but display its fields in a different order, so it
        // falls back to the bytecode loop instead).
        let fields: Vec<String> = inits.iter().map(|(k, _)| k.clone()).collect();
        if comps.iter().map(|(k, _)| k).ne(fields.iter()) {
            return None;
        }
        let ordered: Vec<&Expr> = comps.iter().map(|(_, e)| e).collect();
        return check_slot_bodies(&ordered, pa, pb, &fields, fns);
    }
    None
}

/// Re-check (at JIT-compile time) that already-substituted reduce bodies are `i64`-eligible
/// — a scalar (1 body) over `{pa, pb}`, or a tuple (2..=MAX_ACC_SLOTS bodies) over the slots
/// `{$acc0.., pb}`. Shared by the range reduce loop and the fused reduce sink.
fn bodies_eligible(pa: &str, pb: &str, bodies: &[Expr], fns: &HashSet<&str>) -> bool {
    if bodies.len() == 1 {
        return reduce_loop_eligible(&bodies[0], pa, pb, fns);
    }
    let n = bodies.len();
    if !(2..=MAX_ACC_SLOTS).contains(&n) {
        return false;
    }
    let names: Vec<String> = (0..n).map(acc_ident).collect();
    let mut locals: HashSet<&str> = HashSet::new();
    for nm in &names {
        locals.insert(nm.as_str());
    }
    locals.insert(pb);
    bodies.iter().all(|c| value_eligible(c, fns, &locals, NumKind::Int))
}

fn reduce_bodies_eligible(rl: &crate::bytecode::ReduceLoop, fns: &HashSet<&str>, user_fns: &HashSet<&str>) -> bool {
    // An f64 accumulator over the i64 counter: capture-free, every component's root `Float`,
    // exactly what `define_reduce_loop`'s float path lowers (via `gen_f64_typed`). Scalar (1
    // body over `{pa, pb}`) or tuple (N>1 substituted bodies over `{$acc0…, pb}`).
    if rl.float {
        // v1b: a float SCALAR body indexing captured `f64` arrays by the counter (the float
        // dot-product). Re-run the same indexed collector and require it reproduce `rl.captures`
        // exactly — all `ArrayF64`, body root `Float` — so the build gate matches the compile
        // gate (`define_reduce_loop` binds exactly these caps into its `arrays` map).
        if !rl.captures.is_empty() {
            if rl.bodies.len() != 1 {
                return false;
            }
            let mut caps: Vec<Capture> = Vec::new();
            let root = infer_f64_indexed(&rl.bodies[0], &rl.pa, &rl.pb, &mut caps, user_fns);
            return root == Some(NumKind::Float)
                && caps == rl.captures
                && caps.iter().all(|c| c.kind == CaptureKind::ArrayF64)
                && caps.len() <= MAX_CAPTURES;
        }
        let n = rl.bodies.len();
        if n == 1 {
            // Identical gate to the compiler's `reduce_jit_f64_range_body` (root `Float`, and the
            // division/min-max soundness rule) so the build never lowers a body the compiler
            // rejected — or vice versa.
            return f64_range_body_eligible(&rl.bodies[0], &rl.pa, &rl.pb, user_fns);
        }
        if !(2..=MAX_ACC_SLOTS).contains(&n) {
            return false;
        }
        let mut binders: HashMap<&str, NumKind> = HashMap::new();
        for &slot in ACC_IDENTS.iter().take(n) {
            binders.insert(slot, NumKind::Float);
        }
        binders.insert(rl.pb.as_str(), NumKind::Int);
        return rl.bodies.iter().all(|c| infer_f64_typed(c, &binders, user_fns) == Some(NumKind::Float));
    }
    // A scalar captured body: re-run the SAME indexed collector the compiler used and
    // require it reproduce `rl.captures` exactly — same names, kinds, and order. This keeps
    // the build gate identical to the compile gate: `define_reduce_loop` binds exactly these
    // captures (scalar values and array bases loaded from the `caps` pointer), so any drift
    // (a body eligibility accepted but the build can't lower, or a different capture set)
    // is caught here and the whole loop falls back to the VM. v1a lowers `Scalar` +
    // `ArrayI64`; an `ArrayF64` cap belongs to the f64 variant, not yet lowered → reject.
    if rl.bodies.len() == 1 && !rl.captures.is_empty() {
        if rl.captures.iter().any(|c| c.kind == CaptureKind::ArrayF64) {
            return false;
        }
        let mut locals: HashSet<&str> = HashSet::new();
        locals.insert(rl.pa.as_str());
        locals.insert(rl.pb.as_str());
        let mut caps: Vec<Capture> = Vec::new();
        let mut bounds: Vec<IndexBound> = Vec::new();
        let ok =
            value_eligible_cap_indexed(&rl.bodies[0], fns, &locals, rl.pb.as_str(), &mut caps, &mut bounds);
        // The build gate must reproduce BOTH the capture set and the bounds obligations the VM
        // will check — a drift in either would run the kernel with a pre-check that doesn't match
        // its actual `arr[…]` accesses (an out-of-bounds hazard), so require an exact match.
        return ok && caps == rl.captures && bounds == rl.index_bounds && caps.len() <= MAX_CAPTURES;
    }
    bodies_eligible(&rl.pa, &rl.pb, &rl.bodies, fns)
}

/// True if a `map` body is a pure `i64` value expression over its single binder (calling
/// only `fns`) — the same shape as a reduce body, lowered to a per-element kernel.
pub fn map_kernel_eligible(body: &Expr, binder: &str, fns: &HashSet<&str>) -> bool {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    value_eligible(body, fns, &locals, NumKind::Int)
}

/// At most this many captured variables per kernel (bounds the `caps` slice).
pub const MAX_CAPTURES: usize = 8;

/// Like [`map_kernel_eligible`] but a body referencing **free (captured) variables** is
/// still eligible — each free `i64` variable is recorded (in first-appearance order) and
/// passed to the kernel as a loop-invariant `caps[i]`. Returns the ordered capture names,
/// or `None` if the body is ineligible (a float literal, `/`, a non-eligible call, …) or
/// captures more than [`MAX_CAPTURES`]. Same i64-closed rules as `value_eligible(Int)`.
pub fn map_kernel_captures(body: &Expr, binder: &str, fns: &HashSet<&str>) -> Option<Vec<String>> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    let mut caps: Vec<String> = Vec::new();
    if value_eligible_cap(body, fns, &locals, &mut caps) && caps.len() <= MAX_CAPTURES {
        Some(caps)
    } else {
        None
    }
}

/// f64 `map` eligibility (over a **Floats** source array). The body must use only
/// `+ - *` over the binder, int/float literals, and captured variables, and it must
/// **reference the binder** — so the result is provably `Float` (the binder is `f64`
/// and float-ness propagates through `+ - *`), matching the interpreter. A constant or
/// capture-only body (whose type could be `Int`) is excluded, as are `/` (the
/// interpreter raises on /0 where native fdiv yields ±inf), `%`, `if`, comparisons, and
/// calls — the safe subset that can't introduce a JIT↔interpreter divergence. Returns
/// the ordered captures (passed to the kernel as `f64`), or `None`.
pub fn map_kernel_captures_f64(
    body: &Expr,
    binder: &str,
    user_fns: &HashSet<&str>,
) -> Option<Vec<String>> {
    let mut caps: Vec<String> = Vec::new();
    let mut uses_binder = false;
    if f64_body_eligible(body, binder, &mut caps, &mut uses_binder, user_fns)
        && uses_binder
        && caps.len() <= MAX_CAPTURES
    {
        Some(caps)
    } else {
        None
    }
}

fn f64_body_eligible(
    e: &Expr,
    binder: &str,
    caps: &mut Vec<String>,
    uses_binder: &mut bool,
    user_fns: &HashSet<&str>,
) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) => true,
        Expr::Ident { name, .. } => {
            if name == binder {
                *uses_binder = true;
            } else if !caps.iter().any(|c| c == name) {
                caps.push(name.clone());
            }
            true
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                && f64_body_eligible(left, binder, caps, uses_binder, user_fns)
                && f64_body_eligible(right, binder, caps, uses_binder, user_fns)
        }
        // `sqrt`/`abs`/`min`/`max` (emitted inline by `gen_builtin_f64`) — only the real
        // builtin, never a user function of the same name (which the f64 kernel can't call).
        Expr::Call { name, args, .. } => {
            jit_float_builtin_arity(name) == Some(args.len())
                && !user_fns.contains(name.as_str())
                && args.iter().all(|a| f64_body_eligible(a, binder, caps, uses_binder, user_fns))
        }
        _ => false,
    }
}

/// Is `body` a **mixed** `Int`-source → `Float` map: an `f64`-producing expression over
/// an `i64` element? Eligible when it uses the binder, is built only from `+ - *` over the
/// binder / int / float literals (no captures — a capture's runtime type is unknown at
/// compile time, and an `Int` capture in an `Int` subexpression must wrap as `i64`, which
/// we couldn't guarantee), and its inferred root type is `Float` (else it's a pure `i64`
/// map). The kernel ([`define_array_kernel`] with `mixed`) types every node bottom-up by
/// the interpreter's promotion rule — `Int OP Int` stays `i64` (wrapping `iadd/isub/imul`),
/// and the *first* `Float` operand promotes via `fcvt_from_sint` — so it matches the
/// interpreter bit-for-bit, including any `i64` wrap in an integer subexpression.
pub fn mixed_map_eligible(body: &Expr, binder: &str, user_fns: &HashSet<&str>) -> bool {
    let mut uses_binder = false;
    matches!(infer_mixed_kind(body, binder, &mut uses_binder, user_fns), Some(NumKind::Float))
        && uses_binder
}

/// Bottom-up type of a mixed-map node, or `None` if it contains anything outside the
/// eligible shape (a non-binder ident, a non-`{+,-,*}` operator, a non-eligible call, …).
/// Mirrors the codegen in [`gen_value_typed`] exactly. The pure builtins `sqrt`/`abs`/
/// `min`/`max` are typed like the interpreter: `sqrt` is always `Float`; `abs` preserves
/// its arg kind; `min`/`max` need both args the **same** kind (a mixed `min(int, float)`
/// returns whichever original operand wins, so its type is runtime-dependent — rejected).
fn infer_mixed_kind(
    e: &Expr,
    binder: &str,
    uses_binder: &mut bool,
    user_fns: &HashSet<&str>,
) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_mixed_kind(&args[0], binder, uses_binder, user_fns)?;
                    Some(NumKind::Float) // sqrt always returns Float
                }
                ("abs", 1) => infer_mixed_kind(&args[0], binder, uses_binder, user_fns), // preserves kind
                ("min" | "max", 2) => {
                    let ka = infer_mixed_kind(&args[0], binder, uses_binder, user_fns)?;
                    let kb = infer_mixed_kind(&args[1], binder, uses_binder, user_fns)?;
                    if ka == kb { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        Expr::Ident { name, .. } => {
            if name == binder {
                *uses_binder = true;
                Some(NumKind::Int) // the `i64` element
            } else {
                None // captures are excluded from the mixed kernel
            }
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_mixed_kind(left, binder, uses_binder, user_fns)?;
            let rk = infer_mixed_kind(right, binder, uses_binder, user_fns)?;
            Some(if lk == NumKind::Float || rk == NumKind::Float {
                NumKind::Float
            } else {
                NumKind::Int
            })
        }
        // The i64-closed integer ops (`%`, `//`, bitwise, shifts) — the SAME safe subset as
        // `value_eligible`, so an integer subexpression like `j % 97` in a float-producing map
        // body (`(j % 97) * 1.0`) stays `i64` and promotes at the first float operand, instead
        // of forcing the whole map onto the VM. BOTH operands must be `Int` (these ops are
        // meaningless on `f64`); the result is `Int`. Same const-restrictions as `value_eligible`.
        Expr::Binary {
            op: op @ (BinOp::Mod | BinOp::FloorDiv | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr),
            left,
            right,
            ..
        } => {
            let op_ok = match op {
                BinOp::Mod | BinOp::FloorDiv => matches!(**right, Expr::Int(n) if n > 0),
                BinOp::Shl | BinOp::Shr => matches!(**right, Expr::Int(n) if (0..=63).contains(&n)),
                _ => true, // bitwise: unconditionally i64-closed
            };
            if !op_ok {
                return None;
            }
            let lk = infer_mixed_kind(left, binder, uses_binder, user_fns)?;
            let rk = infer_mixed_kind(right, binder, uses_binder, user_fns)?;
            if lk == NumKind::Int && rk == NumKind::Int {
                Some(NumKind::Int)
            } else {
                None // an i64-only op with a Float operand is not a valid Helix expression
            }
        }
        _ => None,
    }
}

fn value_eligible_cap(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, caps: &mut Vec<String>) -> bool {
    match e {
        Expr::Int(_) => true,
        // Float literals need the (dormant) f64 specialization; not this i64 kernel.
        Expr::Float(_) => false,
        Expr::Ident { name, .. } => {
            if locals.contains(name.as_str()) {
                true
            } else {
                // A free variable → a captured value. Record once, in first-appearance
                // order, so the codegen's `caps[i]` and the VM's load order agree.
                if !caps.iter().any(|c| c == name) {
                    caps.push(name.clone());
                }
                true
            }
        }
        Expr::Binary { op, left, right, .. } => {
            let op_ok = match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => true,
                // `%` only by a positive integer constant (total `rem_euclid`, no `%0`).
                BinOp::Mod => matches!(**right, Expr::Int(n) if n > 0),
                // Bitwise ops are unconditionally i64-closed (this is the i64 kernel).
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => true,
                // `<<`/`>>` only by an in-range constant (0..=63); `//` only by a
                // positive constant — same safe subset as `value_eligible` above.
                BinOp::Shl | BinOp::Shr => matches!(**right, Expr::Int(n) if (0..=63).contains(&n)),
                BinOp::FloorDiv => matches!(**right, Expr::Int(n) if n > 0),
                _ => false, // `/` excluded: not i64-closed; native fdiv diverges on /0
            };
            op_ok
                && value_eligible_cap(left, eligible, locals, caps)
                && value_eligible_cap(right, eligible, locals, caps)
        }
        Expr::Call { name, args, .. } => {
            eligible.contains(name.as_str())
                && jit_builtin_arity_ok(name, args.len())
                && args.iter().all(|a| value_eligible_cap(a, eligible, locals, caps))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            cond_eligible_cap(cond, eligible, locals, caps)
                && value_eligible_cap(then_branch, eligible, locals, caps)
                && value_eligible_cap(else_branch, eligible, locals, caps)
        }
        Expr::Let { bindings, body } => {
            let mut locals2 = locals.clone();
            for (n, v) in bindings {
                if !value_eligible_cap(v, eligible, &locals2, caps) {
                    return false;
                }
                locals2.insert(n.as_str());
            }
            value_eligible_cap(body, eligible, &locals2, caps)
        }
        _ => false,
    }
}

fn cond_eligible_cap(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, caps: &mut Vec<String>) -> bool {
    match e {
        // `and`/`or` in condition position (see `cond_eligible`); recurse through the
        // capture-collecting twin so captured names inside the operands are still found.
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible_cap(left, eligible, locals, caps)
                && cond_eligible_cap(right, eligible, locals, caps)
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne)
                && value_eligible_cap(left, eligible, locals, caps)
                && value_eligible_cap(right, eligible, locals, caps)
        }
        _ => false,
    }
}

/// True if a `filter`/`where` predicate is a pure `i64` comparison over its binder
/// (`it > 5`, `it % 2 == 0`, `is_even(it)`, …), calling only `fns`.
pub fn filter_kernel_eligible(body: &Expr, binder: &str, fns: &HashSet<&str>) -> bool {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    cond_eligible(body, fns, &locals, NumKind::Int)
}

fn eligible_set<'a>(funcs: &[FnDef<'a>], kind: NumKind) -> HashSet<&'a str> {
    // Exclude every function on a recursion *cycle* — directly self-recursive OR
    // mutually recursive. A JIT'd function recurses on the native stack with no
    // depth guard, so unbounded recursion (a missing base case) would overflow the
    // native stack and crash the process instead of raising a clean, catchable
    // error. This is a transitive call-graph check, not just a direct self-call
    // test: the JIT's memory safety must NOT silently depend on the front-end's
    // define-before-use rule (which makes mutual recursion unrepresentable today,
    // but is a front-end policy that could change — see `recursive_funcs`).
    // Recursive functions run on the depth-guarded VM (or are memoized) instead —
    // EXCEPT directly tail-self-recursive ones (`tail_loopable_set`), which lower to
    // native LOOPS (parameter rebind + jump, no stack growth), so the native-stack
    // hazard above does not apply to them.
    let recursive = recursive_funcs(funcs);
    let tail_loop = tail_loopable_set(funcs);
    let mut eligible: HashSet<&str> = funcs
        .iter()
        .filter(|f| {
            f.params.len() <= MAX_ARITY
                && (!recursive.contains(f.name) || tail_loop.contains(f.name))
        })
        .map(|f| f.name)
        .collect();
    // Pure scalar builtins the kernel codegen can emit inline (`abs`/`min`/`max`) — usable
    // from a kernel body just like an eligible user function, EXCEPT when a user function
    // of the same name shadows the builtin (then the call must dispatch to the user fn, so
    // the JIT must not treat it as the builtin). Added before the fixpoint so user
    // functions that call them are themselves eligible. This is the single source the
    // compiler (`int_eligible_fns`) and the JIT build both read, so they always agree.
    for (name, _) in JIT_SCALAR_BUILTINS {
        if !funcs.iter().any(|f| f.name == *name) {
            eligible.insert(name);
        }
    }
    loop {
        let snapshot = eligible.clone();
        let mut changed = false;
        for f in funcs {
            if snapshot.contains(f.name) {
                let locals: HashSet<&str> = f.params.iter().map(|(n, _)| n.as_str()).collect();
                if !value_eligible(f.body, &snapshot, &locals, kind) {
                    eligible.remove(f.name);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    eligible
}

/// Names of functions that lie on a call-graph cycle — directly self-recursive
/// (`f` calls `f`) or mutually recursive (`f` -> `g` -> ... -> `f`). Such a
/// function can reach itself through call edges, so JIT-compiling it would put
/// unguarded recursion on the native stack. The check is *transitive* by design:
/// it keeps the JIT memory-safe regardless of whether the front-end permits the
/// cycle to be written. Today the parser's define-before-use rule makes mutual
/// recursion unrepresentable, so this currently coincides with the direct
/// self-call test — but the JIT no longer *depends* on that front-end policy.
fn recursive_funcs<'a>(funcs: &[FnDef<'a>]) -> HashSet<&'a str> {
    let n = funcs.len();
    // Call graph over the user functions: edge i -> j iff funcs[i]'s body calls
    // funcs[j] (by name). `body_calls` is the per-edge primitive.
    let adj: Vec<Vec<usize>> = funcs
        .iter()
        .map(|f| (0..n).filter(|&j| body_calls(f.body, funcs[j].name)).collect())
        .collect();
    let mut recursive = HashSet::new();
    for i in 0..n {
        // Reachability: can function i reach itself through call edges?
        let mut seen = vec![false; n];
        let mut stack = adj[i].clone();
        while let Some(u) = stack.pop() {
            if u == i {
                recursive.insert(funcs[i].name);
                break;
            }
            if !seen[u] {
                seen[u] = true;
                stack.extend_from_slice(&adj[u]);
            }
        }
    }
    recursive
}

/// True if `e` contains a call to function `name`. The per-edge primitive for the
/// `recursive_funcs` call graph. Only the node kinds that can appear in an eligible
/// body need traversal; anything else means the function is ineligible anyway.
fn body_calls(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Call { name: callee, args, .. } => {
            callee == name || args.iter().any(|a| body_calls(a, name))
        }
        Expr::Binary { left, right, .. } => {
            body_calls(left, name) || body_calls(right, name)
        }
        Expr::Unary { expr, .. } => body_calls(expr, name),
        Expr::If { cond, then_branch, else_branch, .. } => {
            body_calls(cond, name)
                || body_calls(then_branch, name)
                || body_calls(else_branch, name)
        }
        Expr::Let { bindings, body } => {
            bindings.iter().any(|(_, v)| body_calls(v, name))
                || body_calls(body, name)
        }
        // A `match` is i64-eligible (`match_eligible`), so a call can hide in its
        // scrutinee, a guard, or an arm body — all must be traversed. Without this arm a
        // self-call inside a match evaded `recursive_funcs`, and the function was JIT'd
        // with unguarded NATIVE recursion (deep input = native stack overflow = process
        // crash, where the VM raises its clean depth error).
        Expr::Match { scrutinee, arms, .. } => {
            body_calls(scrutinee, name)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(|g| body_calls(g, name))
                        || body_calls(&a.body, name)
                })
        }
        _ => false,
    }
}

/// True iff every call to `self_name` in `e` sits in **tail position** — reachable from
/// the function root only through `if` branches and `let` bodies — and passes exactly
/// `arity` arguments. Such a call is a loop back-edge, not real recursion: the JIT lowers
/// it by rebinding the parameters and jumping to the loop header ([`gen_tail`]), growing
/// no native stack — precisely the VM's `TailCallFn` frame-reuse semantics (the tail-call
/// peephole in `bytecode.rs`). Conditions, `let` binding values, and the tail call's own
/// arguments must be self-free: a self-call there needs a real activation record.
fn self_calls_tail_only(e: &Expr, self_name: &str, arity: usize) -> bool {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => {
            !body_calls(cond, self_name)
                && self_calls_tail_only(then_branch, self_name, arity)
                && self_calls_tail_only(else_branch, self_name, arity)
        }
        Expr::Let { bindings, body } => {
            bindings.iter().all(|(_, v)| !body_calls(v, self_name))
                && self_calls_tail_only(body, self_name, arity)
        }
        Expr::Call { name, args, .. } if name == self_name => {
            args.len() == arity && args.iter().all(|a| !body_calls(a, self_name))
        }
        other => !body_calls(other, self_name),
    }
}

/// True iff `funcs[i]` lies on a call cycle of length ≥ 2 — it can reach itself through
/// some *other* function. The direct self-edge is deliberately ignored: a purely
/// tail-self-recursive function lowers to a native loop, but one on a mutual cycle would
/// still recurse natively through its partner, so it must stay excluded. (Mutual
/// recursion is unrepresentable under today's define-before-use rule; like
/// `recursive_funcs`, this check refuses to depend on that front-end policy.)
fn on_mutual_cycle(i: usize, funcs: &[FnDef]) -> bool {
    let n = funcs.len();
    let mut seen = vec![false; n];
    // First hop: every callee EXCEPT the direct self-edge.
    let mut stack: Vec<usize> =
        (0..n).filter(|&j| j != i && body_calls(funcs[i].body, funcs[j].name)).collect();
    while let Some(u) = stack.pop() {
        if u == i {
            return true;
        }
        if !seen[u] {
            seen[u] = true;
            stack.extend((0..n).filter(|&j| body_calls(funcs[u].body, funcs[j].name)));
        }
    }
    false
}

/// The directly tail-self-recursive functions the JIT lowers as native **loops** instead
/// of excluding for recursion: every self-call is in tail position with the right arity
/// (`self_calls_tail_only`) and the function is on no mutual cycle (`on_mutual_cycle`).
/// The back-edge grows no native stack, so the unguarded-recursion hazard that excludes
/// recursive functions does not apply; a missing base case spins exactly like the VM's
/// `TailCallFn` loop would (identical semantics), it does not overflow. Pure and
/// deterministic — `eligible_set` (read by both the bytecode compiler and the JIT build)
/// and `build`'s codegen branch call it identically, so all sites always agree.
fn tail_loopable_set<'a>(funcs: &[FnDef<'a>]) -> HashSet<&'a str> {
    funcs
        .iter()
        .enumerate()
        .filter(|(_, f)| body_calls(f.body, f.name))
        .filter(|(i, _)| !on_mutual_cycle(*i, funcs))
        .filter(|(_, f)| self_calls_tail_only(f.body, f.name, f.params.len()))
        .map(|(_, f)| f.name)
        .collect()
}

/// Bottom-up kind of an expression over a **typed environment** (parameter and `let`
/// binder kinds), or `None` if anything falls outside the mixed-eligible shape. The
/// env-generalization of [`infer_mixed_kind`] (same operator/builtin/promotion rules,
/// mirrored EXACTLY by [`gen_value_env`]): `+`/`-`/`*` promote `Int` operands to `f64`
/// when the other side is `Float` (the interpreter's numeric promotion); `%`/`//`/
/// bitwise/const-shifts stay `Int`-only under `value_eligible`'s constant constraints;
/// `sqrt` is always `Float`, `abs` preserves, `min`/`max` need same-kind operands. No
/// `let`/`if`/user-calls in VALUE position (tail positions handle `let`/`if`), and
/// crucially NO division — native `fdiv` diverges from the interpreter on /0.
fn infer_typed_env(e: &Expr, env: &HashMap<&str, NumKind>) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Ident { name, .. } => env.get(name.as_str()).copied(),
        Expr::Binary { op, left, right, .. } => {
            let lk = infer_typed_env(left, env)?;
            let rk = infer_typed_env(right, env)?;
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => {
                    Some(if lk == NumKind::Float || rk == NumKind::Float {
                        NumKind::Float
                    } else {
                        NumKind::Int
                    })
                }
                BinOp::Mod | BinOp::FloorDiv => (lk == NumKind::Int
                    && rk == NumKind::Int
                    && matches!(**right, Expr::Int(n) if n > 0))
                .then_some(NumKind::Int),
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    (lk == NumKind::Int && rk == NumKind::Int).then_some(NumKind::Int)
                }
                BinOp::Shl | BinOp::Shr => (lk == NumKind::Int
                    && matches!(**right, Expr::Int(n) if (0..=63).contains(&n)))
                .then_some(NumKind::Int),
                _ => None,
            }
        }
        Expr::Call { name, args, .. } => match (name.as_str(), args.len()) {
            ("sqrt", 1) => {
                infer_typed_env(&args[0], env)?;
                Some(NumKind::Float)
            }
            ("abs", 1) => infer_typed_env(&args[0], env),
            ("min" | "max", 2) => {
                let ka = infer_typed_env(&args[0], env)?;
                let kb = infer_typed_env(&args[1], env)?;
                (ka == kb).then_some(ka)
            }
            _ => None,
        },
        _ => None,
    }
}

/// True iff `e` is a mixed-eligible condition: `and`/`or` over comparisons whose two
/// sides infer to the SAME kind (an `Int`-vs-`Float` comparison is rejected — its
/// promotion semantics past 2^53 are not provably identical to the interpreter's).
/// Mirrored exactly by [`gen_cond_env`].
fn cond_typed_ok(e: &Expr, env: &HashMap<&str, NumKind>) -> bool {
    match e {
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_typed_ok(left, env) && cond_typed_ok(right, env)
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne)
                && match (infer_typed_env(left, env), infer_typed_env(right, env)) {
                    (Some(lk), Some(rk)) => lk == rk,
                    _ => false,
                }
        }
        _ => false,
    }
}

/// The result kind of a mixed tail-recursive body, walking exactly the tail structure
/// [`self_calls_tail_only`] admitted. Returns `None` = ineligible; `Some(None)` = every
/// path re-loops (the body never returns a value); `Some(Some(k))` = all value positions
/// agree on kind `k`. Each tail self-call's argument kinds must EQUAL the annotated
/// parameter kinds — the loop then preserves every parameter's type by induction, which
/// is what makes one static specialization faithful to the dynamically-typed interpreter.
fn mixed_tail_ret_kind<'a>(
    e: &'a Expr,
    env: &mut HashMap<&'a str, NumKind>,
    self_name: &str,
    param_kinds: &[NumKind],
) -> Option<Option<NumKind>> {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => {
            if !cond_typed_ok(cond, env) {
                return None;
            }
            let a = mixed_tail_ret_kind(then_branch, env, self_name, param_kinds)?;
            let b = mixed_tail_ret_kind(else_branch, env, self_name, param_kinds)?;
            match (a, b) {
                (None, x) | (x, None) => Some(x),
                (Some(k1), Some(k2)) if k1 == k2 => Some(Some(k1)),
                _ => None,
            }
        }
        Expr::Let { bindings, body } => {
            let mut saved: Vec<(&'a str, Option<NumKind>)> = Vec::new();
            for (n, v) in bindings {
                let k = infer_typed_env(v, env)?;
                saved.push((n.as_str(), env.insert(n.as_str(), k)));
            }
            let r = mixed_tail_ret_kind(body, env, self_name, param_kinds);
            for (n, old) in saved.into_iter().rev() {
                match old {
                    Some(o) => {
                        env.insert(n, o);
                    }
                    None => {
                        env.remove(n);
                    }
                }
            }
            r
        }
        Expr::Call { name, args, .. } if name == self_name => {
            if args.len() != param_kinds.len() {
                return None;
            }
            for (a, &k) in args.iter().zip(param_kinds) {
                if infer_typed_env(a, env)? != k {
                    return None;
                }
            }
            Some(None)
        }
        other => infer_typed_env(other, env).map(Some),
    }
}

/// The mixed-specialization signature of a tail-loopable function, or `None` if it has
/// no such form: every parameter carries an explicit `Int`/`Float` annotation (the
/// contract that makes one static specialization honest — the VM dispatches it only
/// when the actual argument types match), and the body types consistently under those
/// kinds. An ALL-`Int` signature is admitted too, when the plain i64 path did not
/// already claim the function — that is the "Int state, float intermediates" shape
/// (e.g. an xorshift Monte-Carlo loop: i64 RNG state threaded through the tail calls,
/// f64 math inside each iteration, Int result), which `value_eligible` rejects for its
/// float literals. Returns (float bitmask, per-param kinds, result kind — `None` when
/// every path re-loops).
fn mixed_tail_sig(
    f: &FnDef,
    tail_loop: &HashSet<&str>,
    int_eligible: &HashSet<&str>,
) -> Option<(u16, Vec<NumKind>, Option<NumKind>)> {
    if !tail_loop.contains(f.name) || f.params.is_empty() || f.params.len() > MAX_ARITY {
        return None;
    }
    let mut kinds = Vec::with_capacity(f.params.len());
    let mut mask: u16 = 0;
    for (j, (_, ann)) in f.params.iter().enumerate() {
        match ann {
            Some(TypeAnn::Int) => kinds.push(NumKind::Int),
            Some(TypeAnn::Float) => {
                kinds.push(NumKind::Float);
                mask |= 1 << j;
            }
            _ => return None,
        }
    }
    if mask == 0 && int_eligible.contains(f.name) {
        // The plain i64 loop already covers an all-Int, i64-closed function — a mixed
        // duplicate would never be dispatched (the all-Int arm wins first).
        return None;
    }
    let mut env: HashMap<&str, NumKind> =
        f.params.iter().zip(&kinds).map(|((n, _), &k)| (n.as_str(), k)).collect();
    let ret = mixed_tail_ret_kind(f.body, &mut env, f.name, &kinds)?;
    Some((mask, kinds, ret))
}

/// Pure scalar builtins the `i64` kernel codegen emits inline, matching the interpreter
/// bit-for-bit: `abs` is `wrapping_abs` (Cranelift `iabs`, which wraps `i64::MIN` to
/// itself); `min`/`max` reproduce the interpreter's `as_f64()`-compare-then-return-the-
/// original-operand semantics (so they agree even past 2^53, where a native integer
/// compare would differ). Added to the JIT-eligible set only when no user function of the
/// same name shadows them (then the call dispatches to the user's function instead).
pub const JIT_SCALAR_BUILTINS: &[(&str, usize)] = &[("abs", 1), ("min", 2), ("max", 2)];

/// For a recognized JIT builtin, whether the call arity matches; for any other name (a
/// user function) there is no constraint here — its arity is validated by the front end.
fn jit_builtin_arity_ok(name: &str, nargs: usize) -> bool {
    match JIT_SCALAR_BUILTINS.iter().find(|(n, _)| *n == name) {
        Some((_, ar)) => *ar == nargs,
        None => true,
    }
}

/// Pure float builtins the `f64` kernel codegen emits inline, bit-for-bit with the
/// interpreter: `sqrt` → hardware `fsqrt` (IEEE correctly-rounded, NaN on negatives — the
/// interpreter's `f64::sqrt` doesn't raise); `abs` → `fabs`; `min`/`max` → the
/// interpreter's `as_f64()`-compare (identity for floats) then pick the original operand,
/// so NaN propagates identically. The libm transcendentals (`exp`/`sin`/`tanh`/…) are NOT
/// here: they'd need an external-symbol call whose result must match the host libm exactly.
const JIT_FLOAT_BUILTINS: &[(&str, usize)] = &[("sqrt", 1), ("abs", 1), ("min", 2), ("max", 2)];

/// The arity of a recognized JIT float builtin, or `None` if `name` is not one.
fn jit_float_builtin_arity(name: &str) -> Option<usize> {
    JIT_FLOAT_BUILTINS.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
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

fn value_eligible(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, kind: NumKind) -> bool {
    match e {
        Expr::Int(_) => true,
        // A float literal is only representable in the `f64` specialization.
        Expr::Float(_) => kind == NumKind::Float,
        Expr::Ident { name, .. } => locals.contains(name.as_str()),
        Expr::Binary { op, left, right, .. } => {
            // NOTE: `Div` is intentionally excluded. For `Int`, the interpreter
            // returns a `Float` (`10 / 2 == 5.0`), so `/` is not i64-closed at all;
            // and native `fdiv` yields ±inf where the interpreter errors on /0.
            // Functions using `/` fall back to the VM/interpreter.
            let op_ok = match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => true,
                // `Int % Int` *is* i64-closed (`a.rem_euclid(b)`). We JIT it only
                // when the divisor is a **positive integer constant**: that rules
                // out `%0` (which must raise "modulo by zero") and the negative-
                // divisor sign subtleties, so native `rem_euclid` is total and
                // matches the interpreter exactly. (Float kind is unused today.)
                BinOp::Mod => {
                    kind == NumKind::Int && matches!(**right, Expr::Int(n) if n > 0)
                }
                // Bitwise ops on two Ints are unconditionally i64-closed — the
                // interpreter returns `Int(a & b)` etc. with no overflow, promotion, or
                // trap — so `band`/`bor`/`bxor` match exactly. Int kind only.
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => kind == NumKind::Int,
                // `<<`/`>>` only by a constant in `0..=63`: the interpreter *raises* for
                // an out-of-range shift, while native `ishl`/`sshr` silently mask the
                // count, so only an in-range constant is provably equivalent.
                BinOp::Shl | BinOp::Shr => {
                    kind == NumKind::Int
                        && matches!(**right, Expr::Int(n) if (0..=63).contains(&n))
                }
                // `//` (euclidean floor division) is i64-closed like `%`; JIT only by a
                // positive constant divisor (rules out `//0` and the `sdiv(i64::MIN,-1)`
                // trap), lowered as `sdiv` adjusted down when the remainder is negative.
                BinOp::FloorDiv => {
                    kind == NumKind::Int && matches!(**right, Expr::Int(n) if n > 0)
                }
                _ => false,
            };
            op_ok
                && value_eligible(left, eligible, locals, kind)
                && value_eligible(right, eligible, locals, kind)
        }
        Expr::Call { name, args, .. } => {
            eligible.contains(name.as_str())
                && jit_builtin_arity_ok(name, args.len())
                && args.iter().all(|a| value_eligible(a, eligible, locals, kind))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            cond_eligible(cond, eligible, locals, kind)
                && value_eligible(then_branch, eligible, locals, kind)
                && value_eligible(else_branch, eligible, locals, kind)
        }
        Expr::Let { bindings, body } => {
            let mut locals2 = locals.clone();
            for (n, v) in bindings {
                if !value_eligible(v, eligible, &locals2, kind) {
                    return false;
                }
                locals2.insert(n.as_str());
            }
            value_eligible(body, eligible, &locals2, kind)
        }
        Expr::Match { scrutinee, arms, .. } => match_eligible(scrutinee, arms, eligible, locals, kind),
        _ => false,
    }
}

/// An `i64`-scrutinee `match` the JIT can lower to an if/else chain ([`gen_match`]): the
/// scrutinee and every arm body are `i64`-eligible; each pattern is an `Int` literal, an
/// `Or` of `Int` literals, `_`, or a binder; each guard is an `i64` condition (seeing a
/// binder if the pattern is one); and the **last** arm is an unguarded catch-all (`_`/
/// binder) so the lowering is total — a non-exhaustive `match` (which the interpreter
/// raises on) falls through to the VM instead. `Float`/`Str`/`Bool`/tuple/record patterns
/// are not `i64`-closed and fall through.
fn match_eligible(
    scrutinee: &Expr,
    arms: &[crate::ast::MatchArm],
    eligible: &HashSet<&str>,
    locals: &HashSet<&str>,
    kind: NumKind,
) -> bool {
    use crate::ast::Pattern;
    if kind != NumKind::Int || arms.is_empty() {
        return false;
    }
    if !value_eligible(scrutinee, eligible, locals, kind) {
        return false;
    }
    let last = arms.last().unwrap();
    let last_total =
        last.guard.is_none() && matches!(last.pattern, Pattern::Wildcard | Pattern::Bind(_));
    if !last_total {
        return false;
    }
    arms.iter().all(|arm| {
        let pat_ok = match &arm.pattern {
            Pattern::Int(_) | Pattern::Wildcard | Pattern::Bind(_) => true,
            Pattern::Or(alts) => alts.iter().all(|p| matches!(p, Pattern::Int(_))),
            _ => false,
        };
        if !pat_ok {
            return false;
        }
        // A binder pattern adds its name (the scrutinee) for the guard + body.
        let mut locals2 = locals.clone();
        if let Pattern::Bind(n) = &arm.pattern {
            locals2.insert(n.as_str());
        }
        let guard_ok =
            arm.guard.as_ref().is_none_or(|g| cond_eligible(g, eligible, &locals2, kind));
        guard_ok && value_eligible(&arm.body, eligible, &locals2, kind)
    })
}

fn cond_eligible(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, kind: NumKind) -> bool {
    match e {
        // `and`/`or` are widenable ONLY in condition position (an `if`/filter/guard
        // condition is forced to `Bool`). Each side must itself be a condition, so every
        // leaf is a comparison whose operands are pure and total — a native
        // non-short-circuit `band`/`bor` is then bit-identical to the interpreter's
        // short-circuit `and`/`or` (no operand can be `Missing` or raise). NOT added to
        // `value_eligible`: in *value* position `true or missing` is `Missing`, not i64.
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible(left, eligible, locals, kind)
                && cond_eligible(right, eligible, locals, kind)
        }
        Expr::Binary { op, left, right, .. } => matches!(
            op,
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne
        ) && value_eligible(left, eligible, locals, kind)
            && value_eligible(right, eligible, locals, kind),
        _ => false,
    }
}

// ---------- codegen ----------

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
    // A dividing scalar f64 reduce takes an extra `*mut i8` **poison** out-param: the codegen ORs
    // `divisor == 0` into it (a `/0` where the interpreter raises), and the VM falls back if set.
    // Mutually exclusive with `has_caps` — a caps body (the float dot-product) never divides.
    let needs_poison = float_scalar && !has_caps && reduce_body_divides(rl);
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

    // Load each captured value once (loop-invariant) from the `caps` pointer (4th param).
    let cap_vars: Vec<Variable> = if has_caps {
        let caps_ptr = b.block_params(entry)[3];
        rl.captures
            .iter()
            .enumerate()
            .map(|(j, _)| {
                let v = b.ins().load(I64, MemFlags::trusted(), caps_ptr, (j * 8) as i32);
                let var = b.declare_var(I64);
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
            if cap.kind == CaptureKind::ArrayF64
                && let Some(&cv) = cap_vars.get(j)
            {
                arrays.insert(cap.name.as_str(), cv);
            }
        }
        for body in &rl.bodies {
            new_vals.push(gen_f64_typed(&mut b, body, &binders, &arrays, poison_var).0);
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
    mixed: bool,
) -> Option<()> {
    // Element + capture values are `i64` (map over an `Int` array) or `f64` (map over a
    // `Float` array); the buffer pointers and length are always `i64`. Filter is `Int`.
    // A `mixed` map reads `i64` elements but writes `f64` (Int source, float body) — so the
    // *load* type is `i64` and the result is the `f64` from `gen_value_typed`.
    let elem_ty = if mixed {
        I64
    } else if matches!(elem_kind, NumKind::Float) {
        F64
    } else {
        I64
    };
    ctx.func.signature.call_conv = CallConv::SystemV;
    for _ in 0..3 {
        ctx.func.signature.params.push(AbiParam::new(I64)); // src, dst, len
    }
    if is_filter {
        ctx.func.signature.returns.push(AbiParam::new(I64));
    } else {
        ctx.func.signature.params.push(AbiParam::new(I64)); // map: caps ptr
    }

    let mut b = FunctionBuilder::new(&mut ctx.func, bctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let src = b.block_params(entry)[0];
    let dst = b.block_params(entry)[1];
    let len = b.block_params(entry)[2];
    // map: the caps pointer (loop-invariant captured i64 values), bound below.
    let caps_ptr = if is_filter { None } else { Some(b.block_params(entry)[3]) };

    let i_var = b.declare_var(I64); // read cursor
    let w_var = b.declare_var(I64); // write cursor (filter); == i for map
    let src_var = b.declare_var(I64);
    let dst_var = b.declare_var(I64);
    let len_var = b.declare_var(I64);
    let zero = b.ins().iconst(I64, 0);
    b.def_var(i_var, zero);
    b.def_var(w_var, zero);
    b.def_var(src_var, src);
    b.def_var(dst_var, dst);
    b.def_var(len_var, len);

    // Hoist the loop-invariant capture loads into the entry (pre-loop) block — read each
    // once rather than re-loading it from `caps` on every iteration (mirrors the reduce
    // kernel's entry-block capture loads). Immediate-offset load straight off `caps_ptr`.
    let cap_vars: Vec<Variable> = if let Some(cp) = caps_ptr {
        k.captures
            .iter()
            .enumerate()
            .map(|(j, _)| {
                let v = b.ins().load(elem_ty, MemFlags::trusted(), cp, (j * 8) as i32);
                let cvar = b.declare_var(elem_ty);
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
    for (j, cname) in k.captures.iter().enumerate() {
        vars.insert(cname.as_str(), cap_vars[j]);
    }

    if is_filter {
        // dst[w] = elem; w += (pred ? 1 : 0)
        let wv = b.use_var(w_var);
        let woff = b.ins().imul_imm(wv, 8);
        let dstp = b.use_var(dst_var);
        let daddr = b.ins().iadd(dstp, woff);
        b.ins().store(MemFlags::trusted(), elem, daddr, 0);
        let keep = gen_cond(&mut b, &k.body, &mut vars, fn_ids, module, NumKind::Int);
        let keep64 = b.ins().uextend(I64, keep);
        let wv2 = b.use_var(w_var);
        let nw = b.ins().iadd(wv2, keep64);
        b.def_var(w_var, nw);
    } else {
        // dst[i] = body(elem). `mixed` types the body node-by-node (i64 element → f64
        // result); the plain map uses the monomorphized `elem_kind` codegen.
        let r = if mixed {
            gen_value_typed(&mut b, &k.body, &vars, &k.binder).0
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
    if is_filter {
        let wv = b.use_var(w_var);
        b.ins().return_(&[wv]);
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
                let keep = gen_cond(&mut b, bexpr, &mut vars, fn_ids, module, NumKind::Int);
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
                let new_vals: Vec<ClValue> =
                    bodies.iter().map(|body| gen_f64_typed(&mut b, body, &binders, &no_arrays, None).0).collect();
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
            let cv = gen_cond(b, cond, vars, fn_ids, module, kind);
            b.ins().brif(cv, then_b, &[], else_b, &[]);
            b.switch_to_block(then_b);
            b.seal_block(then_b);
            gen_tail(b, then_branch, vars, fn_ids, module, kind, tl);
            b.switch_to_block(else_b);
            b.seal_block(else_b);
            gen_tail(b, else_branch, vars, fn_ids, module, kind, tl);
        }
        Expr::Let { bindings, body } => {
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
fn gen_value_env<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &HashMap<&'a str, Variable>,
    env: &HashMap<&'a str, NumKind>,
) -> (ClValue, NumKind) {
    match e {
        Expr::Int(i) => (b.ins().iconst(I64, *i), NumKind::Int),
        Expr::Float(f) => (b.ins().f64const(*f), NumKind::Float),
        Expr::Ident { name, .. } => (b.use_var(vars[name.as_str()]), env[name.as_str()]),
        Expr::Binary { op, left, right, .. } => {
            let (lv, lk) = gen_value_env(b, left, vars, env);
            let (rv, rk) = gen_value_env(b, right, vars, env);
            if lk == NumKind::Int && rk == NumKind::Int {
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
                    _ => unreachable!("ineligible operator reached mixed-env codegen"),
                };
                (v, NumKind::Float)
            }
        }
        Expr::Call { name, args, .. } => match name.as_str() {
            "sqrt" => {
                let (av, ak) = gen_value_env(b, &args[0], vars, env);
                let af = if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                (b.ins().sqrt(af), NumKind::Float)
            }
            "abs" => {
                let (av, ak) = gen_value_env(b, &args[0], vars, env);
                match ak {
                    NumKind::Int => (b.ins().iabs(av), NumKind::Int),
                    NumKind::Float => (b.ins().fabs(av), NumKind::Float),
                }
            }
            "min" | "max" => {
                let (av, ak) = gen_value_env(b, &args[0], vars, env);
                let (cv, _ck) = gen_value_env(b, &args[1], vars, env);
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
            _ => unreachable!("ineligible call reached mixed-env codegen"),
        },
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
    tl: &MixedTail,
) -> ClValue {
    match e {
        Expr::Binary { op: BinOp::And, left, right, .. } => {
            let l = gen_cond_env(b, left, vars, env, tl);
            let r = gen_cond_env(b, right, vars, env, tl);
            b.ins().band(l, r)
        }
        Expr::Binary { op: BinOp::Or, left, right, .. } => {
            let l = gen_cond_env(b, left, vars, env, tl);
            let r = gen_cond_env(b, right, vars, env, tl);
            b.ins().bor(l, r)
        }
        Expr::Binary { op, left, right, .. } => {
            let (l, lk) = gen_value_env(b, left, vars, env);
            let (r, _rk) = gen_value_env(b, right, vars, env);
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
    /// Target of the NaN-compare bail; it stores 1 through the poison pointer and
    /// returns (the pointer itself is only needed where the block is FILLED, in
    /// [`build`]'s mixed pass).
    poison_blk: Block,
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
    tl: &MixedTail,
) {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => {
            let then_b = b.create_block();
            let else_b = b.create_block();
            let cv = gen_cond_env(b, cond, vars, env, tl);
            b.ins().brif(cv, then_b, &[], else_b, &[]);
            b.switch_to_block(then_b);
            b.seal_block(then_b);
            gen_tail_mixed(b, then_branch, vars, env, tl);
            b.switch_to_block(else_b);
            b.seal_block(else_b);
            gen_tail_mixed(b, else_branch, vars, env, tl);
        }
        Expr::Let { bindings, body } => {
            let mut saved: Vec<(&'a str, Option<Variable>, Option<NumKind>)> = Vec::new();
            for (n, v) in bindings {
                let (vv, vk) = gen_value_env(b, v, vars, env);
                let var = b.declare_var(vk.cl_type());
                b.def_var(var, vv);
                saved.push((
                    n.as_str(),
                    vars.insert(n.as_str(), var),
                    env.insert(n.as_str(), vk),
                ));
            }
            gen_tail_mixed(b, body, vars, env, tl);
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
                    let (v, ak) = gen_value_env(b, a, vars, env);
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
            let (v, _k) = gen_value_env(b, other, vars, env);
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

            let cv = gen_cond(b, cond, vars, fn_ids, module, kind);
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
        Expr::Let { bindings, body } => {
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
    let g = guard.as_ref().map(|g| gen_cond(b, g, vars, fn_ids, module, NumKind::Int));
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
fn gen_value_typed<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &HashMap<&'a str, Variable>,
    binder: &str,
) -> (ClValue, NumKind) {
    match e {
        Expr::Int(i) => (b.ins().iconst(I64, *i), NumKind::Int),
        Expr::Float(f) => (b.ins().f64const(*f), NumKind::Float),
        Expr::Ident { name, .. } => {
            debug_assert_eq!(name, binder, "only the binder reaches the mixed kernel");
            (b.use_var(vars[name.as_str()]), NumKind::Int)
        }
        Expr::Binary { op, left, right, .. } => {
            let (lv, lk) = gen_value_typed(b, left, vars, binder);
            let (rv, rk) = gen_value_typed(b, right, vars, binder);
            if lk == NumKind::Int && rk == NumKind::Int {
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
                    _ => unreachable!("ineligible operator reached mixed codegen"),
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
                    _ => unreachable!("ineligible operator reached mixed codegen"),
                };
                (v, NumKind::Float)
            }
        }
        // The pure builtins (eligibility guaranteed the names + arities, and same-kind
        // `min`/`max`): `sqrt` promotes its arg to f64 (fsqrt → Float); `abs` is `iabs`
        // (Int) / `fabs` (Float); `min`/`max` compare-then-select-original, on `i64`
        // (via f64 compare, as the interpreter) or `f64`.
        Expr::Call { name, args, .. } => match name.as_str() {
            "sqrt" => {
                let (av, ak) = gen_value_typed(b, &args[0], vars, binder);
                let af = if ak == NumKind::Int { b.ins().fcvt_from_sint(F64, av) } else { av };
                (b.ins().sqrt(af), NumKind::Float)
            }
            "abs" => {
                let (av, ak) = gen_value_typed(b, &args[0], vars, binder);
                match ak {
                    NumKind::Int => (b.ins().iabs(av), NumKind::Int),
                    NumKind::Float => (b.ins().fabs(av), NumKind::Float),
                }
            }
            "min" | "max" => {
                let (av, ak) = gen_value_typed(b, &args[0], vars, binder);
                let (cv, _ck) = gen_value_typed(b, &args[1], vars, binder);
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
            _ => unreachable!("ineligible call reached mixed codegen"),
        },
        _ => unreachable!("ineligible node reached mixed codegen"),
    }
}

fn gen_cond<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
    kind: NumKind,
) -> ClValue {
    match e {
        // `and`/`or` combine two i1 conditions. Handled before the comparison arm
        // because a nested `and`/`or` is itself an `Expr::Binary` and would otherwise
        // fall into the comparison `match op` and hit its `unreachable!`. Non-short-
        // circuit `band`/`bor` is exact here: both operands are pure i64 comparisons, so
        // evaluating the RHS eagerly is observationally identical to short-circuiting.
        Expr::Binary { op: BinOp::And, left, right, .. } => {
            let l = gen_cond(b, left, vars, fn_ids, module, kind);
            let r = gen_cond(b, right, vars, fn_ids, module, kind);
            b.ins().band(l, r)
        }
        Expr::Binary { op: BinOp::Or, left, right, .. } => {
            let l = gen_cond(b, left, vars, fn_ids, module, kind);
            let r = gen_cond(b, right, vars, fn_ids, module, kind);
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
                        BinOp::Eq => FloatCC::Equal,
                        BinOp::Ne => FloatCC::NotEqual,
                        _ => unreachable!("only comparisons reach cond codegen"),
                    };
                    b.ins().fcmp(cc, l, r)
                }
            }
        }
        _ => unreachable!("ineligible condition reached codegen"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> Expr {
        Expr::Call { name: name.to_string(), args: vec![], line: 0, col: 0 }
    }

    // The parser's define-before-use rule means a mutual-recursion cycle can't be
    // *written* in Helix today, so these cases are constructed as raw ASTs. They
    // assert the JIT stays memory-safe independent of that front-end policy: a
    // function on a call cycle must never reach the unguarded native path.
    #[test]
    fn recursive_funcs_catches_mutual_recursion() {
        let p: Vec<(String, Option<TypeAnn>)> = vec![];
        let (fb, gb) = (call("g"), call("f")); // f -> g -> f
        let (leaf, caller) = (Expr::Int(0), call("leaf")); // caller -> leaf (acyclic)
        let funcs = vec![
            FnDef { name: "f", params: &p, body: &fb },
            FnDef { name: "g", params: &p, body: &gb },
            FnDef { name: "leaf", params: &p, body: &leaf },
            FnDef { name: "caller", params: &p, body: &caller },
        ];
        let rec = recursive_funcs(&funcs);
        assert!(rec.contains("f") && rec.contains("g"), "f->g->f cycle must be flagged");
        assert!(!rec.contains("leaf") && !rec.contains("caller"), "acyclic fns are not recursive");
        // ...and eligible_set must keep the cycle off the native path.
        let elig = eligible_set(&funcs, NumKind::Int);
        assert!(!elig.contains("f") && !elig.contains("g"));
    }

    #[test]
    fn recursive_funcs_catches_direct_self_recursion() {
        let p: Vec<(String, Option<TypeAnn>)> = vec![];
        let body = call("fac"); // fac -> fac
        let funcs = vec![FnDef { name: "fac", params: &p, body: &body }];
        assert!(recursive_funcs(&funcs).contains("fac"));
    }

    #[test]
    fn recursive_funcs_allows_acyclic_chain() {
        let p: Vec<(String, Option<TypeAnn>)> = vec![];
        let (ab, bb, cb) = (call("b"), call("c"), Expr::Int(0)); // a -> b -> c (leaf)
        let funcs = vec![
            FnDef { name: "a", params: &p, body: &ab },
            FnDef { name: "b", params: &p, body: &bb },
            FnDef { name: "c", params: &p, body: &cb },
        ];
        assert!(recursive_funcs(&funcs).is_empty(), "an acyclic call chain has no recursion");
    }
}

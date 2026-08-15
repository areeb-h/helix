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

use crate::ast::{BinOp, Expr, Stmt, TypeAnn, UnOp};
use crate::bytecode::{Capture, CaptureKind, IndexBound};

// The JIT's only `unsafe`: the FFI trampolines that call finalized native code. Kept in
// their own file so that boundary is a single auditable unit; re-exported so callers
// still use `crate::jit::call_i64`, etc.
mod ffi;
pub use ffi::*;

const MAX_ARITY: usize = 6;

/// The two scalar specializations. `pub` because [`mixed_fn_sigs`] hands its table to the
/// bytecode compiler, which types calls by these kinds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NumKind {
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
    /// Tail loops compiled with the globals they read appended as trailing `i64`
    /// parameters: name -> (entry point, those globals in parameter order, real arity).
    /// Dispatched only from the VM's `CallFn`, which resolves the names to global slots
    /// once and declines if any of them is not an `Int` at call time.
    cap_fns: HashMap<String, (*const u8, Vec<String>, usize)>,
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
    /// The Int-ROOTED mixed specialization (i64 source, i64 result, Float intermediates).
    /// Same ABI as `map_ptrs` — `fn(*const i64, *mut i64, i64, *const i64)` — so it shares
    /// the i64 kernel's runners and in-place reuse. Same index as `map_ptrs`.
    map_ptrs_mixed_int: Vec<Option<*const u8>>,
    /// The VALUE-SCALAR variant of the plain mixed kernel: captures ride as `f64` BITS
    /// (an `Int` promoted at marshal, a `Float` passed through), dispatched when a runtime
    /// `Float` capture makes the Int-proven marshal decline. Same ABI as `map_ptrs_mixed`.
    map_ptrs_mixed_value: Vec<Option<*const u8>>,
    /// Native `extern "C" fn(*const i64 src, *mut i64 dst, i64 len) -> i64` (kept count)
    /// filter kernels, indexed by [`crate::bytecode::Op::TryJitFilter`]'s `kernel_idx`.
    filter_ptrs: Vec<Option<*const u8>>,
    /// The f64 (`Floats`-source) filter kernels — same index as `filter_ptrs`. The
    /// predicate POISONS (returns -1) when an ordering comparison meets a NaN, because
    /// the interpreter raises there; the dispatch then falls back to the bytecode loop.
    filter_ptrs_f64: Vec<Option<*const u8>>,
    /// Native fused-pipeline kernels (one of three signatures by shape — see
    /// [`define_fused_kernel`]), indexed by [`crate::bytecode::Op::TryJitFused`]'s
    /// `kernel_idx`.
    fused_ptrs: Vec<Option<*const u8>>,
    /// Native `extern "C" fn(start, end, init, *mut i64 dst, *const i64 caps)` scan
    /// (prefix-fold) kernels, indexed by [`crate::bytecode::Op::TryJitScan`]'s `loop_idx`.
    scan_ptrs: Vec<Option<*const u8>>,
}

impl Jit {
    pub fn lookup(&self, name: &str) -> Option<NativeFn> {
        self.by_name.get(name).copied()
    }
    /// The capture-taking tail-loop entry point for `name`, with the globals it expects
    /// appended after its real parameters.
    pub fn capture_loop(&self, name: &str) -> Option<(*const u8, &[String], usize)> {
        self.cap_fns.get(name).map(|(p, c, a)| (*p, c.as_slice(), *a))
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
    /// The native Int-ROOTED mixed map kernel (Int source → Int result through Float
    /// intermediates) for site `idx`.
    pub fn map_kernel_mixed_int(&self, idx: usize) -> Option<*const u8> {
        self.map_ptrs_mixed_int.get(idx).copied().flatten()
    }
    /// The value-scalar variant of the mixed map kernel for site `idx`.
    pub fn map_kernel_mixed_value(&self, idx: usize) -> Option<*const u8> {
        self.map_ptrs_mixed_value.get(idx).copied().flatten()
    }
    /// The native filter kernel for site `idx`, if one compiled.
    pub fn filter_kernel(&self, idx: usize) -> Option<*const u8> {
        self.filter_ptrs.get(idx).copied().flatten()
    }
    /// The native f64 (`Floats`-source) filter kernel for site `idx`, if one compiled.
    pub fn filter_kernel_f64(&self, idx: usize) -> Option<*const u8> {
        self.filter_ptrs_f64.get(idx).copied().flatten()
    }
    /// The native fused-pipeline kernel for site `idx`, if one compiled.
    pub fn fused_kernel(&self, idx: usize) -> Option<*const u8> {
        self.fused_ptrs.get(idx).copied().flatten()
    }
    /// The native scan (prefix-fold) kernel for site `idx`, if one compiled.
    pub fn scan_loop(&self, idx: usize) -> Option<*const u8> {
        self.scan_ptrs.get(idx).copied().flatten()
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
/// What an indexed collector returns: the ordered captures, the bounds obligations the VM
/// must discharge, and any synthetic `$aff` base/coef terms (expressions the compile site
/// evaluates once in the enclosing scope — a site that pushes bare idents only must
/// decline when this is non-empty).
pub type IndexedCaptures = (Vec<Capture>, Vec<IndexBound>, Vec<(String, Expr)>);

pub fn reduce_loop_captures(
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
) -> Option<IndexedCaptures> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(pa);
    locals.insert(pb);
    let mut caps: Vec<Capture> = Vec::new();
    let mut bounds: Vec<IndexBound> = Vec::new();
    // Synthetic `$aff` base/coef terms from affine indices (`a[2*i]`) — expressions the
    // compile site evaluates once in the enclosing scope. A site whose capture-push loop
    // cannot evaluate an expression (it pushes bare idents only) must DECLINE when this
    // is non-empty rather than push an unresolvable name.
    let mut synth: Vec<(String, Expr)> = Vec::new();
    if value_eligible_cap_indexed(body, fns, &locals, pb, &mut caps, &mut bounds, &mut synth)
        && caps.len() <= MAX_CAPTURES
    {
        Some((caps, bounds, synth))
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

/// Relabel every `Scalar` cap that is used only as a VALUE (never as an index) to
/// [`CaptureKind::ScalarValue`] — a purely-VALUE scalar. Both map indexed analyses call this
/// before returning, so the i64 and mixed derivations of the same body produce byte-identical
/// capture lists and the dual-spec re-gate matches. A scalar that IS index-referenced stays
/// `Scalar`: an index is an integer, so it is `i64` in both specs even when the same name also
/// appears in a value position (`n` in both `a[i*n+k]` and `n * x[i]` is necessarily `Int`, so
/// `i64` is correct there too). Reduce captures never pass through here, so the reduce path
/// keeps `Scalar` and is untouched. Idempotent.
///
/// A scalar is index-referenced when a bound names it DIRECTLY (a `Scalar` index, or an
/// `Affine` `base`/`coef` that is a bare ident), OR when it appears inside a COMPOUND affine
/// term — `a` and `b` in `x[i + a + b]`, folded into a synthetic `$aff` slot. The affine
/// codegen recomputes the whole index (`i + a + b`) from the individual `a`/`b` caps, so those
/// too are index arithmetic and must stay `i64`; missing them let the mixed kernel type the
/// index in `f64` and emit ill-typed IR (the Cranelift verifier caught it and the kernel
/// silently declined — a perf cliff, not a divergence). So `synth`'s expressions are scanned
/// for cap names as well.
fn relabel_value_scalars(caps: &mut [Capture], bounds: &[IndexBound], synth: &[(String, Expr)]) {
    let mut index_ref = vec![false; caps.len()];
    for b in bounds {
        match *b {
            IndexBound::Scalar { scalar, .. } => index_ref[scalar as usize] = true,
            IndexBound::Affine { base, coef, .. } => {
                index_ref[base as usize] = true;
                index_ref[coef as usize] = true;
            }
            IndexBound::Counter { .. } => {}
        }
    }
    // A cap named inside any synthetic affine term (`$aff0 = a + b`) is part of the index.
    for i in 0..caps.len() {
        if caps[i].kind == CaptureKind::Scalar
            && synth.iter().any(|(_, e)| expr_uses_ident(e, &caps[i].name))
        {
            index_ref[i] = true;
        }
    }
    for (i, c) in caps.iter_mut().enumerate() {
        if c.kind == CaptureKind::Scalar && !index_ref[i] {
            c.kind = CaptureKind::ScalarValue;
        }
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
    synth: &mut Vec<(String, Expr)>,
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
            // `arr[AFFINE(pb)]`: any other index affine in the counter — `a[2*i]`,
            // `a[i + 1]`, `a[i*n + k]`. The same admission, by the same helpers, as the
            // mixed map's arm (see `infer_f64_indexed`): validate the WHOLE index first
            // as a pure `i64` expression over the counter, free scalars and `Int`
            // literals (codegen lowers exactly that expression from `vars`, so it must
            // be checked verbatim; every leaf effect-free and non-trapping, which is
            // what licenses `affine_split`'s algebraic folding), then split it into
            // counter-free `base`/`coef` terms that land as Scalar cap slots — bare
            // idents reuse the body's own caps, compound terms get a synthetic `$aff`
            // slot the compile site evaluates once. The VM discharges the bound from
            // the two ENDPOINT indices in i128 — over the range endpoints for a reduce
            // (whose `pb` IS the counter), and composed with the lazy range's
            // `start/step` for a map (`map_index_caps`), which declines any other
            // source. There is no `pa` here — the empty string, never a legal ident,
            // fills `index_scalars_eligible`'s reject slot; the REAL accumulator (and
            // every other local) is refused by the `locals` scan below, because a
            // loop-varying name in the index would make the once-evaluated base/coef
            // caps stale, and a `let`-local does not even exist in the enclosing scope
            // the compile site evaluates them in.
            (Expr::Ident { name: arr, .. }, idx) if !locals.contains(arr.as_str()) => {
                if locals.iter().any(|l| *l != pb && expr_uses_ident(idx, l)) {
                    return false;
                }
                let Some(ap) = record_cap_pos(caps, arr, CaptureKind::ArrayI64) else {
                    return false;
                };
                if index_scalars_eligible(idx, "", pb, caps).is_none() {
                    return false;
                }
                let Some((base, coef)) = affine_split(idx, pb) else {
                    return false;
                };
                let (Some(bp), Some(cp)) = (
                    record_index_term(caps, synth, base),
                    record_index_term(caps, synth, coef),
                ) else {
                    return false;
                };
                push_bound(
                    bounds,
                    IndexBound::Affine { array: ap as u32, base: bp as u32, coef: cp as u32 },
                );
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
                && value_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds, synth)
                && value_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds, synth)
        }
        Expr::Call { name, args, .. } => {
            eligible.contains(name.as_str())
                && jit_builtin_arity_ok(name, args.len())
                && args
                    .iter()
                    .all(|a| value_eligible_cap_indexed(a, eligible, locals, pb, caps, bounds, synth))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            cond_eligible_cap_indexed(cond, eligible, locals, pb, caps, bounds, synth)
                && value_eligible_cap_indexed(then_branch, eligible, locals, pb, caps, bounds, synth)
                && value_eligible_cap_indexed(else_branch, eligible, locals, pb, caps, bounds, synth)
        }
        Expr::Let { bindings, body } => {
            let mut locals2 = locals.clone();
            for (n, v) in bindings {
                // A `let` that REBINDS the loop counter `pb` breaks the invariant the `Index`
                // arm relies on: `arr[pb]` no longer means `arr[counter]` — codegen would emit
                // an UNCHECKED load at the let-bound index, past what the VM's counter-range
                // pre-check validated → an out-of-bounds native read. It also can't shadow a
                // captured scalar index without changing what a `Scalar` bound refers to —
                // nor a name an `Affine` bound's `base`/`coef` slot refers to, for the same
                // reason: the bound was proved against the ENCLOSING-scope value, and codegen
                // would recompute the index from the let-bound one. (`$aff` slots cannot
                // collide — `$` is not a legal identifier character.) Refuse to JIT any such
                // `let`; the VM/tree-walker evaluate such a body correctly.
                if n.as_str() == pb
                    || bounds.iter().any(|b| {
                        let names_cap = |pos: u32| {
                            caps.get(pos as usize).is_some_and(|c| c.name == *n)
                        };
                        match b {
                            IndexBound::Scalar { scalar, .. } => names_cap(*scalar),
                            IndexBound::Affine { base, coef, .. } => {
                                names_cap(*base) || names_cap(*coef)
                            }
                            IndexBound::Counter { .. } => false,
                        }
                    })
                {
                    return false;
                }
                if !value_eligible_cap_indexed(v, eligible, &locals2, pb, caps, bounds, synth) {
                    return false;
                }
                locals2.insert(n.as_str());
            }
            value_eligible_cap_indexed(body, eligible, &locals2, pb, caps, bounds, synth)
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
    synth: &mut Vec<(String, Expr)>,
) -> bool {
    match e {
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds, synth)
                && cond_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds, synth)
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne)
                && value_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds, synth)
                && value_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds, synth)
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
fn infer_reduce_f64_kind(
    e: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        // A USER function call — ONE arm for both specializations, exactly as
        // `infer_mixed_kind` does for the map. Splitting it into an `fns`-guarded arm and an
        // `msigs`-guarded arm below is what cost 66x there: `fns` means "i64-closed BODY", not
        // "Int parameters", so a callee can be in BOTH sets, the i64 arm claims the call site
        // by name, and Rust match arms cannot fall through to the mixed one. Merged here from
        // the start rather than repeating that.
        //
        // All-`Int` arguments to an i64-closed callee take the i64 path — that is the contract
        // its specialization was compiled under, so the result is an `i64` and types `Int`, and
        // the body promotes it at the first float precisely where the interpreter does.
        // Otherwise the MIXED specialization applies, and only when the argument kinds EQUAL
        // its parameter kinds: the callee was compiled for exactly those, and there is no
        // promoting at the boundary.
        //
        // The mixed callee's ABI carries a poison pointer, so a `/0` or NaN compare inside it
        // bails the whole reduce. `body_raises` already counts a mixed call for exactly this
        // reason, and `ReduceLoop::raises` carries that answer to both the kernel builder and
        // the VM — which is what makes admitting this arm safe rather than a way to swallow
        // the callee's error.
        Expr::Call { name, args, .. } if user_fns.contains(name.as_str()) => {
            // Typed exactly once, into a `Vec`, before anything is decided.
            let mut kinds = Vec::with_capacity(args.len());
            for a in args {
                kinds.push(infer_reduce_f64_kind(a, pa, pb, fns, user_fns, msigs)?);
            }
            if kinds.iter().all(|k| *k == NumKind::Int) && fns.contains(name.as_str()) {
                if !jit_builtin_arity_ok(name, args.len()) {
                    return None;
                }
                return Some(NumKind::Int);
            }
            let (params, ret) = msigs.get(name.as_str())?;
            if kinds.len() != params.len() || kinds.iter().zip(params).any(|(k, w)| k != w) {
                return None;
            }
            Some(*ret)
        }
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
            let lk = infer_reduce_f64_kind(left, pa, pb, fns, user_fns, msigs)?;
            let rk = infer_reduce_f64_kind(right, pa, pb, fns, user_fns, msigs)?;
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
            infer_reduce_f64_kind(left, pa, pb, fns, user_fns, msigs)?;
            infer_reduce_f64_kind(right, pa, pb, fns, user_fns, msigs)?;
            Some(NumKind::Float)
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_reduce_f64_kind(&args[0], pa, pb, fns, user_fns, msigs)?;
                    Some(NumKind::Float)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_reduce_f64_kind(&args[0], pa, pb, fns, user_fns, msigs)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_reduce_f64_kind(&args[0], pa, pb, fns, user_fns, msigs),
                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                ("to_int" | "sign", 1) => {
                    infer_reduce_f64_kind(&args[0], pa, pb, fns, user_fns, msigs)?;
                    Some(NumKind::Int)
                }
                ("min" | "max", 2) => {
                    let ka = infer_reduce_f64_kind(&args[0], pa, pb, fns, user_fns, msigs)?;
                    let kb = infer_reduce_f64_kind(&args[1], pa, pb, fns, user_fns, msigs)?;
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
pub fn reduce_jit_f64_range_body(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<Expr> {
    if matches!(init, Expr::Float(_)) && f64_range_body_eligible(body, pa, pb, fns, user_fns, msigs) {
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
fn f64_range_body_eligible(
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> bool {
    infer_reduce_f64_kind(body, pa, pb, fns, user_fns, msigs) == Some(NumKind::Float)
}

// `expr_has_div` / `reduce_body_divides` lived here. Both are gone: the poison decision is now
// `ReduceLoop::raises`, set once at compile time by `body_raises` (the predicate the map side
// already used) and READ by both the kernel builder and the VM. `expr_has_div` could not have
// been widened in place anyway — its `Call` arm recursed into a call's ARGUMENTS but never into
// the callee's BODY, so `fn f(x) = 1.0 / x` used as `acc + f(i)` reported no division at all.

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

/// Bottom-up [`MixT`] of a **scalar f64 reduce body that indexes captured `f64` arrays by the
/// loop counter** (the float dot-product / weighted-sum / SAXPY-sum case): `pa` is the `f64`
/// accumulator, `pb` the `i64` counter, and `arr[index]` for a free array `arr` is an `f64`
/// element → records `arr` as a [`CaptureKind::ArrayF64`] capture (first-appearance order).
///
/// A bare free var is a VALUE SCALAR — the coefficient `c` in `s + c * a[i]`. It rides as `f64`
/// in this kernel (which is monomorphically `f64`: a `Float` init picked it), so unlike the map
/// case there is no representation routing to do. What DOES carry over is the bit-identity rule:
/// the codegen evaluates integer subexpressions in `i64` and promotes at the first float, exactly
/// like the interpreter, so a value scalar — `f64` in the kernel but possibly `Int` at runtime —
/// is admitted ONLY where a genuine float ([`MixT::GFloat`]: the accumulator, an array load, a
/// float literal) promotes it. `c * a[i]` is safe; `c * pb` or `c + d` would be `i64` in the
/// interpreter and `f64` here, diverging past 2^53, so they are rejected. See [`MixT`].
///
/// The VM pre-checks each array's bounds before the kernel does raw `f64` loads. `None` outside
/// the eligible shape.
/// The three parallel OUTPUTS of an indexed analysis. They are always constructed together,
/// passed together, and consumed together — bundling them keeps the walker's signature at a
/// readable width now that it also needs the eligible-function set.
#[derive(Default)]
struct IndexedOut {
    caps: Vec<Capture>,
    synth: Vec<(String, Expr)>,
    bounds: Vec<IndexBound>,
}

fn infer_f64_indexed(
    e: &Expr,
    pa: &str,
    pb: &str,
    out: &mut IndexedOut,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
) -> Option<MixT> {
    match e {
        Expr::Int(_) => Some(MixT::Int),
        // A USER function with an `i64` specialization, typed exactly as the mixed map's twin
        // arm does (Stage 3p): `int_eligible` means "i64-closed for all-`Int` arguments", so
        // such a call takes `Int` args and returns `Int`, and `mix_combine` then promotes the
        // result at the first genuine float precisely where the interpreter does. Tried BEFORE
        // the builtin arm so a user function shadowing `abs`/`min`/`max` dispatches to the
        // user's function. An `SFloat` argument is refused for the same reason `abs` refuses
        // one — its runtime type is not pinned, and the callee would read it directly.
        Expr::Call { name, args, .. }
            if fns.contains(name.as_str()) && user_fns.contains(name.as_str()) =>
        {
            if !jit_builtin_arity_ok(name, args.len()) {
                return None;
            }
            for a in args {
                if infer_f64_indexed(a, pa, pb, out, fns, user_fns)? != MixT::Int {
                    return None;
                }
            }
            Some(MixT::Int)
        }
        Expr::Float(_) => Some(MixT::GFloat),
        Expr::Ident { name, .. } => {
            if name == pa {
                Some(MixT::GFloat) // the f64 accumulator register — a genuine float
            } else if name == pb {
                Some(MixT::Int)
            } else {
                // A free VALUE scalar, loaded `f64` by the kernel. Recorded `Scalar` here and
                // relabeled to `ScalarValue` by the caller once the bounds show it is not an
                // index (an index scalar must stay `i64`).
                record_cap(&mut out.caps, name, CaptureKind::Scalar).then_some(MixT::SFloat)
            }
        }
        // A free `f64` array read at an index that is AFFINE in the counter → an `f64` element.
        // `arr[pb]` (v1b) keeps its cheap `Counter` bound; any other affine index
        // (`a[i*n+k]`, `b[k*n+j]`, `a[k+1]`) records an `Affine` bound instead.
        Expr::Index { recv, index, .. } => {
            let arr = match &**recv {
                Expr::Ident { name, .. } if name != pa && name != pb => name,
                _ => return None,
            };
            let ap = record_cap_pos(&mut out.caps, arr, CaptureKind::ArrayF64)?;
            match &**index {
                // The bare counter: exactly v1b's shape, exactly v1b's obligation.
                Expr::Ident { name: idx, .. } if idx == pb => {
                    push_bound(&mut out.bounds, IndexBound::Counter { array: ap as u32 });
                }
                _ => {
                    // Validate the WHOLE index first — a pure `i64` expression over the counter,
                    // free scalars (recorded as caps) and `Int` literals. This is what codegen
                    // lowers verbatim, so it must be checked verbatim; it also makes every leaf
                    // effect-free and non-trapping, which is what licenses `affine_split`'s
                    // algebraic folding to DISCARD subterms (`0 * x → 0`) without losing a raise.
                    index_scalars_eligible(index, pa, pb, &mut out.caps)?;
                    let (base, coef) = affine_split(index, pb)?;
                    let bp = record_index_term(&mut out.caps, &mut out.synth, base)?;
                    let cp = record_index_term(&mut out.caps, &mut out.synth, coef)?;
                    push_bound(
                        &mut out.bounds,
                        IndexBound::Affine { array: ap as u32, base: bp as u32, coef: cp as u32 },
                    );
                }
            }
            Some(MixT::GFloat) // an f64 array load is a genuine float
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_f64_indexed(left, pa, pb, out, fns, user_fns)?;
            let rk = infer_f64_indexed(right, pa, pb, out, fns, user_fns)?;
            mix_combine(lk, rk)
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                // `sqrt` promotes its argument in BOTH engines → an `SFloat` arg is safe.
                ("sqrt", 1) => {
                    infer_f64_indexed(&args[0], pa, pb, out, fns, user_fns)?;
                    Some(MixT::GFloat)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_f64_indexed(&args[0], pa, pb, out, fns, user_fns)?;
                    Some(MixT::GFloat)
                }
                // `abs`/`min`/`max` do NOT promote (interp `abs(Int)` is `iabs`), so an `SFloat`
                // argument would diverge; admit only genuine floats or ints, preserving the kind.
                                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                // An unpromoted value scalar is refused for the same reason `abs` refuses one:
                // its runtime type is not yet pinned, and `to_int`/`sign` read it directly.
                ("to_int" | "sign", 1) => match infer_f64_indexed(&args[0], pa, pb, out, fns, user_fns)? {
                    MixT::SFloat => None,
                    _ => Some(MixT::Int),
                },
("abs", 1) => match infer_f64_indexed(&args[0], pa, pb, out, fns, user_fns)? {
                    MixT::SFloat => None,
                    k => Some(k),
                },
                ("min" | "max", 2) => {
                    let ka = infer_f64_indexed(&args[0], pa, pb, out, fns, user_fns)?;
                    let kb = infer_f64_indexed(&args[1], pa, pb, out, fns, user_fns)?;
                    if ka == kb && ka != MixT::SFloat { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Split an index expression into `(base, coef)` with `index ≡ base + coef*pb` and both parts
/// FREE of the counter `pb` — the algebraic core of [`IndexBound::Affine`]. Only shapes whose
/// linearity is provable by construction are admitted: a counter-free subtree is a pure base;
/// the counter itself is `0 + 1*pb`; `+`/`-` combine componentwise; `*` is linear only when at
/// least one side is counter-free (`k*n` is affine, `k*k` is NOT — quadratic, so `None`).
/// Everything else (`%`, `/`, calls, `if`, indexes) → `None`. Distributing a counter-free factor
/// over both components is exact under wrapping i64: `c*(b + a*pb) = c*b + (c*a)*pb` holds mod
/// 2^64 because multiplication distributes over addition in the ring Z/2^64.
fn affine_split(e: &Expr, pb: &str) -> Option<(Expr, Expr)> {
    fn zero() -> Expr {
        Expr::Int(0)
    }
    fn one() -> Expr {
        Expr::Int(1)
    }
    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary { op, left: Box::new(l), right: Box::new(r), line: 0, col: 0 }
    }
    fn lit(e: &Expr, v: i64) -> bool {
        matches!(e, Expr::Int(k) if *k == v)
    }
    // Identity/constant folding, in the ring Z/2^64 that Helix's `Int` arithmetic already is —
    // so these rewrites are exact, not approximations. This is not cosmetic: splitting `k*n+j`
    // yields `0*n+j` and `1*n+0`, and only folding turns those back into the bare `j` and `n`
    // that [`record_index_term`] can map onto the caps the body ALREADY holds. Without it every
    // affine index mints two fresh synthetic caps and a two-array body blows MAX_CAPTURES.
    // Discarding a factor under `0 * x` is safe because every leaf here is an ident or literal
    // (see the caller's `index_scalars_eligible` pre-check) — nothing to trap or observe.
    fn mk_add(l: Expr, r: Expr) -> Expr {
        match (&l, &r) {
            (Expr::Int(a), Expr::Int(b)) => Expr::Int(a.wrapping_add(*b)),
            _ if lit(&l, 0) => r,
            _ if lit(&r, 0) => l,
            _ => bin(BinOp::Add, l, r),
        }
    }
    fn mk_sub(l: Expr, r: Expr) -> Expr {
        match (&l, &r) {
            (Expr::Int(a), Expr::Int(b)) => Expr::Int(a.wrapping_sub(*b)),
            // `0 - x` is NEGATION, not `x` — only the right identity folds.
            _ if lit(&r, 0) => l,
            _ => bin(BinOp::Sub, l, r),
        }
    }
    fn mk_mul(l: Expr, r: Expr) -> Expr {
        match (&l, &r) {
            (Expr::Int(a), Expr::Int(b)) => Expr::Int(a.wrapping_mul(*b)),
            _ if lit(&l, 0) || lit(&r, 0) => Expr::Int(0),
            _ if lit(&l, 1) => r,
            _ if lit(&r, 1) => l,
            _ => bin(BinOp::Mul, l, r),
        }
    }
    if !expr_uses_ident(e, pb) {
        // Counter-free ⇒ a pure base. (`expr_uses_ident` is conservative: an unrecognised node
        // shape reports "uses", so it can never mis-classify an unknown node as invariant.)
        return Some((e.clone(), zero()));
    }
    match e {
        Expr::Ident { name, .. } if name == pb => Some((zero(), one())),
        Expr::Binary { op: BinOp::Add, left, right, .. } => {
            let (lb, lc) = affine_split(left, pb)?;
            let (rb, rc) = affine_split(right, pb)?;
            Some((mk_add(lb, rb), mk_add(lc, rc)))
        }
        Expr::Binary { op: BinOp::Sub, left, right, .. } => {
            let (lb, lc) = affine_split(left, pb)?;
            let (rb, rc) = affine_split(right, pb)?;
            Some((mk_sub(lb, rb), mk_sub(lc, rc)))
        }
        Expr::Binary { op: BinOp::Mul, left, right, .. } => {
            let l_free = !expr_uses_ident(left, pb);
            let r_free = !expr_uses_ident(right, pb);
            if l_free {
                let (rb, rc) = affine_split(right, pb)?;
                Some((mk_mul((**left).clone(), rb), mk_mul((**left).clone(), rc)))
            } else if r_free {
                let (lb, lc) = affine_split(left, pb)?;
                Some((mk_mul(lb, (**right).clone()), mk_mul(lc, (**right).clone())))
            } else {
                None // both sides vary with the counter → non-linear
            }
        }
        _ => None,
    }
}

/// Validate an index expression as a pure `i64` expression over the counter `pb`, free scalars,
/// and `Int` literals — recording each free scalar as a [`CaptureKind::Scalar`] cap, since codegen
/// lowers this very expression and needs every name it mentions bound. The accumulator `pa` is
/// `f64`, so an index reading it is rejected, as is any `Float` literal or operator outside
/// `+ - *` (the VM marshals a `Scalar` cap only from a `Value::Int`, so a non-integer capture
/// falls back at dispatch anyway).
fn index_scalars_eligible(e: &Expr, pa: &str, pb: &str, caps: &mut Vec<Capture>) -> Option<()> {
    match e {
        Expr::Int(_) => Some(()),
        Expr::Ident { name, .. } => {
            if name == pa {
                None
            } else if name == pb {
                Some(()) // the counter is a binder, not a capture
            } else {
                record_cap(caps, name, CaptureKind::Scalar).then_some(())
            }
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            index_scalars_eligible(left, pa, pb, caps)?;
            index_scalars_eligible(right, pa, pb, caps)
        }
        _ => None,
    }
}

/// Give an affine `base`/`coef` term a cap slot holding its VALUE, so the VM can range-check the
/// index arithmetically without interpreting an AST. A bare free ident already has one (reuse it
/// — `b[k*n+j]`'s base `j` and coef `n` are just the scalar caps the body already captured); any
/// compound term (`i*n`) gets a synthetic `$aff{k}` cap whose expression the compiler evaluates
/// once, in the enclosing scope, before dispatch. Synthetic terms are deduped by their printed
/// form, so the naming is a deterministic function of the body alone — which is what lets the
/// build re-gate re-derive an identical capture list from the same body.
fn record_index_term(
    caps: &mut Vec<Capture>,
    synth: &mut Vec<(String, Expr)>,
    term: Expr,
) -> Option<usize> {
    if let Expr::Ident { name, .. } = &term {
        return record_cap_pos(caps, name, CaptureKind::Scalar);
    }
    let key = format!("{term:?}");
    if let Some((name, _)) = synth.iter().find(|(_, e)| format!("{e:?}") == key) {
        let name = name.clone();
        return record_cap_pos(caps, &name, CaptureKind::Scalar);
    }
    let name = format!("$aff{}", synth.len());
    synth.push((name.clone(), term));
    record_cap_pos(caps, &name, CaptureKind::Scalar)
}

/// Decide whether `range(..).reduce(0.0, (pa, pb) => body)` can JIT as a **scalar `f64` fold
/// that indexes captured `f64` arrays by the counter** — the float dot-product. A `Float`-
/// literal init, a body whose root infers `Float`, and **at least one** `ArrayF64` capture
/// (so this never competes with the capture-free [`reduce_jit_f64_range_body`]). Returns the
/// body + the ordered captures, or `None`. (The VM confirms each capture is a `Floats` array
/// and pre-checks its bounds at dispatch, falling back otherwise.)
pub struct F64RangeCaptures {
    /// The body to lower.
    pub body: Expr,
    /// The ordered captures the VM marshals into the kernel's argument block.
    pub caps: Vec<Capture>,
    /// Index bounds the VM pre-checks at dispatch (empty on the f64 path, which
    /// range-checks its array caps inline).
    pub bounds: Vec<IndexBound>,
    /// Synthetic `$aff{k}` terms the compiler evaluates once in the enclosing scope.
    pub synth: Vec<(String, Expr)>,
}

pub fn reduce_jit_f64_range_captures(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
) -> Option<F64RangeCaptures> {
    if !matches!(init, Expr::Float(_)) {
        return None;
    }
    let mut out = IndexedOut::default();
    if infer_f64_indexed(body, pa, pb, &mut out, fns, user_fns) == Some(MixT::GFloat)
        // Non-empty, NOT "contains an array". Requiring an array capture kept this path from
        // competing with the capture-free `reduce_jit_f64_range_body` — but it also meant a body
        // whose only capture is a SCALAR matched neither: `s + to_float(i) * c` fell to the
        // bytecode loop while `s + to_float(i) * 0.5` ran natively, 0.78s against 0.01s over 10M
        // elements. An empty list still falls through to the capture-free path exactly as before,
        // so this only admits shapes that previously had no kernel at all.
        && !out.caps.is_empty()
        && out.caps.len() <= MAX_CAPTURES
    {
        // Value scalars (`c` in `s + c*a[i]`) become `ScalarValue`, loaded `f64` by the kernel;
        // INDEX scalars (an `a[k]` index, an affine `base`/`coef`, incl. names inside a
        // synthetic `$aff` term) stay `Scalar` — `i64`, since an index is an integer.
        relabel_value_scalars(&mut out.caps, &out.bounds, &out.synth);
        Some(F64RangeCaptures {
            body: body.clone(),
            caps: out.caps,
            bounds: out.bounds,
            synth: out.synth,
        })
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
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_f64_typed(&args[0], binders, user_fns)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_f64_typed(&args[0], binders, user_fns),
                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                ("to_int" | "sign", 1) => {
                    infer_f64_typed(&args[0], binders, user_fns)?;
                    Some(NumKind::Int)
                }
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
/// Everything [`gen_f64_typed`] threads through its recursion unchanged. Bundled for the
/// same reason [`TypedCtx`] is: the walker calls itself thirteen times, and repeating six
/// identical arguments at each one buries the single argument that actually differs.
struct F64Ctx<'c> {
    /// The kernel's binders — the accumulator, the i64 counter, and any scalar captures.
    binders: &'c HashMap<&'c str, (Variable, NumKind)>,
    /// `ArrayF64` capture bases, for `arr[counter]` in the dot-product kernel.
    arrays: &'c HashMap<&'c str, Variable>,
    /// The kernel's `i8` poison accumulator, when it carries one.
    poison: Option<Variable>,
    /// Monomorphized `i64` user functions this kernel may call directly.
    fn_ids: &'c HashMap<&'c str, FuncId>,
    module: &'c mut JITModule,
    /// Present only when the kernel carries poison — see the call arm.
    mixed: Option<&'c MixedCallCtx<'c>>,
}

fn gen_f64_typed(
    b: &mut FunctionBuilder,
    e: &Expr,
    cx: &mut F64Ctx,
) -> (ClValue, NumKind) {
    match e {
        Expr::Int(i) => (b.ins().iconst(I64, *i), NumKind::Int),
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

/// Replace every occurrence of the identifier `name` in `e` with `repl` — the substitution
/// behind map→reduce fusion (`g_body[pb := f_body[fb := $counter]]`, the classical
/// `map(f).reduce(init,g) ≡ reduce(init, (acc,i) => g(acc, f(i)))` identity).
///
/// DELIBERATELY CONSERVATIVE: only the pure-arithmetic node set that the f64 indexed reduce can
/// actually lower is handled, and **anything else returns `None`** so the caller declines the
/// whole fusion instead of emitting a body whose meaning it has not reasoned about. That rules
/// out every binding form (`let`, lambda, `match`) by construction, so there is no shadowing
/// case to get subtly wrong: a rebound `name` inside the substituted region cannot occur because
/// no construct that could rebind it is admitted here. `repl` is substituted structurally, so a
/// `name` occurring more than once duplicates it — safe, because every admitted node is pure and
/// deterministic (the same reason the reduce kernel may re-evaluate an index expression).
pub fn subst_ident(e: &Expr, name: &str, repl: &Expr) -> Option<Expr> {
    Some(match e {
        Expr::Int(_) | Expr::Float(_) => e.clone(),
        Expr::Ident { name: n, .. } => {
            if n == name {
                repl.clone()
            } else {
                e.clone()
            }
        }
        Expr::Unary { op, expr, line, col } => Expr::Unary {
            op: op.clone(),
            expr: Box::new(subst_ident(expr, name, repl)?),
            line: *line,
            col: *col,
        },
        Expr::Binary { op, left, right, line, col } => Expr::Binary {
            op: op.clone(),
            left: Box::new(subst_ident(left, name, repl)?),
            right: Box::new(subst_ident(right, name, repl)?),
            line: *line,
            col: *col,
        },
        Expr::Index { recv, index, line, col } => Expr::Index {
            recv: Box::new(subst_ident(recv, name, repl)?),
            index: Box::new(subst_ident(index, name, repl)?),
            line: *line,
            col: *col,
        },
        Expr::Call { name: f, args, line, col } => Expr::Call {
            name: f.clone(),
            args: args.iter().map(|a| subst_ident(a, name, repl)).collect::<Option<Vec<_>>>()?,
            line: *line,
            col: *col,
        },
        // Every other node — binding forms, strings, records, method calls, … — declines.
        _ => return None,
    })
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

fn reduce_bodies_eligible(
    rl: &crate::bytecode::ReduceLoop,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> bool {
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
            let mut out = IndexedOut::default();
            let root = infer_f64_indexed(&rl.bodies[0], &rl.pa, &rl.pb, &mut out, fns, user_fns);
            // Mirror the compile gate exactly, INCLUDING the value-scalar relabel — otherwise the
            // re-derived list differs from the stored one by kind alone and every such kernel
            // silently declines.
            if root == Some(MixT::GFloat) {
                relabel_value_scalars(&mut out.caps, &out.bounds, &out.synth);
            }
            let (caps, bounds) = (out.caps, out.bounds);
            // Reproduce BOTH the capture set and the bounds obligations exactly (the i64 path's
            // rule, now that an f64 kernel can carry `Scalar`/`ScalarValue` caps and affine
            // bounds): any drift would run unchecked native loads behind a pre-check that doesn't
            // describe them.
            // `!caps.is_empty()`, matching the compile gate exactly — see
            // `reduce_jit_f64_range_captures`. These two must relax together or the build
            // declines a kernel the compiler emitted.
            return root == Some(MixT::GFloat)
                && caps == rl.captures
                && bounds == rl.index_bounds
                && !caps.is_empty()
                && caps.iter().all(|c| {
                    matches!(
                        c.kind,
                        CaptureKind::ArrayF64 | CaptureKind::Scalar | CaptureKind::ScalarValue
                    )
                })
                && caps.len() <= MAX_CAPTURES;
        }
        let n = rl.bodies.len();
        if n == 1 {
            // Identical gate to the compiler's `reduce_jit_f64_range_body` (root `Float`, and the
            // division/min-max soundness rule) so the build never lowers a body the compiler
            // rejected — or vice versa.
            return f64_range_body_eligible(&rl.bodies[0], &rl.pa, &rl.pb, fns, user_fns, msigs);
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
        // `$aff` naming is a deterministic function of the body, so the re-derived caps
        // (which include any synthetic slots) match the stored list iff nothing drifted —
        // the synth expressions themselves were consumed at the compile site's push loop.
        let mut synth: Vec<(String, Expr)> = Vec::new();
        let ok = value_eligible_cap_indexed(
            &rl.bodies[0],
            fns,
            &locals,
            rl.pb.as_str(),
            &mut caps,
            &mut bounds,
            &mut synth,
        );
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

/// Like [`map_kernel_captures`] but the body may additionally read a captured array —
/// `a[it]` (the binder) or `a[i]` (a loop-invariant scalar cap). Returns the ordered
/// captures plus the bounds the VM must discharge before the kernel's unchecked loads,
/// or `None` if ineligible. Shares [`value_eligible_cap_indexed`] with the reduce path:
/// a reduce passes its counter as `pb`, a map passes its binder, and the index shapes
/// the analysis accepts are the same.
///
/// The two paths differ in what `pb` MEANS, and that difference is a soundness cliff,
/// not a detail. A reduce's `pb` is the loop counter, so an [`IndexBound::Counter`] is
/// discharged by the range's endpoints. A map's binder is an ELEMENT VALUE: for
/// `xs.map(x => a[x])` the index is arbitrary data — and possibly negative, which the
/// interpreter Python-WRAPS rather than rejecting, so no cheap scan can discharge it.
/// The VM therefore takes this kernel ONLY when the receiver is a lazy `Range` (whose
/// elements ARE the counter), and only checks that BEFORE materializing it. See
/// [`crate::bytecode::ArrayKernel::index_bounds`] — the obligation is stated there
/// because the VM, not this analysis, is what discharges it.
pub fn map_kernel_captures_indexed(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
) -> Option<IndexedCaptures> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    let mut caps: Vec<Capture> = Vec::new();
    let mut bounds: Vec<IndexBound> = Vec::new();
    let mut synth: Vec<(String, Expr)> = Vec::new();
    if value_eligible_cap_indexed(body, fns, &locals, binder, &mut caps, &mut bounds, &mut synth)
        && caps.len() <= MAX_CAPTURES
    {
        // Relabel purely-value scalars to `ScalarValue` — same as the mixed twin, so the two
        // derivations of one body produce identical lists (the i64 kernel loads a `ScalarValue`
        // as `i64` exactly as it did a `Scalar`, so its behavior is unchanged; the relabel only
        // lets the mixed kernel recognize the same cap as `f64`). The reduce path does NOT
        // relabel, so its captures are untouched. `synth` carries any affine `$aff` terms —
        // the map compile site's push loop evaluates them, exactly as it does the mixed twin's.
        relabel_value_scalars(&mut caps, &bounds, &synth);
        Some((caps, bounds, synth))
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
    // The ROOT must be proven `Float` — with `uses_binder` required that is implied (a
    // Promotable root is a lone leaf), but asserting it directly is what the soundness
    // argument actually says.
    if f64_body_eligible(body, binder, &mut caps, &mut uses_binder, user_fns)
        == Some(F64Proof::Float)
        && uses_binder
        && caps.len() <= MAX_CAPTURES
    {
        Some(caps)
    } else {
        None
    }
}

/// How a node of an f64-source body relates to the INTERPRETER's arithmetic — the type
/// that keeps the monomorphic f64 kernel honest about integers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum F64Proof {
    /// Provably computed in `f64` by the interpreter: the binder (a `Floats` element), a
    /// float literal, a float builtin, or any operation with such an operand — the
    /// interpreter promotes the other side on the spot, so the kernel's f64 arithmetic
    /// matches bit-for-bit.
    Float,
    /// An int literal or a capture: EXACT to convert (`as f64`, the very conversion the
    /// dispatch marshal performs), but only at a node where a `Float` operand forces the
    /// interpreter to promote it. Anywhere else the interpreter computes in i64.
    Promotable,
}

/// The typed eligibility for the f64 (Floats-source) kernel. `None` = ineligible.
///
/// The rule that matters is `(Promotable, Promotable) => None`, and it exists because its
/// absence was a WRONG-VALUE, JIT-vs-interpreter divergence — the oracle-breaking kind:
///
///     k = 4611686018427387904            # 2^62, an Int
///     ys = (0..100000).map(it * 1.0)
///     ys.map(it + (k + k)).first()       # JIT:  9223372036854775808.0
///                                        # VM/tw: -9223372036854775808.0
///
/// The interpreter computes the `Int + Int` subexpression in i64 — WRAPPING — and only
/// then promotes; this kernel is monomorphic f64 and computes `f64(k) + f64(k)`, which
/// does not wrap. Same for `k * k`, for pure literal arithmetic
/// (`it + (9223372036854775807 + 1)`), and for `-k` (interpreter `wrapping_neg`, kernel
/// `fneg` — a sign flip, not a wrap, divergent at exactly `i64::MIN`; that one arrived
/// with the unary-minus admission and is fixed by the same rule). The mixed kernels are
/// immune by construction — `gen_value_typed` types per node and emits Int subtrees as
/// wrapping i64 ops — and probes confirmed the reduce/fused/scan families agree; this
/// monomorphic family was the only unsound one.
///
/// A `Promotable` under a `Float` operand is exact: the leaf is converted by `as f64`
/// once, which is bit-identical to what the interpreter's promotion does at that node,
/// and to what the dispatch marshal does to an `Int` capture.
fn f64_body_eligible(
    e: &Expr,
    binder: &str,
    caps: &mut Vec<String>,
    uses_binder: &mut bool,
    user_fns: &HashSet<&str>,
) -> Option<F64Proof> {
    match e {
        Expr::Float(_) => Some(F64Proof::Float),
        Expr::Int(_) => Some(F64Proof::Promotable),
        Expr::Ident { name, .. } => {
            if name == binder {
                *uses_binder = true;
                Some(F64Proof::Float)
            } else {
                if !caps.iter().any(|c| c == name) {
                    caps.push(name.clone());
                }
                Some(F64Proof::Promotable)
            }
        }
        Expr::Binary { op, left, right, .. } => {
            if !matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) {
                return None;
            }
            let l = f64_body_eligible(left, binder, caps, uses_binder, user_fns)?;
            let r = f64_body_eligible(right, binder, caps, uses_binder, user_fns)?;
            match (l, r) {
                // Int OP Int is the interpreter's i64 (wrapping) arithmetic — this kernel
                // cannot reproduce it, so the body declines to the VM, which is the
                // semantics. Everything else has a Float operand, so the interpreter
                // promotes here too.
                (F64Proof::Promotable, F64Proof::Promotable) => None,
                _ => Some(F64Proof::Float),
            }
        }
        // Negation of a PROVEN f64 is `fneg`, the interpreter's exact IEEE sign flip. A
        // Promotable operand must decline: the interpreter negates an Int with
        // `wrapping_neg`, which differs from a sign flip at exactly `i64::MIN`.
        Expr::Unary { op: UnOp::Neg, expr, .. } => {
            (f64_body_eligible(expr, binder, caps, uses_binder, user_fns)? == F64Proof::Float)
                .then_some(F64Proof::Float)
        }
        // `sqrt`/`abs`/`min`/`max` (emitted inline by `gen_builtin_f64`) — only the real
        // builtin, never a user function of the same name (which the f64 kernel can't
        // call). Arguments must be PROVEN Float: the interpreter's `abs(Int)` stays Int
        // (and wraps at `i64::MIN`), and a mixed-type `min`/`max` returns whichever
        // original operand wins, so its type is runtime-dependent — the same reasons
        // `infer_mixed_kind` rejects them.
        Expr::Call { name, args, .. } => {
            (jit_float_builtin_arity(name) == Some(args.len())
                && !user_fns.contains(name.as_str())
                && args.iter().all(|a| {
                    f64_body_eligible(a, binder, caps, uses_binder, user_fns)
                        == Some(F64Proof::Float)
                }))
            .then_some(F64Proof::Float)
        }
        _ => None,
    }
}

/// Is `body` a **mixed** `Int`-source → `Float` map: an `f64`-producing expression over
/// an `i64` element? Eligible when it uses the binder, is built only from `+ - *` over the
/// binder / int / float literals / free scalars, and its inferred root type is `Float`
/// (else it's a pure `i64` map). Returns the ordered captures, or `None` if ineligible.
/// The kernel ([`define_array_kernel`] with `mixed`) types every node bottom-up by
/// the interpreter's promotion rule — `Int OP Int` stays `i64` (wrapping `iadd/isub/imul`),
/// and the *first* `Float` operand promotes via `fcvt_from_sint` — so it matches the
/// interpreter bit-for-bit, including any `i64` wrap in an integer subexpression.
///
/// A free scalar rides as a plain `i64` [`CaptureKind::Scalar`] (loaded as `elem_ty`, which
/// is `I64` for a mixed kernel, and typed `Int` by [`gen_value_typed`]'s `Ident` arm). Captures
/// were once excluded here outright, because "a capture's runtime type is unknown at compile
/// time, and an `Int` capture in an `Int` subexpression must wrap as `i64`, which we couldn't
/// guarantee". We CAN guarantee it — just not statically: both dispatch sites (`try_map_range`
/// and `Op::TryJitMap` in `vm.rs`) require every capture to be a `Value::Int` at run time and
/// decline to the bytecode loop otherwise, which is the identical runtime proof the plain i64
/// map path has always relied on. A `Float` in that slot would promote EARLIER in the kernel
/// than in the interpreter, so declining is not a missed optimization but the correctness rule.
///
/// Excluding them cost a lot: capture-free `((7 * j) % 100) * 0.5` ran native while the same
/// body with `7` replaced by a variable fell to the VM — 0.01s vs 0.37s over 4M elements. That
/// is the shape every nested array build has (the inner map captures the outer binder), and
/// `map(i => i * dt)` besides.
/// `fns` is the `i64`-eligible set (`int_eligible` at build time, `jit_fn_set()` at compile
/// time — the same set by contract). A user function in it may be CALLED from a mixed body:
/// it takes `Int` arguments and returns `Int` by construction, which is precisely the
/// contract its `i64` specialization was compiled under, so the call types with no extra
/// information. Without this, factoring a loop body into a named function dropped the whole
/// map to the bytecode loop — measured 1.50s against 0.02s inline over 20M elements.
pub fn mixed_map_eligible(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<Vec<Capture>> {
    let mut uses_binder = false;
    let mut caps: Vec<Capture> = Vec::new();
    let root = infer_mixed_kind(body, binder, &mut uses_binder, &mut caps, fns, user_fns, msigs)?;
    (root == NumKind::Float && uses_binder && caps.len() <= MAX_CAPTURES).then_some(caps)
}

/// The **Int-rooted** mixed map: an `i64` source and an `i64` RESULT, through `Float`
/// intermediates — `map(i => to_int(to_float(i) * 1.5))`, the shape that previously had no
/// kernel at all (measured 4.05s JIT against 4.01s VM: silently interpreted). The same
/// node-by-node typing as [`mixed_map_eligible`], but the root must be `Int` — the kernel
/// reads `i64` and writes `i64`, so its ABI is exactly the plain i64 kernel's and it rides
/// the same FFI wrappers, dispatch arm, and in-place reuse.
///
/// It must never COMPETE with the plain i64 kernel: the compile site tries the i64 analysis
/// first, and the build re-gate requires `map_kernel_captures` to have REJECTED the body
/// (a float literal or float-producing call somewhere is what makes this shape this shape).
/// The four rounding builtins that RAISE when their result leaves the i64 range. Arity 1
/// only — `round(x, digits)` stays a `Float` and is a different (non-raising) operation the
/// analyses do not admit.
const RAISING_ROUNDERS: &[&str] = &["floor", "ceil", "round", "trunc"];

/// Whether a kernel body can RAISE where native code would silently produce inf/NaN or a
/// wrapped integer — i.e. whether its kernel needs the poison out-param and its dispatch the
/// poison call wrapper. A user function SHADOWING one of these names is not the raising
/// builtin (the call dispatches to the user's function), so it does not count.
/// Over-approximates on an `Int`-typed argument (where the builtin is the identity and cannot
/// raise): the kernel then carries a poison slot it never sets, which costs one dead store
/// and nothing else.
///
/// Shared by MAP and REDUCE bodies — the question and the expression forms are identical, and
/// one predicate is what keeps the two from drifting apart as either side widens. Its answer
/// is stored on the kernel ([`crate::bytecode::ArrayKernel::raises`],
/// [`crate::bytecode::ReduceLoop::raises`]) rather than recomputed by the VM, because the
/// answer decides an ABI and the VM cannot reach the user functions a call would need.
pub fn body_raises(e: &Expr, user_fns: &HashSet<&str>, msigs: &MixedSigTable) -> bool {
    match e {
        Expr::Call { name, args, .. } => {
            // A call to a MIXED specialization always counts: its ABI carries a poison
            // pointer precisely because it can bail — a NaN comparison anywhere in its body,
            // or a `/0`. The kernel must therefore be built with the poison signature so the
            // callee's flag has somewhere to land, even when the map body itself contains no
            // rounder and no division. (Without this the kernel is built poison-free, the VM
            // calls the non-poison wrapper, and a raising callee is silently swallowed.)
            (msigs.contains_key(name.as_str()) && user_fns.contains(name.as_str()))
                || (RAISING_ROUNDERS.contains(&name.as_str())
                    && args.len() == 1
                    && !user_fns.contains(name.as_str()))
                || args.iter().any(|a| body_raises(a, user_fns, msigs))
        }
        // Any `/`: the interpreter raises on a zero divisor. Over-approximates on a nonzero
        // literal divisor (which cannot raise) — that costs a dead poison slot, nothing else.
        Expr::Binary { op: BinOp::Div, .. } => true,
        Expr::Binary { left, right, .. } => {
            body_raises(left, user_fns, msigs) || body_raises(right, user_fns, msigs)
        }
        Expr::Unary { expr, .. } => body_raises(expr, user_fns, msigs),
        Expr::Index { recv, index, .. } => {
            body_raises(recv, user_fns, msigs) || body_raises(index, user_fns, msigs)
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            body_raises(cond, user_fns, msigs)
                || body_raises(then_branch, user_fns, msigs)
                || body_raises(else_branch, user_fns, msigs)
        }
        _ => false,
    }
}

pub fn mixed_map_int_root_eligible(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<Vec<Capture>> {
    let mut uses_binder = false;
    let mut caps: Vec<Capture> = Vec::new();
    let root = infer_mixed_kind(body, binder, &mut uses_binder, &mut caps, fns, user_fns, msigs)?;
    (root == NumKind::Int && uses_binder && caps.len() <= MAX_CAPTURES).then_some(caps)
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
    caps: &mut Vec<Capture>,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        // Negation PRESERVES its operand's kind, so it needs no promotion rule of its own.
        // Emitted by `gen_value_typed`'s twin arm as `ineg`/`fneg` — wrapping exactly like
        // the interpreter's `wrapping_neg`, and the exact IEEE sign flip, respectively.
        //
        // Admitted here and emitted there in the SAME commit, deliberately: `e30f9fe` fixed
        // the i64 kernel by adding eligibility alone, because `gen_value` already lowered
        // `Neg`; this path had NEITHER, and admitting a shape the codegen cannot emit is
        // how this area was reverted three times before.
        Expr::Unary { op: UnOp::Neg, expr, .. } => {
            infer_mixed_kind(expr, binder, uses_binder, caps, fns, user_fns, msigs)
        }
        // A USER function with an `i64` specialization. Tried BEFORE the builtin arm, so a
        // user function shadowing `abs`/`min`/`max` dispatches to the user's function — the
        // precedence `gen_value` already establishes via its `fn_ids` lookup, and mirrored
        // by `gen_value_typed`'s twin arm.
        //
        // Every argument must type `Int`, which is exactly the contract the callee's i64
        // specialization was compiled under (`int_eligible` means "i64-closed for all-`Int`
        // arguments"), so the result is an `i64` and types `Int` here. The enclosing
        // expression then promotes it at the first `Float` precisely where the interpreter
        // does. A FLOAT argument is rejected rather than converted: the callee has no f64
        // form to call, and silently truncating or promoting would not be the interpreter's
        // answer.
        //
        // That `Int` check is defence in depth, not the only line: relaxing it does not
        // produce a wrong answer, because the f64 value would reach an `i64` call signature
        // and Cranelift rejects the function, so the kernel simply declines (verified by
        // removing the check — the three float-argument cases still agree on all engines).
        // It is kept because the alternative is CONSTRUCTING ill-typed IR and relying on the
        // builder to refuse it, and a builder that panics instead of erroring would breach
        // ADR-0024's never-abort guarantee. Cheaper to never build it.
        // ONE ARM FOR EVERY USER CALL, because two of them could not both be tried.
        //
        // This used to be two arms: an i64 one guarded by `fns.contains(name)`, and a mixed
        // one guarded by `msigs.contains_key(name)` below it, with a comment asserting that
        // "an all-`Int` function has no mixed form, so the two never compete". THAT WAS
        // FALSE, and it cost 66x on the shape this JIT exists for:
        //
        //     fn f(x: Float) -> Float = x * x            (0..20M).map(i => f(to_float(i)))
        //     fn f(x: Float) -> Float = x * x * 1.0       the SAME call site
        //
        // 1.85s and 0.028s. `fns` means "i64-closed BODY", not "Int parameters" — and
        // `x * x` is i64-closed, so the first `f` is in BOTH sets. The i64 arm claimed the
        // call site by name, typed `to_float(i)` as Float, and returned None. Rust match arms
        // cannot fall through, so the mixed arm twenty lines below was unreachable for
        // exactly the callee that needed it. Adding a redundant `* 1.0` to the CALLEE — which
        // does nothing to the call site — pushed `f` out of `fns` and let the mixed arm see
        // it. That is a two-character difference with a 66x cost and no feedback.
        //
        // Merged, the priority is unchanged where it used to apply and defined where it did
        // not: all-Int arguments to an i64-closed function still take the i64 path first;
        // anything else gets the mixed specialization if the argument kinds EQUAL the
        // callee's parameter kinds. That equality is strict on purpose — the specialization
        // was compiled for exactly those kinds and there is no promoting at the boundary,
        // the same rule `infer_typed_env` uses for a mixed sibling call.
        //
        // Every argument is typed EXACTLY ONCE, into a `Vec`, before anything is decided.
        // Walking them twice would be fine for `record_cap` (which dedupes by name) but is
        // not a property worth relying on for `uses_binder` or for whatever capture-order
        // logic arrives next.
        Expr::Call { name, args, .. } if user_fns.contains(name.as_str()) => {
            let mut kinds = Vec::with_capacity(args.len());
            for a in args {
                kinds.push(infer_mixed_kind(a, binder, uses_binder, caps, fns, user_fns, msigs)?);
            }
            let all_int = kinds.iter().all(|k| *k == NumKind::Int);
            if all_int && fns.contains(name.as_str()) {
                if !jit_builtin_arity_ok(name, args.len()) {
                    return None;
                }
                return Some(NumKind::Int);
            }
            // The MIXED specialization — the `Float`-parameter callee. The kernel marshals
            // to the bits ABI and shares its poison cell, so a NaN-compare or `/0` inside
            // the callee bails the whole map exactly as it bails a mixed function.
            let (params, ret) = msigs.get(name.as_str())?;
            if kinds.len() != params.len() || kinds.iter().zip(params).any(|(k, w)| k != w) {
                return None;
            }
            Some(*ret)
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    Some(NumKind::Float) // sqrt always returns Float
                }
                // `to_float` is the explicit Int->Float conversion: always Float, and the typed
                // codegen emits the same `fcvt_from_sint` promotion it already emits for `sqrt`.
                ("to_float", 1) => {
                    infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs), // preserves kind
                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                ("to_int" | "sign", 1) => {
                    infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    Some(NumKind::Int)
                }
                // The RAISING rounders: `Float` in, `Int` out, and an out-of-i64-range result
                // raises where the never-raising `to_int` saturates. Admissible only because
                // the kernel carries a poison out-param (`ArrayKernel::raises`, set by
                // `body_raises` from this same name list): on any raising condition the
                // codegen sets poison, the VM discards the whole output, and the bytecode loop
                // re-runs to raise the exact interpreter error. An `Int` argument makes the
                // builtin the identity (`floor(2) == 2`) and is admitted as such. (This plain
                // analysis types every operand `Int` or genuine `Float` — value scalars ride
                // as `i64` here — so there is no unpromoted-scalar case to refuse.)
                ("floor" | "ceil" | "round" | "trunc", 1) => {
                    match infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)? {
                        NumKind::Int => Some(NumKind::Int), // identity on Int
                        NumKind::Float => Some(NumKind::Int),
                    }
                }
                ("min" | "max", 2) => {
                    let ka = infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    let kb = infer_mixed_kind(&args[1], binder, uses_binder, caps, fns, user_fns, msigs)?;
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
                // A free scalar, typed `Int` and loaded `i64` — sound ONLY because the VM
                // proves the value really is a `Value::Int` before dispatch (see
                // [`mixed_map_eligible`]); a `Float` there declines to the bytecode loop.
                // Typing it `Int` is what keeps an integer subexpression containing it
                // wrapping exactly like the interpreter's.
                record_cap(caps, name, CaptureKind::Scalar).then_some(NumKind::Int)
            }
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_mixed_kind(left, binder, uses_binder, caps, fns, user_fns, msigs)?;
            let rk = infer_mixed_kind(right, binder, uses_binder, caps, fns, user_fns, msigs)?;
            Some(if lk == NumKind::Float || rk == NumKind::Float {
                NumKind::Float
            } else {
                NumKind::Int
            })
        }
        // `/` is always float division and always yields Float, for ANY eligible divisor —
        // admissible because `body_raises` counts every `/`, so the kernel carries the
        // poison accumulator `gen_value_typed`'s Div arm ORs `divisor == 0.0` into (the
        // interpreter raises on `/0` where native `fdiv` yields inf). This is what lets
        // `ceil(to_float(i) / 4.0)` compile instead of forcing the `* 0.25` spelling.
        Expr::Binary { op: BinOp::Div, left, right, .. } => {
            infer_mixed_kind(left, binder, uses_binder, caps, fns, user_fns, msigs)?;
            infer_mixed_kind(right, binder, uses_binder, caps, fns, user_fns, msigs)?;
            Some(NumKind::Float)
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
            let lk = infer_mixed_kind(left, binder, uses_binder, caps, fns, user_fns, msigs)?;
            let rk = infer_mixed_kind(right, binder, uses_binder, caps, fns, user_fns, msigs)?;
            if lk == NumKind::Int && rk == NumKind::Int {
                Some(NumKind::Int)
            } else {
                None // an i64-only op with a Float operand is not a valid Helix expression
            }
        }
        _ => None,
    }
}

/// The **indexed** mixed-map analysis: an i64 range source, an `f64` result, and a body that
/// reads captured **`f64` arrays** by the binder (`a[it]`) or by a loop-invariant scalar
/// (`a[k]`) — the vector-add / AXPY / gather-transform shape `(0..n).map(i => a[i] + b[i])`.
/// Returns the ordered captures plus the bounds the VM must discharge, or `None`.
///
/// This types `a[…]` as **`Float`** where [`map_kernel_captures_indexed`] (the i64 twin over
/// the same body shapes) types it `Int`. Both analyses record the same names, kinds, and
/// bounds in the same first-appearance order, so ONE stored kernel can carry BOTH
/// specializations and the VM dispatches on the runtime capture type: all-`Ints` caps run the
/// i64 kernel, all-`Floats` caps run this mixed kernel, and a mismatch falls back to the
/// bytecode loop. The `ArrayI64` capture kind therefore means "array indexed by the counter",
/// not "an array of i64" — which marshal it gets is the dispatch's decision, and the marshal
/// itself is the type guard (an `Ints` buffer never reaches this kernel's F64 loads).
///
/// INDEX scalars (`a[k]`, affine `base`/`coef`) stay `Scalar` (`i64`, an index is an integer);
/// VALUE scalars (`a` in `a * x[i]`) become [`CaptureKind::ScalarValue`] via
/// [`relabel_value_scalars`], loaded `f64` here and `i64` in the i64 twin. A value scalar is
/// admitted only where a genuine float promotes it (SAXPY `a * x[i]`), not `a * i` — see
/// [`MixT`]. The bounds story is IDENTICAL to the i64 path — same [`IndexBound`]s, same
/// lazy-range-only discharge (`map_index_caps` in `vm.rs`) — because bounds depend on the
/// index arithmetic, which is `i64` in both.
pub type MapIndexAnalysis = (Vec<Capture>, Vec<IndexBound>, Vec<(String, Expr)>);

pub fn mixed_map_captures_indexed(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<MapIndexAnalysis> {
    let mut acc = IndexedOut::default();
    let root = infer_mixed_kind_indexed(body, binder, &mut acc, fns, user_fns, msigs)?;
    let IndexedOut { mut caps, bounds, synth } = acc;
    // GFloat root: a genuine `f64` the kernel writes to the output buffer (a bare-`SFloat` root
    // — an un-promoted value scalar — is rejected, matching the interpreter). Non-empty bounds:
    // an unindexed body belongs to the plain i64/f64/mixed analyses, which run first at the
    // compile site.
    if root == MixT::GFloat && !bounds.is_empty() && caps.len() <= MAX_CAPTURES {
        relabel_value_scalars(&mut caps, &bounds, &synth);
        Some((caps, bounds, synth))
    } else {
        None
    }
}

/// The VALUE-SCALAR variant of the plain mixed map: an unindexed `Int`→`Float` body whose
/// free scalars ride as **f64 bits** (`ScalarValue`) instead of proven-`Int` `i64`s. This is
/// the second specialization of the same stored kernel: the plain analysis
/// ([`mixed_map_eligible`]) types captures `Int` and its dispatch declines when one is a
/// runtime `Float` — so `d = 4.0; map(i => to_float(i) / d)` ran on the VM (3.48s against
/// 0.24s for `d = 4` at 20M) while producing identical values. Here the [`MixT`] analysis
/// admits a capture only where a genuine float promotes it (`mix_combine`'s sabotage-proven
/// rule), which is exactly when riding as f64 matches the interpreter bit for bit.
///
/// Returns the ordered captures (all relabeled `ScalarValue`), or `None`. Bounds and synth
/// must be EMPTY — an indexed body belongs to [`mixed_map_captures_indexed`] — and the list
/// must be non-empty, since a capture-free body is already the plain kernel's territory.
/// The build gate compares NAMES AND ORDER against the stored list (the stored kinds are the
/// plain analysis's `Scalar`s; the kinds here are what this specialization loads by).
pub fn mixed_map_value_scalar_eligible(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<Vec<Capture>> {
    let mut acc = IndexedOut::default();
    let root = infer_mixed_kind_indexed(body, binder, &mut acc, fns, user_fns, msigs)?;
    let IndexedOut { mut caps, bounds, synth } = acc;
    if root == MixT::GFloat
        && bounds.is_empty()
        && synth.is_empty()
        && !caps.is_empty()
        && caps.len() <= MAX_CAPTURES
        && caps.iter().all(|c| c.kind == CaptureKind::Scalar)
    {
        relabel_value_scalars(&mut caps, &bounds, &synth);
        Some(caps)
    } else {
        None
    }
}

/// The kernel-vs-interpreter type of an indexed-mixed subexpression. The mixed kernel
/// evaluates integer subexpressions in `i64` and promotes at the first FLOAT, exactly like
/// the interpreter's `arith` — so the two agree bit-for-bit ONLY if they promote at the same
/// point. A value scalar breaks that: it rides as `f64` in the kernel but is possibly-`Int`
/// at runtime, so the interpreter keeps it `i64` until IT hits a float. `MixT` tracks the
/// distinction that makes the promotion points line up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MixT {
    /// Both engines evaluate this in `i64` (binder, int literal, index scalar, i64-closed op).
    Int,
    /// A GENUINE float in both engines — an array load or a float literal — so combining it
    /// with anything promotes identically (the interpreter promotes the `i64` side to `f64`,
    /// which is the same `fcvt` the kernel does). Safe to combine with anything.
    GFloat,
    /// A value SCALAR riding as `f64` but possibly `Int` at runtime. Safe ONLY once a
    /// `GFloat` has promoted it: `a * x[i]` (`SFloat * GFloat`) is fine because the
    /// interpreter also promotes `a` there, but `a * i` / `a + b` (`SFloat` with `Int` or
    /// another bare `SFloat`) would be `i64` in the interpreter and `f64` in the kernel —
    /// diverging once the true product exceeds 2^53. Such a node is REJECTED.
    SFloat,
}

/// Combine two `+`/`-`/`*` operand kinds: a genuine float promotes anything (both engines
/// promote, so it is safe); two `Int`s stay `Int`; a value scalar NOT paired with a genuine
/// float is the divergence case and is rejected. Shared by the mixed MAP analysis
/// ([`infer_mixed_kind_indexed`]) and the f64 indexed REDUCE analysis ([`infer_f64_indexed`])
/// so both sites enforce one rule — the rule proven load-bearing by sabotage (forcing
/// `(SFloat, Int)` to combine makes `(2^53+1) * 3 + x[i]` differ from the interpreter).
fn mix_combine(l: MixT, r: MixT) -> Option<MixT> {
    match (l, r) {
        (MixT::GFloat, _) | (_, MixT::GFloat) => Some(MixT::GFloat),
        (MixT::Int, MixT::Int) => Some(MixT::Int),
        // (SFloat, Int) | (Int, SFloat) | (SFloat, SFloat): the interpreter may do i64.
        _ => None,
    }
}

/// Bottom-up [`MixT`] of an indexed-mixed node, recording captures/bounds as it goes —
/// [`infer_mixed_kind`]'s arm set plus the index shapes and value-scalar captures, mirroring
/// [`gen_value_typed`]'s codegen (a node this admits and that miscompiles is a divergence, so
/// the two stay twins). No `Let` arm — `gen_value_typed` has none, so the counter-shadowing
/// hazard the i64 path guards against cannot arise here: a shadowing body is simply
/// ineligible. The caller requires the root to be [`MixT::GFloat`] (the map writes `f64`).
fn infer_mixed_kind_indexed(
    e: &Expr,
    binder: &str,
    out: &mut IndexedOut,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<MixT> {
    match e {
        Expr::Int(_) => Some(MixT::Int),
        Expr::Float(_) => Some(MixT::GFloat),
        Expr::Ident { name, .. } => {
            if name == binder {
                Some(MixT::Int) // the i64 range element
            } else {
                // A value scalar — recorded `Scalar` here, relabeled to `ScalarValue` by
                // `relabel_value_scalars` once the bounds show it is not an index. It rides as
                // `f64` in the mixed kernel, so `SFloat`.
                record_cap(&mut out.caps, name, CaptureKind::Scalar).then_some(MixT::SFloat)
            }
        }
        Expr::Index { recv, index, .. } => {
            let arr = match &**recv {
                Expr::Ident { name, .. } if name != binder => name,
                _ => return None,
            };
            let ap = record_cap_pos(&mut out.caps, arr, CaptureKind::ArrayI64)?;
            match &**index {
                // `a[binder]`: read by the counter → a Counter bound.
                Expr::Ident { name: idx, .. } if idx == binder => {
                    push_bound(&mut out.bounds, IndexBound::Counter { array: ap as u32 });
                }
                // `a[k]`: a free loop-invariant scalar → a point bound. The index scalar is
                // recorded `Scalar` and STAYS `Scalar` (an index is `i64`).
                Expr::Ident { name: idx, .. } if idx != arr => {
                    let sp = record_cap_pos(&mut out.caps, idx, CaptureKind::Scalar)?;
                    push_bound(&mut out.bounds, IndexBound::Scalar { array: ap as u32, scalar: sp as u32 });
                }
                // An AFFINE index (`a[2*i]`, `a[i*n + k]` with the map binder as the counter)
                // — the same admission as the f64 reduce's [`infer_f64_indexed`]. The whole
                // index is validated first as a pure `i64` expression over the binder, free
                // scalars, and `Int` literals (codegen lowers it VERBATIM from those caps, so
                // it must be checked verbatim; every leaf effect-free and non-trapping, which
                // licenses `affine_split`'s algebraic folding). `base`/`coef` land as extra
                // Scalar cap slots — bare idents reuse the body's own caps, compound terms
                // (`i*n`) get a synthetic `$aff{k}` slot the compile site evaluates once —
                // and the VM proves the two ENDPOINT indices of the range in bounds, in i128
                // (`map_index_caps`, composed with the range's step). There is no `pa` in a
                // map, so the empty string — never a legal ident — fills that reject-slot.
                _ => {
                    index_scalars_eligible(index, "", binder, &mut out.caps)?;
                    let (base, coef) = affine_split(index, binder)?;
                    let bp = record_index_term(&mut out.caps, &mut out.synth, base)?;
                    let cp = record_index_term(&mut out.caps, &mut out.synth, coef)?;
                    push_bound(
                        &mut out.bounds,
                        IndexBound::Affine { array: ap as u32, base: bp as u32, coef: cp as u32 },
                    );
                }
            }
            Some(MixT::GFloat) // an f64 array load is a genuine float
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                // `sqrt` promotes its argument to `f64` in BOTH engines, so an `SFloat` arg is
                // safe here and the result is a genuine float.
                ("sqrt", 1) => {
                    infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)?;
                    Some(MixT::GFloat)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)?;
                    Some(MixT::GFloat)
                }
                // `abs`/`min`/`max` do NOT promote (interp `abs(Int)` is `iabs`, `min(Int,Int)`
                // an i64 compare) — so an `SFloat` argument would diverge. Admit only genuine
                // floats or ints, and preserve the kind (an `Int` `abs`/`min` stays i64).
                                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                // An unpromoted value scalar is refused for the same reason `abs` refuses one:
                // its runtime type is not yet pinned, and `to_int`/`sign` read it directly.
                ("to_int" | "sign", 1) => match infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)? {
                    MixT::SFloat => None,
                    _ => Some(MixT::Int),
                },
("abs", 1) => match infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)? {
                    MixT::SFloat => None,
                    k => Some(k),
                },
                ("min" | "max", 2) => {
                    let ka = infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)?;
                    let kb = infer_mixed_kind_indexed(&args[1], binder, out, fns, user_fns, msigs)?;
                    if ka == kb && ka != MixT::SFloat { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        // A USER FUNCTION. Without this arm, a body that BOTH captures a float and calls a
        // user function declines — and that is exactly the shape of a numerical derivative,
        // where the captured value is the step size:
        //
        //     h = 0.001
        //     fn f(x: Float) -> Float = x * x
        //     (0..10M).map(i => (f(to_float(i) + h) - f(to_float(i))) / h)     1.783 s
        //     …the same body with `h` written as the literal 0.001              0.021 s
        //
        // 86x for naming a constant. Both halves already worked on their own — a float
        // capture with no call, and a user call with no capture (becf927) — so the gap was
        // only that this walker had no user-call arm at all, and no access to the tables it
        // would need to type one.
        //
        // TWO RULES, AND THE SECOND IS THE SUBTLE ONE:
        //
        // * An i64-closed function with all-`Int` arguments returns `Int`, the same priority
        //   the unindexed walker gives it.
        // * A `Float` parameter must receive a GENUINE float (`GFloat`), never an `SFloat` —
        //   the same rule `abs`, `to_int` and `sign` apply two arms above, for the same
        //   reason. An `SFloat` is a value scalar riding as `f64` that may be an `Int` at
        //   runtime, and an ANNOTATION IS NOT A COERCION: with `c = 2^53+1` and
        //   `fn f(x: Float) -> Float = x * x`, the interpreter computes `f(c)` as a WRAPPING
        //   i64 multiply and answers `18014398509481985` (an Int!), while `f(to_float(c))` is
        //   an f64 multiply answering `8.1e31`. Handing a callee an unpromoted capture would
        //   pick the second while the interpreter picks the first.
        //
        //   DEFENCE IN DEPTH, and stated as such because sabotage would not break it:
        //   relaxing this to admit `SFloat` left every probe — including that 2^53 pair —
        //   byte-identical on all three engines, because the kernel's runtime dispatch
        //   independently declines when a `ScalarValue` capture turns out to be an `Int`. The
        //   guard is kept for the reason the sibling check one function up is kept: the
        //   alternative is CONSTRUCTING ill-typed IR and relying on a later check to refuse
        //   it, and this file would rather never build it.
        //
        //   A capture reaches a callee as a genuine float only once something promoted it,
        //   which is exactly what `to_float(i) + h` does — so the derivative shape qualifies.
        //
        // No codegen work: the value-scalar and indexed variants both lower through
        // `gen_value_typed`, whose merged call arm already emits both forms.
        Expr::Call { name, args, .. } if user_fns.contains(name.as_str()) => {
            let mut kinds = Vec::with_capacity(args.len());
            for a in args {
                kinds.push(infer_mixed_kind_indexed(a, binder, out, fns, user_fns, msigs)?);
            }
            if kinds.iter().all(|k| *k == MixT::Int) && fns.contains(name.as_str()) {
                return jit_builtin_arity_ok(name, args.len()).then_some(MixT::Int);
            }
            let (params, ret) = msigs.get(name.as_str())?;
            if kinds.len() != params.len() {
                return None;
            }
            for (k, want) in kinds.iter().zip(params) {
                let ok = match want {
                    NumKind::Int => *k == MixT::Int,
                    // NEVER `SFloat` — see above.
                    NumKind::Float => *k == MixT::GFloat,
                };
                if !ok {
                    return None;
                }
            }
            Some(match ret {
                NumKind::Int => MixT::Int,
                NumKind::Float => MixT::GFloat,
            })
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_mixed_kind_indexed(left, binder, out, fns, user_fns, msigs)?;
            let rk = infer_mixed_kind_indexed(right, binder, out, fns, user_fns, msigs)?;
            mix_combine(lk, rk)
        }
        // `/` promotes BOTH operands in BOTH engines (even `Int / Int` is a float divide,
        // `10 / 2 == 5.0`), so unlike `+ - *` it is safe for ANY operand mix — including an
        // unpromoted value scalar, which is precisely the promotion the interpreter also
        // performs at this node. Result is a genuine float. A zero divisor poisons
        // (`body_raises` counts every `/`, so a dividing kernel always carries the
        // poison accumulator `gen_value_typed`'s Div arm ORs into).
        Expr::Binary { op: BinOp::Div, left, right, .. } => {
            infer_mixed_kind_indexed(left, binder, out, fns, user_fns, msigs)?;
            infer_mixed_kind_indexed(right, binder, out, fns, user_fns, msigs)?;
            Some(MixT::GFloat)
        }
        Expr::Binary {
            op: op @ (BinOp::Mod | BinOp::FloorDiv | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr),
            left,
            right,
            ..
        } => {
            let op_ok = match op {
                BinOp::Mod | BinOp::FloorDiv => matches!(**right, Expr::Int(n) if n > 0),
                BinOp::Shl | BinOp::Shr => matches!(**right, Expr::Int(n) if (0..=63).contains(&n)),
                _ => true,
            };
            if !op_ok {
                return None;
            }
            let lk = infer_mixed_kind_indexed(left, binder, out, fns, user_fns, msigs)?;
            let rk = infer_mixed_kind_indexed(right, binder, out, fns, user_fns, msigs)?;
            // The i64-closed ops require both operands `Int` — a genuine float or a value
            // scalar is not even valid Helix here (`x[i] % 3` on an f64 array is a type error).
            if lk == MixT::Int && rk == MixT::Int {
                Some(MixT::Int)
            } else {
                None
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
        // Unary negation, admitted for exactly the reason `value_eligible` (the
        // capture-free twin) already admits it: `gen_value`'s `Neg` arm lowers it to
        // `ineg`, which wraps precisely like the interpreter's `wrapping_neg`. Nothing in
        // codegen changes — this gate was simply the only one of the pair that had not
        // been taught the operator.
        //
        // Its absence made the IDIOMATIC spelling lose to the clumsy one, which is the
        // defect signature this project hunts. At 8M elements, bit-identical results:
        //     xs.map(-it)        0.43s   vs   xs.map(0 - it)      0.05s
        //     xs.map(-(it + 1))  0.48s   vs   xs.map((0 - it) - 1) 0.06s
        Expr::Unary { op: UnOp::Neg, expr, .. } => value_eligible_cap(expr, eligible, locals, caps),
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
/// Returns the ordered captures, or `None` if the predicate is ineligible.
///
/// A `filter` predicate may CAPTURE free `i64` variables, exactly as a `map` body may — each
/// is passed to the kernel as a loop-invariant `caps[i]` and proven `Int` at dispatch. Without
/// this, `xs.filter(it % k == 0)` fell to the bytecode loop while the identical
/// `xs.filter(it % 7 == 0)` ran natively: measured 0.66s against 0.01s over 10M elements, the
/// same "swap a literal for a variable" cliff the map path had.
///
/// The two FUSED call sites require an EMPTY list: a fused pipeline has no caps mechanism, so
/// a capturing predicate must decline there and be handled by this standalone kernel instead.
pub fn filter_kernel_eligible(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
) -> Option<Vec<Capture>> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    let mut names: Vec<String> = Vec::new();
    if cond_eligible_cap(body, fns, &locals, &mut names) && names.len() <= MAX_CAPTURES {
        Some(names.into_iter().map(|name| Capture { name, kind: CaptureKind::Scalar }).collect())
    } else {
        None
    }
}

/// Like [`filter_kernel_eligible`] but for a **`Floats`-source** predicate: comparisons
/// over the [`F64Proof`] expression subset, combined with `and`/`or`. Each comparison
/// needs at least one PROVEN-Float side — two `Promotable` sides would be the
/// interpreter's exact i64 comparison (`k1 < k2` on Int captures), which f64 cannot
/// reproduce above 2^53.
///
/// NaN is handled at RUN time, deliberately not here. The interpreter RAISES on a NaN
/// operand in an ordering comparison ("cannot compare these values") and is IEEE for
/// `==`/`!=`; the kernel therefore accumulates an `Unordered` flag per ordering
/// comparison (see [`gen_cond`]) and returns -1, and the dispatch falls back to the
/// bytecode loop for the exact error at the exact element. That covers NaN produced
/// INSIDE the predicate too (`it - it < 1.0` over an `inf` element), which no source
/// pre-scan could see — and costs nothing on clean data.
pub fn filter_kernel_eligible_f64(
    body: &Expr,
    binder: &str,
    user_fns: &HashSet<&str>,
) -> Option<Vec<Capture>> {
    let mut names: Vec<String> = Vec::new();
    let mut uses_binder = false;
    if cond_eligible_f64(body, binder, &mut names, &mut uses_binder, user_fns)
        && names.len() <= MAX_CAPTURES
    {
        Some(
            names
                .into_iter()
                .map(|name| Capture { name, kind: CaptureKind::Scalar })
                .collect(),
        )
    } else {
        None
    }
}

fn cond_eligible_f64(
    e: &Expr,
    binder: &str,
    caps: &mut Vec<String>,
    uses_binder: &mut bool,
    user_fns: &HashSet<&str>,
) -> bool {
    match e {
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible_f64(left, binder, caps, uses_binder, user_fns)
                && cond_eligible_f64(right, binder, caps, uses_binder, user_fns)
        }
        Expr::Binary {
            op: BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne,
            left,
            right,
            ..
        } => {
            let l = f64_body_eligible(left, binder, caps, uses_binder, user_fns);
            let r = f64_body_eligible(right, binder, caps, uses_binder, user_fns);
            // At least one side proven Float: the interpreter then promotes the other
            // side AT the comparison, exactly as the marshal's `as_f64` does.
            matches!(
                (l, r),
                (Some(F64Proof::Float), Some(_)) | (Some(_), Some(F64Proof::Float))
            )
        }
        _ => false,
    }
}

fn eligible_set<'a>(funcs: &[FnDef<'a>], kind: NumKind) -> HashSet<&'a str> {
    // Exclude every function on a recursion *cycle* — directly self-recursive OR
    // mutually recursive. A JIT'd function recurses on the native stack with no
    // depth guard, so unbounded recursion (a missing base case) would overflow the
    // native stack and crash the process instead of raising a clean, catchable
    // error. This is a transitive call-graph check, not just a direct self-call
    // test: the JIT's memory safety must NOT silently depend on the front-end's
    // define-before-use rule — a front-end policy that could change (see
    // `recursive_funcs`). It since DID change: two-pass bytecode registration made
    // mutual recursion representable, and this check absorbed it with no edit. The
    // property is pinned by `unbounded_mutual_recursion_raises_instead_of_crashing`.
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
/// The free identifiers of `e` — the names it reads that it does not itself bind.
///
/// Correct by construction over exactly the forms [`value_eligible`] ACCEPTS, which is
/// all that is needed: its catch-all is `false`, so for any other expression the body is
/// ineligible and its capture list is never consulted. Within that set, binders occur in
/// only two places — `Let` bindings and `Match` arm patterns — and both are handled here
/// the same way `value_eligible` handles them (a `Let` binding is in scope for the
/// bindings after it and for the body; an arm's pattern names are in scope for its guard
/// and body). First-appearance order, no duplicates: the order IS the parameter order of
/// the compiled specialization, so it must be deterministic.
fn free_idents<'a>(e: &'a Expr, bound: &HashSet<&'a str>, out: &mut Vec<&'a str>) {
    match e {
        Expr::Ident { name, .. } => {
            let n = name.as_str();
            if !bound.contains(n) && !out.contains(&n) {
                out.push(n);
            }
        }
        Expr::Binary { left, right, .. } => {
            free_idents(left, bound, out);
            free_idents(right, bound, out);
        }
        Expr::Unary { expr, .. } => free_idents(expr, bound, out),
        // The callee NAME is a function, not a value — only the arguments are reads.
        Expr::Call { args, .. } => {
            for a in args {
                free_idents(a, bound, out);
            }
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            free_idents(cond, bound, out);
            free_idents(then_branch, bound, out);
            free_idents(else_branch, bound, out);
        }
        Expr::Let { bindings, body } => {
            let mut bound2 = bound.clone();
            for (n, v) in bindings {
                free_idents(v, &bound2, out);
                bound2.insert(n.as_str());
            }
            free_idents(body, &bound2, out);
        }
        Expr::Match { scrutinee, arms, .. } => {
            free_idents(scrutinee, bound, out);
            for arm in arms {
                let mut bound2 = bound.clone();
                // Exactly what `match_eligible` binds: a single `Bind` pattern. Literal,
                // `Or` and wildcard patterns bind no names, and any richer pattern makes
                // the arm ineligible there, so nothing else can be in scope here.
                if let crate::ast::Pattern::Bind(n) = &arm.pattern {
                    bound2.insert(n.as_str());
                }
                if let Some(g) = &arm.guard {
                    free_idents(g, &bound2, out);
                }
                free_idents(&arm.body, &bound2, out);
            }
        }
        // Literals bind and read nothing; anything else makes the body ineligible.
        _ => {}
    }
}

/// Tail-self-recursive functions that would be `i64`-eligible IF the globals they read
/// were parameters, together with those globals in parameter order.
///
/// This is the loop counterpart of the capture work the map/filter/reduce kernels got:
/// `value_eligible`'s `Ident` arm admits only parameters, so ONE global read anywhere in
/// a function — condition or body, it made no difference — dropped the entire loop to the
/// bytecode VM. Measured at 10M iterations: 0.01s compiled against 0.80s interpreted, an
/// 80x penalty for naming a bound instead of passing it.
///
/// Deliberately ADDITIVE. `eligible_set` is untouched, so `int_eligible_fns` — which the
/// bytecode compiler reads to decide whether a kernel may CALL a user function — still
/// describes exactly the functions whose ABI is `params.len()` arguments. A capture-taking
/// function is compiled under its own entry point that only the VM's `CallFn` dispatches
/// to, so no kernel can call it with the wrong signature. Its own calls still resolve
/// against `eligible`, i.e. only to capture-free functions, so there is no transitive
/// capture set to close over.
fn tail_loop_captures<'a>(
    funcs: &[FnDef<'a>],
    eligible: &HashSet<&'a str>,
    kind: NumKind,
) -> Vec<(&'a str, Vec<&'a str>)> {
    let tail_loop = tail_loopable_set(funcs);
    let mut out = Vec::new();
    for f in funcs {
        // Only loops, and only ones the plain analysis already rejected — a function
        // that compiled without captures keeps its existing, cheaper entry point.
        if !tail_loop.contains(f.name) || eligible.contains(f.name) || f.params.len() > MAX_ARITY {
            continue;
        }
        let params: HashSet<&str> = f.params.iter().map(|(n, _)| n.as_str()).collect();
        let mut caps = Vec::new();
        free_idents(f.body, &params, &mut caps);
        // No free names → it was rejected for some other reason (a `/`, a Float, an
        // ineligible callee), and captures cannot rescue it.
        if caps.is_empty() || caps.len() > MAX_CAPTURES {
            continue;
        }
        // A capture that names a user FUNCTION is not a global read — `free_idents` never
        // records a callee, but a function used as a value would be, and it is not an i64.
        if caps.iter().any(|c| funcs.iter().any(|g| g.name == *c)) {
            continue;
        }
        // Now ask the REAL predicate whether the body is eligible once those names are
        // treated as parameters. Same function the capture-free path uses, so the two
        // cannot drift apart on what `i64`-closed means.
        let mut widened = params.clone();
        for c in &caps {
            widened.insert(c);
        }
        // The SELF-call must be treated as eligible while re-checking. `eligible` cannot
        // contain `f` — `f` is here precisely because it was rejected — but `gen_tail`
        // lowers a tail self-call to a parameter rebind and a jump, never to a call
        // instruction, so it needs no entry in `fn_ids` and no compiled callee. Every
        // self-call is in tail position by `tail_loopable_set`, which is what makes that
        // lowering total.
        let mut callable = eligible.clone();
        callable.insert(f.name);
        if value_eligible(f.body, &callable, &widened, kind) {
            out.push((f.name, caps));
        }
    }
    out
}

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

/// A mixed specialization visible to OTHER mixed bodies (declared before any body is
/// defined, so `escape` can call `step`): its Cranelift id, per-param kinds, and result
/// kind (`Int` placeholder for a body whose every path re-loops).
/// A mixed specialization's numeric SIGNATURE — parameter kinds and return kind, with no
/// codegen identity attached. Deliberately id-free so the inference that produces it
/// ([`mixed_fn_sigs`]) needs no `JITModule` and can therefore run at BYTECODE-COMPILE time,
/// where the decision to emit a kernel guard is made. `build` keeps the `FuncId`s in a
/// parallel map (`mixed_ids`), keyed by the same names.
#[derive(Clone)]
struct MixedSig {
    params: Vec<NumKind>,
    ret: NumKind,
}

/// Every user function that gets a MIXED specialization, with its parameter kinds and
/// return kind. Pure over the AST — the twin of [`int_eligible_fns`], and the table the
/// bytecode compiler needs in order to type a call to a `Float`-parameter function inside a
/// map body (it knows only NAMES otherwise). Computed identically here and inside `build`,
/// so the compile-time guard decision matches what the JIT will actually compile.
///
/// Program order matters and is preserved: each accepted signature is visible to LATER
/// functions, so `fn escape(...) = step(...)` sees `step` (the define-before-use rule
/// guarantees callees precede callers).
pub type MixedSigTable = std::collections::HashMap<String, (Vec<NumKind>, NumKind)>;

pub fn mixed_fn_sigs(program: &[Stmt]) -> MixedSigTable {
    let funcs: Vec<FnDef> = program
        .iter()
        .filter_map(|s| match s {
            Stmt::Func { name, params, body, .. } => Some(FnDef { name, params, body }),
            _ => None,
        })
        .collect();
    let int_eligible = eligible_set(&funcs, NumKind::Int);
    let tail_loop = tail_loopable_set(&funcs);
    let recursive = recursive_funcs(&funcs);
    let user_fns: HashSet<&str> = funcs.iter().map(|f| f.name).collect();
    let mut sigs: HashMap<&str, MixedSig> = HashMap::new();
    let mut out = std::collections::HashMap::new();
    for f in &funcs {
        let Some((_, params, ret)) =
            mixed_fn_sig(f, &tail_loop, &recursive, &int_eligible, &sigs, &user_fns)
        else {
            continue;
        };
        // A body whose every path re-loops never returns; `Int` is the same placeholder
        // `build` uses, so the two tables agree.
        let ret = ret.unwrap_or(NumKind::Int);
        sigs.insert(f.name, MixedSig { params: params.clone(), ret });
        out.insert(f.name.to_string(), (params, ret));
    }
    out
}

/// Bottom-up kind of an expression over a **typed environment** (parameter and `let`
/// binder kinds), or `None` if anything falls outside the mixed-eligible shape. The
/// env-generalization of [`infer_mixed_kind`] (same operator/builtin/promotion rules,
/// mirrored EXACTLY by [`gen_value_env`]): `+`/`-`/`*` promote `Int` operands to `f64`
/// when the other side is `Float` (the interpreter's numeric promotion); `%`/`//`/
/// bitwise/const-shifts stay `Int`-only under `value_eligible`'s constant constraints;
/// `sqrt` is always `Float`, `abs` preserves, `min`/`max` need same-kind operands.
/// `/` is admitted ONLY with a nonzero `Float`-literal divisor (both sides promote to
/// f64; the interpreter's `/` always yields Float, and a literal divisor can never
/// raise its /0 error, so native `fdiv` is bit-exact with no poison obligation).
/// Calls dispatch in priority order: a MIXED sibling (`sigs` — arg kinds must EQUAL
/// its param kinds), then any OTHER user function → ineligible (never silently treat a
/// user-shadowed `sqrt` as the builtin — the shadowing hole the map path already
/// guards with `user_fns`), then the inline builtins. No `let`/`if` in VALUE position
/// (tail positions handle those).
fn infer_typed_env(
    e: &Expr,
    env: &HashMap<&str, NumKind>,
    sigs: &HashMap<&str, MixedSig>,
    user_fns: &HashSet<&str>,
) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Ident { name, .. } => env.get(name.as_str()).copied(),
        Expr::Binary { op, left, right, .. } => {
            let lk = infer_typed_env(left, env, sigs, user_fns)?;
            let rk = infer_typed_env(right, env, sigs, user_fns)?;
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => {
                    Some(if lk == NumKind::Float || rk == NumKind::Float {
                        NumKind::Float
                    } else {
                        NumKind::Int
                    })
                }
                // `/` is always float division and always yields Float, for ANY eligible
                // divisor. This was literal-only (`Expr::Float(d) if d != 0.0`) — and that
                // single restriction was k2's entire 5.3×: `row`'s `2.7 / to_float(g)`
                // declined the whole function to the VM, costing ~250 ns of dispatch per
                // pixel around a native `step` (0.39s against 0.07s with the reciprocal
                // hoisted). A zero divisor now bails IMMEDIATELY to the poison block
                // (`gen_value_env`'s Div arm) — the same rule as the NaN-compare bail, and
                // for the same reason: a tail loop can be infinite, so the interpreter's
                // `/0` error cannot wait for an accumulate-and-store. The VM then discards
                // the result and re-runs on bytecode, raising the exact error.
                BinOp::Div => Some(NumKind::Float),
                // Any `Int` divisor, literal or not. A zero divisor — and the
                // `(i64::MIN, -1)` pair, which does not raise but WRAPS where native
                // `srem`/`sdiv` would trap — bail to the poison block, exactly like the
                // `/` arm below and for the same reason. Naming a modulus used to cost
                // 17-110x: `MOD = 1000000007` then `% MOD` declined the whole enclosing
                // kernel, and a divisor arriving from data had no fast spelling at all.
                BinOp::Mod | BinOp::FloorDiv => {
                    (lk == NumKind::Int && rk == NumKind::Int).then_some(NumKind::Int)
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    (lk == NumKind::Int && rk == NumKind::Int).then_some(NumKind::Int)
                }
                // Any `Int` shift count; one outside `0..=63` bails, since the
                // interpreter raises there and a native shift is undefined.
                BinOp::Shl | BinOp::Shr => {
                    (lk == NumKind::Int && rk == NumKind::Int).then_some(NumKind::Int)
                }
                _ => None,
            }
        }
        Expr::Unary { op: UnOp::Neg, expr, .. } => infer_typed_env(expr, env, sigs, user_fns),
        Expr::Call { name, args, .. } => {
            if let Some(sig) = sigs.get(name.as_str()) {
                // A mixed sibling: strict per-param kind equality (no promotion — the
                // callee's specialization is compiled for exactly these kinds).
                if args.len() != sig.params.len() {
                    return None;
                }
                for (a, &k) in args.iter().zip(&sig.params) {
                    if infer_typed_env(a, env, sigs, user_fns)? != k {
                        return None;
                    }
                }
                return Some(sig.ret);
            }
            if user_fns.contains(name.as_str()) {
                // A user function without a mixed form (or shadowing a builtin name):
                // not lowerable — never treat it as the inline builtin.
                return None;
            }
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_typed_env(&args[0], env, sigs, user_fns)?;
                    Some(NumKind::Float)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_typed_env(&args[0], env, sigs, user_fns)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_typed_env(&args[0], env, sigs, user_fns),
                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                ("to_int" | "sign", 1) => {
                    infer_typed_env(&args[0], env, sigs, user_fns)?;
                    Some(NumKind::Int)
                }
                ("min" | "max", 2) => {
                    let ka = infer_typed_env(&args[0], env, sigs, user_fns)?;
                    let kb = infer_typed_env(&args[1], env, sigs, user_fns)?;
                    (ka == kb).then_some(ka)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// True iff `e` is a mixed-eligible condition: `and`/`or` over comparisons whose two
/// sides infer to the SAME kind (an `Int`-vs-`Float` comparison is rejected — its
/// promotion semantics past 2^53 are not provably identical to the interpreter's).
/// Mirrored exactly by [`gen_cond_env`].
fn cond_typed_ok(
    e: &Expr,
    env: &HashMap<&str, NumKind>,
    sigs: &HashMap<&str, MixedSig>,
    user_fns: &HashSet<&str>,
) -> bool {
    match e {
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_typed_ok(left, env, sigs, user_fns) && cond_typed_ok(right, env, sigs, user_fns)
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne)
                && match (
                    infer_typed_env(left, env, sigs, user_fns),
                    infer_typed_env(right, env, sigs, user_fns),
                ) {
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
    sigs: &HashMap<&str, MixedSig>,
    user_fns: &HashSet<&str>,
) -> Option<Option<NumKind>> {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => {
            if !cond_typed_ok(cond, env, sigs, user_fns) {
                return None;
            }
            let a = mixed_tail_ret_kind(then_branch, env, self_name, param_kinds, sigs, user_fns)?;
            let b = mixed_tail_ret_kind(else_branch, env, self_name, param_kinds, sigs, user_fns)?;
            match (a, b) {
                (None, x) | (x, None) => Some(x),
                (Some(k1), Some(k2)) if k1 == k2 => Some(Some(k1)),
                _ => None,
            }
        }
        Expr::Let { bindings, body } => {
            let mut saved: Vec<(&'a str, Option<NumKind>)> = Vec::new();
            for (n, v) in bindings {
                let k = infer_typed_env(v, env, sigs, user_fns)?;
                saved.push((n.as_str(), env.insert(n.as_str(), k)));
            }
            let r = mixed_tail_ret_kind(body, env, self_name, param_kinds, sigs, user_fns);
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
                if infer_typed_env(a, env, sigs, user_fns)? != k {
                    return None;
                }
            }
            Some(None)
        }
        other => infer_typed_env(other, env, sigs, user_fns).map(Some),
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
/// Does this subtree force `f64` evaluation? A `Float` literal, a float-returning builtin, a
/// division (never `i64`-closed — see `value_eligible_cap`), or a parameter already known to be
/// `Float`. Used only to PROPOSE kinds in [`infer_param_kinds`]; the proposal is then validated,
/// so a wrong answer here costs a missed specialization, never a wrong one.
fn subtree_forces_float(e: &Expr, float_params: &HashSet<&str>) -> bool {
    match e {
        Expr::Float(_) => true,
        Expr::Int(_) => false,
        Expr::Ident { name, .. } => float_params.contains(name.as_str()),
        Expr::Call { name, args, .. } => {
            matches!(name.as_str(), "sqrt" | "to_float")
                || args.iter().any(|a| subtree_forces_float(a, float_params))
        }
        Expr::Binary { op: BinOp::Div, .. } => true,
        Expr::Binary { left, right, .. } => {
            subtree_forces_float(left, float_params) || subtree_forces_float(right, float_params)
        }
        Expr::Unary { expr, .. } => subtree_forces_float(expr, float_params),
        Expr::If { cond, then_branch, else_branch, .. } => {
            subtree_forces_float(cond, float_params)
                || subtree_forces_float(then_branch, float_params)
                || subtree_forces_float(else_branch, float_params)
        }
        _ => false,
    }
}

/// Collect the parameter names occurring anywhere in `e`.
fn params_in<'a>(e: &'a Expr, params: &HashSet<&'a str>, out: &mut HashSet<&'a str>) {
    match e {
        Expr::Ident { name, .. } => {
            if params.contains(name.as_str()) {
                out.insert(name.as_str());
            }
        }
        Expr::Binary { left, right, .. } => {
            params_in(left, params, out);
            params_in(right, params, out);
        }
        Expr::Unary { expr, .. } => params_in(expr, params, out),
        Expr::Call { args, .. } => args.iter().for_each(|a| params_in(a, params, out)),
        Expr::If { cond, then_branch, else_branch, .. } => {
            params_in(cond, params, out);
            params_in(then_branch, params, out);
            params_in(else_branch, params, out);
        }
        Expr::Index { recv, index, .. } => {
            params_in(recv, params, out);
            params_in(index, params, out);
        }
        _ => {}
    }
}

/// Walk `e` marking parameters `Float` (float taint) and `Int` (used by an `i64`-closed operator
/// or as an index). Returns `false` on a CONTRADICTION — a parameter with both kinds of evidence —
/// so the caller declines rather than guessing.
fn gather_kind_evidence<'a>(
    e: &'a Expr,
    self_name: &str,
    params: &HashSet<&'a str>,
    order: &[&'a str],
    float: &mut HashSet<&'a str>,
    int: &mut HashSet<&'a str>,
) -> bool {
    match e {
        // A self-call ties argument j to parameter j — the strongest signal in a
        // tail-recursive function, and the shape this exists for.
        Expr::Call { name, args, .. } if name == self_name && args.len() == order.len() => {
            for (j, a) in args.iter().enumerate() {
                if subtree_forces_float(a, float) {
                    float.insert(order[j]);
                }
                if !gather_kind_evidence(a, self_name, params, order, float, int) {
                    return false;
                }
            }
        }
        // `%`, `//`, bitwise and shifts are `i64`-closed: their operands are integers.
        Expr::Binary {
            op:
                BinOp::Mod
                | BinOp::FloorDiv
                | BinOp::BitAnd
                | BinOp::BitOr
                | BinOp::BitXor
                | BinOp::Shl
                | BinOp::Shr,
            left,
            right,
            ..
        } => {
            let mut here = HashSet::new();
            params_in(left, params, &mut here);
            params_in(right, params, &mut here);
            int.extend(here);
            if !gather_kind_evidence(left, self_name, params, order, float, int)
                || !gather_kind_evidence(right, self_name, params, order, float, int)
            {
                return false;
            }
        }
        // A COMPARISON ties its two sides to the same kind in practice — the loop-bound idiom
        // `i >= lim` is how a float counter's limit gets its type, and without this the limit
        // infers `Int`, the mask mismatches at dispatch, and the whole function silently falls
        // back (correct, but 60× slower). Same proposal-not-proof status as the rest.
        Expr::Binary {
            op: BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne,
            left,
            right,
            ..
        } => {
            if subtree_forces_float(left, float) || subtree_forces_float(right, float) {
                let mut here = HashSet::new();
                params_in(left, params, &mut here);
                params_in(right, params, &mut here);
                float.extend(here);
            }
            if !gather_kind_evidence(left, self_name, params, order, float, int)
                || !gather_kind_evidence(right, self_name, params, order, float, int)
            {
                return false;
            }
        }
        // `+ - * /` mixing a parameter with anything float-forcing makes that parameter float.
        Expr::Binary {
            op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div, left, right, ..
        } => {
            if subtree_forces_float(e, float) {
                let mut here = HashSet::new();
                params_in(left, params, &mut here);
                params_in(right, params, &mut here);
                float.extend(here);
            }
            if !gather_kind_evidence(left, self_name, params, order, float, int)
                || !gather_kind_evidence(right, self_name, params, order, float, int)
            {
                return false;
            }
        }
        // An index is an integer.
        Expr::Index { recv, index, .. } => {
            let mut here = HashSet::new();
            params_in(index, params, &mut here);
            int.extend(here);
            if !gather_kind_evidence(recv, self_name, params, order, float, int)
                || !gather_kind_evidence(index, self_name, params, order, float, int)
            {
                return false;
            }
        }
        Expr::Binary { left, right, .. } => {
            if !gather_kind_evidence(left, self_name, params, order, float, int)
                || !gather_kind_evidence(right, self_name, params, order, float, int)
            {
                return false;
            }
        }
        Expr::Unary { expr, .. } => {
            if !gather_kind_evidence(expr, self_name, params, order, float, int) {
                return false;
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                if !gather_kind_evidence(a, self_name, params, order, float, int) {
                    return false;
                }
            }
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            for b in [cond, then_branch, else_branch] {
                if !gather_kind_evidence(b, self_name, params, order, float, int) {
                    return false;
                }
            }
        }
        _ => {}
    }
    // A parameter cannot be both an `i64`-closed operand and float-tainted.
    !float.iter().any(|p| int.contains(p))
}

/// Propose a numeric kind per parameter when the source did not annotate them.
///
/// WHY THIS EXISTS. The mixed specialization is the sound successor to the removed blanket-`f64`
/// function spec (see the note above `let kind = NumKind::Int` in [`build`]): it tracks each
/// parameter's kind AND derives the exact return kind, so it cannot diverge from the interpreter
/// on result type the way blanket `f64` codegen did. But it was reachable only through explicit
/// `: Int` / `: Float` annotations — so an ORDINARY numeric loop, whose natural shape is float
/// state plus an integer counter, never reached native code at all. Measured: `fn spin(zr, zi, i,
/// n)` ran 0.72s where the identical annotated body ran 0.01s, a **72×** cliff with `JIT ≈ NOJIT`
/// (i.e. it never compiled), and the same cliff hit all-`Float` recursion too.
///
/// WHY A PROPOSAL IS ENOUGH — this needs to be plausible, not sound, because two independent
/// validators already stand behind it:
/// 1. [`mixed_tail_ret_kind`] re-types the whole body under the proposed kinds and returns `None`
///    if anything fails to check, so a body that does not fit the proposal is never compiled.
/// 2. The VM re-tests every ARGUMENT's runtime type against `float_mask` before dispatching to
///    the specialization (`vm.rs`, `Op::CallFn`), so a specialization built on a wrong guess is
///    simply never called — the ordinary bytecode path runs and the result is unchanged.
///
/// So the cost of a bad proposal is a few microseconds of wasted JIT time, never a wrong answer.
/// A parameter with contradictory evidence (used both as an `i64`-closed operand and float-tainted)
/// declines the whole function rather than picking a side. Unresolved parameters default to `Int`;
/// if that makes the signature all-`Int`, [`mixed_fn_sig`]'s existing `int_eligible` check drops
/// it so the plain `i64` loop keeps the function.
fn infer_param_kinds<'a>(f: &'a FnDef) -> Option<Vec<NumKind>> {
    let order: Vec<&'a str> = f.params.iter().map(|(n, _)| n.as_str()).collect();
    let params: HashSet<&'a str> = order.iter().copied().collect();
    if params.len() != order.len() {
        return None; // duplicate parameter names — not a shape to reason about
    }
    let mut float: HashSet<&'a str> = HashSet::new();
    let mut int: HashSet<&'a str> = HashSet::new();
    // Seed from whatever WAS annotated, so a partly-annotated signature is honoured exactly.
    for (n, ann) in f.params {
        match ann {
            Some(TypeAnn::Float) => {
                float.insert(n.as_str());
            }
            Some(TypeAnn::Int) => {
                int.insert(n.as_str());
            }
            _ => {}
        }
    }
    // Float taint propagates (a param becomes Float, which makes its neighbours Float), so
    // iterate to a fixpoint. Bounded by the parameter count: each round either grows `float` or
    // stops.
    for _ in 0..=order.len() {
        let before = float.len() + int.len();
        if !gather_kind_evidence(f.body, f.name, &params, &order, &mut float, &mut int) {
            return None;
        }
        if float.len() + int.len() == before {
            break;
        }
    }
    Some(
        order
            .iter()
            .map(|n| if float.contains(n) { NumKind::Float } else { NumKind::Int })
            .collect(),
    )
}

fn mixed_fn_sig(
    f: &FnDef,
    tail_loop: &HashSet<&str>,
    recursive: &HashSet<&str>,
    int_eligible: &HashSet<&str>,
    sigs: &HashMap<&str, MixedSig>,
    user_fns: &HashSet<&str>,
) -> Option<(u16, Vec<NumKind>, Option<NumKind>)> {
    // Recursive functions qualify only in the tail-loopable shape; NON-recursive ones
    // compile straight-line with the same walker (no self-call arm ever fires) — the
    // `fn escape(px: Int, py: Int) = step(…)` wrapper shape.
    if recursive.contains(f.name) && !tail_loop.contains(f.name) {
        return None;
    }
    if f.params.is_empty() || f.params.len() > MAX_ARITY {
        return None;
    }
    // Annotations win where present; anything unannotated gets an INFERRED kind, so an ordinary
    // numeric loop (`fn spin(zr, zi, i, n)`) reaches this specialization instead of falling to
    // the per-element VM — a measured 72× cliff. The proposal is validated by
    // `mixed_tail_ret_kind` below and again by the VM's per-argument type test at dispatch, so a
    // wrong inference costs a never-used specialization, not a wrong result. See
    // [`infer_param_kinds`].
    let inferred = if f.params.iter().any(|(_, a)| a.is_none()) {
        Some(infer_param_kinds(f)?)
    } else {
        None
    };
    let mut kinds = Vec::with_capacity(f.params.len());
    let mut mask: u16 = 0;
    for (j, (_, ann)) in f.params.iter().enumerate() {
        let k = match ann {
            Some(TypeAnn::Int) => NumKind::Int,
            Some(TypeAnn::Float) => NumKind::Float,
            Some(_) => return None, // a non-numeric annotation is not this specialization's shape
            None => inferred.as_ref()?[j],
        };
        if matches!(k, NumKind::Float) {
            mask |= 1 << j;
        }
        kinds.push(k);
    }
    if mask == 0 && int_eligible.contains(f.name) {
        // THE GENERIC HELPER. `fn sq(x) = x * x` is the shape every library author writes,
        // and until now it got NO mixed specialization at all — so every FLOAT call site
        // silently declined and took the enclosing map down with it:
        //
        //     fn sq(x) = x * x            , Float call:  0.967s jit / 0.944s nojit — declines
        //     fn sq(x: Float) -> Float    , Float call:  0.019s                      52x
        //     fn sq(x) = x * x * 1.0      , Float call:  0.018s                     132x
        //     fn sq(x) = x * x            , Int   call:  0.026s                      30x
        //
        // The reasoning that used to end here — "the plain i64 loop already covers an
        // all-Int, i64-closed function, so a mixed duplicate would never be dispatched" — is
        // true about calling `sq` DIRECTLY, and false about a call to it from inside a
        // kernel. `infer_param_kinds` reads the function's OWN BODY only, never its call
        // sites, so a kind-agnostic body like `x * x` yields Int by default rather than by
        // evidence, and the Float reading was simply never built.
        //
        // The two specializations do not compete, because they live in DIFFERENT tables: the
        // i64 one in `fn_ids`, this one in `msigs`/`mixed_ids`. Emitting the Float reading
        // fills an empty slot rather than shadowing anything, and dispatch stays exact — the
        // VM type-tests every argument, so an `Int` argument still takes the i64 path and
        // still gets the interpreter's WRAPPING i64 arithmetic. That matters: `x * x` on
        // 2^53+1 is an exact wrapping multiply in the interpreter and a lossy f64 one here,
        // and it is the per-argument test, not this function, that keeps them apart.
        //
        // ONLY when every parameter is UNANNOTATED. A written `Int` is evidence; an absent
        // annotation is not, and promoting a partly-annotated signature would overrule
        // something the author actually said.
        //
        // A NAME THAT SHADOWS A BUILTIN IS NO LONGER EXCLUDED. It used to be, because
        // `mixed_fn_sigs` is derived from the whole AST and has no notion of definition
        // ORDER while the engines resolved in source order — so promoting `fn round(x) = 99`
        // applied the user's function to the call sites ABOVE it and printed
        // `[99, 99, 99, 99]` against `[1, 2, 3, 4]`. ADR 0027 removed the premise: a
        // top-level `fn` is file-scoped, so an order-blind analysis is now simply CORRECT
        // about these names rather than needing to be kept away from them. This is one of
        // the three guards that decision was taken to delete.
        if f.params.iter().all(|(_, a)| a.is_none()) {
            let fkinds = vec![NumKind::Float; f.params.len()];
            let mut fenv: HashMap<&str, NumKind> =
                f.params.iter().map(|(n, _)| (n.as_str(), NumKind::Float)).collect();
            // A body that cannot be read as f64 declines here exactly as it would have
            // before — this adds a reading, it does not weaken one.
            if let Some(fret) =
                mixed_tail_ret_kind(f.body, &mut fenv, f.name, &fkinds, sigs, user_fns)
            {
                let all_float: u16 = ((1u32 << f.params.len()) - 1) as u16;
                return Some((all_float, fkinds, fret));
            }
        }
        return None;
    }
    let mut env: HashMap<&str, NumKind> =
        f.params.iter().zip(&kinds).map(|((n, _), &k)| (n.as_str(), k)).collect();
    let ret = mixed_tail_ret_kind(f.body, &mut env, f.name, &kinds, sigs, user_fns)?;
    Some((mask, kinds, ret))
}

/// Pure scalar builtins the `i64` kernel codegen emits inline, matching the interpreter
/// bit-for-bit: `abs` is `wrapping_abs` (Cranelift `iabs`, which wraps `i64::MIN` to
/// itself); `min`/`max` reproduce the interpreter's `as_f64()`-compare-then-return-the-
/// original-operand semantics (so they agree even past 2^53, where a native integer
/// compare would differ). Added to the JIT-eligible set only when no user function of the
/// same name shadows them (then the call dispatches to the user's function instead).
pub const JIT_SCALAR_BUILTINS: &[(&str, usize)] =
    &[("abs", 1), ("min", 2), ("max", 2), ("to_int", 1), ("sign", 1)];

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
const JIT_FLOAT_BUILTINS: &[(&str, usize)] =
    &[("sqrt", 1), ("abs", 1), ("min", 2), ("max", 2), ("to_float", 1)];

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
        // Unary negation: the interpreter is `wrapping_neg` on Int / `-f` on Float —
        // exactly native `ineg`/`fneg`. (Without this arm every NEGATIVE LITERAL, which
        // parses as `Neg(lit)`, silently disqualified its whole kernel.)
        Expr::Unary { op: UnOp::Neg, expr, .. } => value_eligible(expr, eligible, locals, kind),
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
            binders: &binders,
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
                    binders: &binders,
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
        Expr::Let { bindings, body } => {
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

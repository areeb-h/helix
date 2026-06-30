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
//! SAFETY: calling generated code is inherently `unsafe`. The two `unsafe` blocks
//! are confined to [`call_i64`]/[`call_f64`], guarded by the VM's type/arity
//! check so the native ABI contract always holds. The JIT deals only in scalar
//! `i64`/`f64` — no heap, no `Rc` — so it adds no leak surface.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F64, I64};
use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlags, Type, Value as ClValue};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::ast::{BinOp, Expr, Stmt, TypeAnn};

const MAX_ARITY: usize = 4;

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

/// The native entry points for one user function (whichever specializations
/// compiled), plus its arity. `Copy` so the VM can pull it out cheaply.
#[derive(Clone, Copy)]
pub struct NativeFn {
    pub i64_ptr: Option<*const u8>,
    pub f64_ptr: Option<*const u8>,
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

            let ret = gen_value(&mut builder, f.body, &mut vars, &fn_ids, &mut module, kind);
            builder.ins().return_(&[ret]);
            builder.finalize();

            module.define_function(fn_ids[f.name], &mut ctx).ok()?;
            module.clear_context(&mut ctx);

            compiled.push((f.name.to_string(), kind, fn_ids[f.name], f.params.len()));
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
            if !reduce_bodies_eligible(rl, &int_eligible) {
                reduce_ids.push(None);
                continue;
            }
            // Both shapes take 3 `i64` params (start, end, and `init` for a scalar acc or
            // an `acc_ptr` for a tuple acc); a scalar returns the accumulator, a tuple
            // writes its slots back through the pointer (no return).
            let mut sig = module.make_signature();
            sig.call_conv = CallConv::SystemV;
            for _ in 0..3 {
                sig.params.push(AbiParam::new(I64));
            }
            if rl.bodies.len() == 1 {
                sig.returns.push(AbiParam::new(I64));
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
    let fused_ids = define_fused_kernels(&mut module, fused_kernels, &fn_ids, &int_eligible);

    if compiled.is_empty()
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
            .or_insert(NativeFn { i64_ptr: None, f64_ptr: None, arity });
        match kind {
            NumKind::Int => entry.i64_ptr = Some(ptr),
            NumKind::Float => entry.f64_ptr = Some(ptr),
        }
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
fn fusion_eligible(k: &crate::bytecode::FusedKernel, fns: &HashSet<&str>) -> bool {
    use crate::bytecode::{FusionSink, FusionStage};
    k.stages.iter().all(|s| match s {
        FusionStage::Map { binder, body } => map_kernel_eligible(body, binder, fns),
        FusionStage::Filter { binder, body } => filter_kernel_eligible(body, binder, fns),
    }) && match &k.sink {
        FusionSink::Collect | FusionSink::Count => true,
        FusionSink::Reduce { pa, pb, bodies } => bodies_eligible(pa, pb, bodies, fns),
    }
}

/// Declare + define every fuseable pipeline kernel (one slot each, `None` if declined).
fn define_fused_kernels(
    module: &mut JITModule,
    kernels: &[crate::bytecode::FusedKernel],
    fn_ids: &HashMap<&str, FuncId>,
    eligible: &HashSet<&str>,
) -> Vec<Option<FuncId>> {
    let mut ids: Vec<Option<FuncId>> = Vec::with_capacity(kernels.len());
    let mut ctx = module.make_context();
    let mut bctx = FunctionBuilderContext::new();
    for (i, k) in kernels.iter().enumerate() {
        if !fusion_eligible(k, eligible) {
            ids.push(None);
            continue;
        }
        let mut sig = module.make_signature();
        sig.call_conv = CallConv::SystemV;
        for _ in 0..3 {
            sig.params.push(AbiParam::new(I64));
        }
        // A tuple reduce writes its slots through `acc_ptr` (no return); see
        // `define_fused_kernel`.
        let tuple_reduce = matches!(&k.sink,
            crate::bytecode::FusionSink::Reduce { bodies, .. } if bodies.len() > 1);
        if !tuple_reduce {
            sig.returns.push(AbiParam::new(I64));
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
/// variables is still eligible — each free `i64` variable is recorded (in first-appearance
/// order) and passed to the kernel as a loop-invariant `caps[i]`. This is the nested-fold
/// case: an inner `range(..).reduce(..)` whose body reads the outer `map` variable. Returns
/// the ordered captures (possibly empty), or `None` if the body is ineligible or captures
/// more than [`MAX_CAPTURES`]. Mirrors [`map_kernel_captures`]; same i64-closed rules.
pub fn reduce_loop_captures(body: &Expr, pa: &str, pb: &str, fns: &HashSet<&str>) -> Option<Vec<String>> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(pa);
    locals.insert(pb);
    let mut caps: Vec<String> = Vec::new();
    if value_eligible_cap(body, fns, &locals, &mut caps) && caps.len() <= MAX_CAPTURES {
        Some(caps)
    } else {
        None
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

fn reduce_bodies_eligible(rl: &crate::bytecode::ReduceLoop, fns: &HashSet<&str>) -> bool {
    // A scalar captured body is eligible over `{pa, pb} ∪ captures` — exactly what
    // `define_reduce_loop` binds (the captures are loop-invariant `i64` locals loaded from
    // the `caps` pointer). This must match the codegen's variable set, or the build would
    // compile a loop the VM can't safely take (or skip one it could).
    if rl.bodies.len() == 1 && !rl.captures.is_empty() {
        let mut locals: HashSet<&str> = HashSet::new();
        locals.insert(rl.pa.as_str());
        locals.insert(rl.pb.as_str());
        for c in &rl.captures {
            locals.insert(c.as_str());
        }
        return value_eligible(&rl.bodies[0], fns, &locals, NumKind::Int);
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
    // Recursive functions run on the depth-guarded VM (or are memoized) instead.
    let recursive = recursive_funcs(funcs);
    let mut eligible: HashSet<&str> = funcs
        .iter()
        .filter(|f| f.params.len() <= MAX_ARITY && !recursive.contains(f.name))
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
        _ => false,
    }
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
    // A scalar body may capture loop-invariant outer `i64` values, passed via a 4th
    // pointer param `caps` (the nested-fold case). Tuple accumulators don't capture.
    let has_caps = scalar && !rl.captures.is_empty();
    ctx.func.signature.call_conv = CallConv::SystemV;
    for _ in 0..3 {
        ctx.func.signature.params.push(AbiParam::new(I64));
    }
    if has_caps {
        ctx.func.signature.params.push(AbiParam::new(I64)); // caps: *const i64
    }
    if scalar {
        ctx.func.signature.returns.push(AbiParam::new(I64));
    }

    let mut b = FunctionBuilder::new(&mut ctx.func, bctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let start = b.block_params(entry)[0];
    let end = b.block_params(entry)[1];
    let third = b.block_params(entry)[2]; // scalar: init value; tuple: acc slot pointer

    let x_var = b.declare_var(I64);
    let end_var = b.declare_var(I64);
    b.def_var(x_var, start);
    b.def_var(end_var, end);

    // One register per accumulator slot. Scalar seeds slot 0 with `init`; tuple loads
    // each slot from `acc_ptr[k]`.
    let acc_vars: Vec<Variable> = (0..n).map(|_| b.declare_var(I64)).collect();
    if scalar {
        b.def_var(acc_vars[0], third);
    } else {
        for (k, &v) in acc_vars.iter().enumerate() {
            let loaded = b.ins().load(I64, MemFlags::trusted(), third, (k * 8) as i32);
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
    // Bind each captured variable to its (loop-invariant) loaded value.
    for (j, capname) in rl.captures.iter().enumerate() {
        if let Some(&cv) = cap_vars.get(j) {
            vars.insert(capname.as_str(), cv);
        }
    }
    // Compute every component from the OLD slot values, then assign — so a component that
    // reads another slot (`(a[0] + x, a[1] + a[0])`) sees the pre-update value.
    let mut new_vals: Vec<ClValue> = Vec::with_capacity(n);
    for body in &rl.bodies {
        let v = gen_value(&mut b, body, &mut vars, fn_ids, module, NumKind::Int);
        new_vals.push(v);
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
    // Bind each captured variable to `caps[i]` — loop-invariant, so the loads are
    // trivially hoistable; correctness only needs the right slot per name. (`caps[i]`
    // is `i64` for an `Int` kernel, `f64` for a `Float` one — the VM coerces to match.)
    if let Some(cp) = caps_ptr {
        let caps_var = b.declare_var(I64);
        b.def_var(caps_var, cp);
        for (j, cname) in k.captures.iter().enumerate() {
            let base = b.use_var(caps_var);
            let off = b.ins().iconst(I64, (j * 8) as i64);
            let addr = b.ins().iadd(base, off);
            let v = b.ins().load(elem_ty, MemFlags::trusted(), addr, 0);
            let cvar = b.declare_var(elem_ty);
            b.def_var(cvar, v);
            vars.insert(cname.as_str(), cvar);
        }
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

    ctx.func.signature.call_conv = CallConv::SystemV;
    for _ in 0..3 {
        ctx.func.signature.params.push(AbiParam::new(I64));
    }
    // Scalar reduce / collect / count return one `i64` (accumulator or kept count); a
    // tuple reduce instead writes its N slots back through the `acc_ptr` (param 3).
    if !tuple_reduce {
        ctx.func.signature.returns.push(AbiParam::new(I64));
    }

    let mut b = FunctionBuilder::new(&mut ctx.func, bctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let (p0, p1, p2) = (b.block_params(entry)[0], b.block_params(entry)[1], b.block_params(entry)[2]);

    let idx_var = b.declare_var(I64); // read cursor (array index `i`, or range counter `x`)
    let limit_var = b.declare_var(I64);
    let sink_var = b.declare_var(I64); // scalar accumulator / write cursor `w` / counter
    let src_var = b.declare_var(I64);
    let dst_var = b.declare_var(I64);
    // A tuple reduce keeps its N slots in their own registers (loaded from `acc_ptr`).
    let acc_vars: Vec<Variable> = (0..if tuple_reduce { n_acc } else { 0 })
        .map(|_| b.declare_var(I64))
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
            let loaded = b.ins().load(I64, MemFlags::trusted(), p2, (k2 * 8) as i32);
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
        b.ins().load(I64, MemFlags::trusted(), addr, 0)
    };
    let cur_var = b.declare_var(I64);
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
        FusionSink::Reduce { pa, pb, bodies } => {
            if tuple_reduce {
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
                let nacc = gen_value(&mut b, &bodies[0], &mut vars, fn_ids, module, NumKind::Int);
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
                let v = match op {
                    BinOp::Add => b.ins().iadd(lv, rv),
                    BinOp::Sub => b.ins().isub(lv, rv),
                    BinOp::Mul => b.ins().imul(lv, rv),
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

/// Call an `i64`-specialized JIT function. SAFETY: see module docs; the VM
/// guarantees `ptr` is a finalized `extern "C" fn(i64×n)->i64` and `args.len()==n`.
pub unsafe fn call_i64(ptr: *const u8, args: &[i64]) -> i64 {
    unsafe {
        match args.len() {
            0 => std::mem::transmute::<*const u8, extern "C" fn() -> i64>(ptr)(),
            1 => std::mem::transmute::<*const u8, extern "C" fn(i64) -> i64>(ptr)(args[0]),
            2 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64) -> i64>(ptr)(args[0], args[1]),
            3 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64) -> i64>(ptr)(
                args[0], args[1], args[2],
            ),
            4 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64, i64) -> i64>(ptr)(
                args[0], args[1], args[2], args[3],
            ),
            _ => unreachable!("JIT arity is capped at {MAX_ARITY}"),
        }
    }
}

/// Call a native reduce loop. SAFETY: the VM guarantees `ptr` is a finalized
/// `extern "C" fn(i64,i64,i64)->i64` produced by [`define_reduce_loop`].
pub unsafe fn call_reduce(ptr: *const u8, start: i64, end: i64, init: i64) -> i64 {
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64) -> i64>(ptr)(start, end, init)
    }
}

/// Call a **captured** scalar reduce loop `fn(start, end, init, caps) -> i64` (the nested-
/// fold kernel). `caps` points to the loop's capture count of loop-invariant `i64`s.
/// SAFETY: the VM guarantees `ptr` is a finalized captured scalar kernel from
/// [`define_reduce_loop`] and `caps` points to at least that many `i64`s.
pub unsafe fn call_reduce_caps(ptr: *const u8, start: i64, end: i64, init: i64, caps: *const i64) -> i64 {
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64, *const i64) -> i64>(ptr)(
            start, end, init, caps,
        )
    }
}

/// Run a native **tuple**-accumulator reduce loop (`define_reduce_loop`'s N-body shape).
/// `acc` points to the N `i64` slots: their initial values on entry, the folded result on
/// return. The caller owns the buffer (its length must equal the loop's accumulator arity).
///
/// # Safety
/// `ptr` must be a tuple reduce kernel and `acc` must point to at least that kernel's slot
/// count of writable `i64`s.
pub unsafe fn call_tuple_reduce(ptr: *const u8, start: i64, end: i64, acc: *mut i64) {
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, *mut i64)>(ptr)(start, end, acc)
    }
}

/// Run a native map kernel over `src`, returning the mapped buffer (same length, same
/// order). SAFETY: `ptr` is a finalized `extern "C" fn(*const i64,*mut i64,i64)` from
/// [`define_array_kernel`].
pub unsafe fn run_map_kernel(ptr: *const u8, src: &[i64], caps: &[i64]) -> Vec<i64> {
    let mut dst = vec![0i64; src.len()];
    if !src.is_empty() {
        let f: extern "C" fn(*const i64, *mut i64, i64, *const i64) =
            unsafe { std::mem::transmute(ptr) };
        // `caps.as_ptr()` is valid even when empty (a capture-free kernel never reads it).
        f(src.as_ptr(), dst.as_mut_ptr(), src.len() as i64, caps.as_ptr());
    }
    dst
}

/// The `f64` map kernel: `dst[i] = body(src[i])` over an `f64` buffer, with `f64`
/// captures. SAFETY: as [`run_map_kernel`], with an `fn(*const f64, *mut f64, i64,
/// *const f64)` contract guaranteed by the VM's `Float`-array + numeric-caps check.
pub unsafe fn run_map_kernel_f64(ptr: *const u8, src: &[f64], caps: &[f64]) -> Vec<f64> {
    let mut dst = vec![0.0f64; src.len()];
    if !src.is_empty() {
        let f: extern "C" fn(*const f64, *mut f64, i64, *const f64) =
            unsafe { std::mem::transmute(ptr) };
        f(src.as_ptr(), dst.as_mut_ptr(), src.len() as i64, caps.as_ptr());
    }
    dst
}

/// The **mixed** map kernel: `dst[i] = body(src[i])` reading an `i64` buffer and writing
/// `f64` (Int source, float body), no captures. SAFETY: as [`run_map_kernel`], with an
/// `fn(*const i64, *mut f64, i64, *const i64)` contract; the caps pointer is a valid
/// (empty) slice the kernel never reads (mixed kernels are capture-free by construction).
pub unsafe fn run_map_kernel_mixed(ptr: *const u8, src: &[i64]) -> Vec<f64> {
    let mut dst = vec![0.0f64; src.len()];
    if !src.is_empty() {
        let f: extern "C" fn(*const i64, *mut f64, i64, *const i64) =
            unsafe { std::mem::transmute(ptr) };
        let no_caps: [i64; 0] = [];
        f(src.as_ptr(), dst.as_mut_ptr(), src.len() as i64, no_caps.as_ptr());
    }
    dst
}

/// Run a native filter kernel over `src`, returning the kept elements in order. SAFETY:
/// `ptr` is a finalized `extern "C" fn(*const i64,*mut i64,i64)->i64` (kept count) from
/// [`define_array_kernel`].
pub unsafe fn run_filter_kernel(ptr: *const u8, src: &[i64]) -> Vec<i64> {
    let mut dst = vec![0i64; src.len()];
    if src.is_empty() {
        return dst;
    }
    let f: extern "C" fn(*const i64, *mut i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    let kept = f(src.as_ptr(), dst.as_mut_ptr(), src.len() as i64);
    dst.truncate(kept as usize);
    dst
}

/// Run a fused `Collect` pipeline over `src` (`fn(src,dst,len)->kept`), returning the
/// surviving elements in order. SAFETY: `ptr` is the matching kernel from
/// [`define_fused_kernel`].
pub unsafe fn run_fused_collect(ptr: *const u8, src: &[i64]) -> Vec<i64> {
    let mut dst = vec![0i64; src.len()];
    if src.is_empty() {
        return dst;
    }
    let f: extern "C" fn(*const i64, *mut i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    let kept = f(src.as_ptr(), dst.as_mut_ptr(), src.len() as i64);
    dst.truncate(kept as usize);
    dst
}

/// Run a fused array→`Reduce` pipeline over `src` (`fn(src,len,init)->acc`). SAFETY: as
/// [`run_fused_collect`].
pub unsafe fn run_fused_reduce(ptr: *const u8, src: &[i64], init: i64) -> i64 {
    let f: extern "C" fn(*const i64, i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    f(src.as_ptr(), src.len() as i64, init)
}

/// Run a fused array→**tuple**-`Reduce` pipeline over `src` (`fn(src, len, acc_ptr)`):
/// `acc` holds the N `i64` slots — initial values in, folded result out.
///
/// # Safety
/// `ptr` must be a tuple fused-reduce kernel and `acc` must point to its slot count of
/// writable `i64`s.
pub unsafe fn run_fused_tuple_reduce(ptr: *const u8, src: &[i64], acc: *mut i64) {
    let f: extern "C" fn(*const i64, i64, *mut i64) = unsafe { std::mem::transmute(ptr) };
    f(src.as_ptr(), src.len() as i64, acc)
}

/// Run a fused array→`Count` pipeline over `src` (`fn(src,len,_)->count`). SAFETY: as
/// [`run_fused_collect`].
pub unsafe fn run_fused_count(ptr: *const u8, src: &[i64]) -> i64 {
    let f: extern "C" fn(*const i64, i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    f(src.as_ptr(), src.len() as i64, 0)
}

/// Call an `f64`-specialized JIT function. SAFETY: as [`call_i64`], with an
/// `extern "C" fn(f64×n)->f64` contract.
pub unsafe fn call_f64(ptr: *const u8, args: &[f64]) -> f64 {
    unsafe {
        match args.len() {
            0 => std::mem::transmute::<*const u8, extern "C" fn() -> f64>(ptr)(),
            1 => std::mem::transmute::<*const u8, extern "C" fn(f64) -> f64>(ptr)(args[0]),
            2 => std::mem::transmute::<*const u8, extern "C" fn(f64, f64) -> f64>(ptr)(args[0], args[1]),
            3 => std::mem::transmute::<*const u8, extern "C" fn(f64, f64, f64) -> f64>(ptr)(
                args[0], args[1], args[2],
            ),
            4 => std::mem::transmute::<*const u8, extern "C" fn(f64, f64, f64, f64) -> f64>(ptr)(
                args[0], args[1], args[2], args[3],
            ),
            _ => unreachable!("JIT arity is capped at {MAX_ARITY}"),
        }
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

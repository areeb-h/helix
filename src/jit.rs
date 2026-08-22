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

use std::collections::HashMap;


use crate::ast::Expr;
use crate::ast::TypeAnn;
#[cfg(feature = "jit")]
use cranelift_jit::JITModule;

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
    #[cfg_attr(not(feature = "jit"), allow(dead_code))]
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
    #[cfg(feature = "jit")]
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

pub(crate) struct FnDef<'a> {
    name: &'a str,
    params: &'a [(String, Option<TypeAnn>)],
    body: &'a Expr,
}

// ---------- the feature seam (ADR 0032) ----------
//
// Everything that TOUCHES Cranelift lives in `codegen` (gated); everything the
// bytecode compiler consults to shape programs lives in `analysis` (ungated) —
// so a build without the JIT compiles IDENTICAL bytecode. The `jit` feature
// changes execution speed, never program shape or output: the same contract as
// `HELIX_NOJIT=1`, decided at build time instead of run time.
mod analysis;
pub use analysis::*;

#[cfg(feature = "jit")]
mod codegen;
#[cfg(feature = "jit")]
pub use codegen::build;

/// The engine-less twin: same signature, always declines — exactly what `build`
/// already does on every non-x86-64-linux target, so the VM's `jit = None` path
/// is proven by the whole existing suite.
#[cfg(not(feature = "jit"))]
pub fn build(
    program: &[crate::ast::Stmt],
    reduce_loops: &[crate::bytecode::ReduceLoop],
    map_kernels: &[crate::bytecode::ArrayKernel],
    filter_kernels: &[crate::bytecode::ArrayKernel],
    fused_kernels: &[crate::bytecode::FusedKernel],
    scan_loops: &[crate::bytecode::ReduceLoop],
) -> Option<Jit> {
    let _ = (program, reduce_loops, map_kernels, filter_kernels, fused_kernels, scan_loops);
    None
}

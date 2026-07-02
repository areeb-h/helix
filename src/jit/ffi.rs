//! The JIT's FFI trampolines — the ONLY `unsafe` surface in the JIT. Each function
//! transmutes a finalized code pointer (`*const u8`) to the matching `extern "C"`
//! function pointer and calls it. Isolated here so the whole unsafe ABI boundary is
//! one auditable file; the rest of `jit.rs` is safe analysis + Cranelift codegen. The
//! VM upholds each call's SAFETY contract (finalized pointer, correct arity and ABI).

use super::MAX_ARITY;

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

/// Call a native **scalar `f64`** range reduce `fn(start, end, init) -> f64`: `acc = init;
/// for x in start..end { acc = body(acc, x) }` with the i64 counter `x` and the f64
/// accumulator folded left-to-right — bit-exact to the interpreter (mixed promotion).
/// SAFETY: the VM guarantees `ptr` is a finalized `float` [`define_reduce_loop`] kernel.
pub unsafe fn call_reduce_f64(ptr: *const u8, start: i64, end: i64, init: f64) -> f64 {
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, f64) -> f64>(ptr)(start, end, init)
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

/// Run a native **scalar `f64`** reduce over a `Float` buffer: `acc = init; for x in src
/// { acc = body(acc, x) }` with `fadd`/`fmul` left-to-right — bit-exact to the interpreter.
/// SAFETY: `ptr` is a finalized `extern "C" fn(*const f64, i64, f64) -> f64` from a
/// `float`-flagged `define_fused_kernel`, guaranteed by the VM's `Floats` source + `Float`
/// init check.
pub unsafe fn run_fused_reduce_f64(ptr: *const u8, src: &[f64], init: f64) -> f64 {
    let f: extern "C" fn(*const f64, i64, f64) -> f64 = unsafe { std::mem::transmute(ptr) };
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

/// Run a fused **f64 tuple**-`Reduce` over a `Float` buffer `src`: the kernel reads `f64`
/// elements and folds N `f64` accumulator slots packed as bit patterns in the `i64` `acc`
/// buffer (initial values in, folded result out — the same memory the VM packs/unpacks via
/// `acc_to_slots_f64`/`rebuild_acc_f64`). SAFETY: as [`run_fused_tuple_reduce`], but the
/// element pointer is `*const f64` and `ptr` is a `float`-flagged tuple kernel.
pub unsafe fn run_fused_tuple_reduce_f64(ptr: *const u8, src: &[f64], acc: *mut i64) {
    let f: extern "C" fn(*const f64, i64, *mut i64) = unsafe { std::mem::transmute(ptr) };
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

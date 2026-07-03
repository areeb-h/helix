//! The JIT's FFI trampolines — the ONLY `unsafe` surface in the JIT. Each function
//! transmutes a finalized code pointer (`*const u8`) to the matching `extern "C"`
//! function pointer and calls it. Isolated here so the whole unsafe ABI boundary is
//! one auditable file; the rest of `jit.rs` is safe analysis + Cranelift codegen. The
//! VM upholds each call's SAFETY contract (finalized pointer, correct arity and ABI).

use super::MAX_ARITY;

// ---- test-only JIT engagement counter ------------------------------------------------
// Every native trampoline below bumps this. The differential fuzzers reset it before a run
// and assert it grew afterward, so a fuzzer can never pass by SILENTLY falling back to the
// bytecode VM (the "engagement ≠ correctness" trap): if the JIT stopped engaging, the
// assertion fails loudly instead of trivially comparing VM == tree-walker. Test-only, so
// there is zero release overhead; incremented once per kernel INVOCATION (not per element).
#[cfg(test)]
thread_local! {
    static NATIVE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
#[inline]
fn note_native_call() {
    NATIVE_CALLS.with(|c| c.set(c.get() + 1));
}
#[cfg(not(test))]
#[inline(always)]
fn note_native_call() {}

/// Number of native JIT trampoline invocations since the last reset (test-only engagement probe).
#[cfg(test)]
pub fn native_call_count() -> u64 {
    NATIVE_CALLS.with(|c| c.get())
}
/// Reset the engagement counter before a fuzzer run (test-only).
#[cfg(test)]
pub fn reset_native_call_count() {
    NATIVE_CALLS.with(|c| c.set(0));
}

/// Call an `i64`-specialized JIT function. SAFETY: see module docs; the VM
/// guarantees `ptr` is a finalized `extern "C" fn(i64×n)->i64` and `args.len()==n`.
pub unsafe fn call_i64(ptr: *const u8, args: &[i64]) -> i64 {
    note_native_call();
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
            5 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64, i64, i64) -> i64>(
                ptr,
            )(args[0], args[1], args[2], args[3], args[4]),
            6 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64>(
                ptr,
            )(args[0], args[1], args[2], args[3], args[4], args[5]),
            // MAX_ARITY user args + the mixed specialization's trailing poison-pointer
            // slot (see `MixedFn`) — the only way a 7-slot call arises.
            7 => std::mem::transmute::<
                *const u8,
                extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64,
            >(ptr)(args[0], args[1], args[2], args[3], args[4], args[5], args[6]),
            _ => unreachable!("JIT arity is capped at {MAX_ARITY} (+1 poison slot)"),
        }
    }
}

/// Call a native reduce loop. SAFETY: the VM guarantees `ptr` is a finalized
/// `extern "C" fn(i64,i64,i64)->i64` produced by [`define_reduce_loop`].
pub unsafe fn call_reduce(ptr: *const u8, start: i64, end: i64, init: i64) -> i64 {
    note_native_call();
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64) -> i64>(ptr)(start, end, init)
    }
}

/// Call a native **scalar `f64`** range reduce `fn(start, end, init) -> f64`: `acc = init;
/// for x in start..end { acc = body(acc, x) }` with the i64 counter `x` and the f64
/// accumulator folded left-to-right — bit-exact to the interpreter (mixed promotion).
/// SAFETY: the VM guarantees `ptr` is a finalized `float` [`define_reduce_loop`] kernel.
pub unsafe fn call_reduce_f64(ptr: *const u8, start: i64, end: i64, init: f64) -> f64 {
    note_native_call();
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, f64) -> f64>(ptr)(start, end, init)
    }
}

/// Call a scalar `f64` range reduce whose body **divides** — `fn(start, end, init, *mut i8) -> f64`.
/// `*poison` is set non-zero iff some iteration divided by zero (where the interpreter raises), so
/// the VM discards the result and falls back to the exact-erroring bytecode loop. When it stays
/// zero the fold is bit-exact to the interpreter (no `/0` occurred).
/// SAFETY: the VM guarantees `ptr` is a finalized dividing `float` scalar [`define_reduce_loop`]
/// kernel and `poison` points to a writable `i8`.
pub unsafe fn call_reduce_f64_div(ptr: *const u8, start: i64, end: i64, init: f64, poison: *mut i8) -> f64 {
    note_native_call();
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, f64, *mut i8) -> f64>(ptr)(
            start, end, init, poison,
        )
    }
}

/// Call a **captured** scalar `f64` range reduce `fn(start, end, init, caps) -> f64` (the
/// float dot-product kernel). Each `caps` slot is a packed `f64`-array BASE pointer the
/// kernel indexes by the loop counter (`CaptureKind::ArrayF64`), loading `f64` elements.
/// SAFETY: the VM guarantees `ptr` is a finalized captured f64 scalar kernel from
/// [`define_reduce_loop`], `caps` points to at least the loop's capture count of pointers,
/// and for every array the whole counter range `[start, end)` is within its bounds (the VM's
/// pre-check) so the kernel's unchecked `f64` loads stay in-bounds.
pub unsafe fn call_reduce_f64_caps(ptr: *const u8, start: i64, end: i64, init: f64, caps: *const i64) -> f64 {
    note_native_call();
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, f64, *const i64) -> f64>(ptr)(
            start, end, init, caps,
        )
    }
}

/// Call a **captured** scalar reduce loop `fn(start, end, init, caps) -> i64` (the nested-
/// fold / dot-product kernel). `caps` points to the loop's capture count of `i64` slots —
/// each either a loop-invariant scalar VALUE (`CaptureKind::Scalar`) or a packed-array BASE
/// pointer the kernel indexes by the loop counter (`CaptureKind::ArrayI64`), per the loop's
/// ordered `captures`. SAFETY: the VM guarantees `ptr` is a finalized captured scalar kernel
/// from [`define_reduce_loop`], `caps` points to at least that many `i64`s, and for every
/// array slot the whole counter range `[start, end)` is within that array's bounds (the VM's
/// pre-check) so the kernel's unchecked element loads stay in-bounds.
pub unsafe fn call_reduce_caps(ptr: *const u8, start: i64, end: i64, init: i64, caps: *const i64) -> i64 {
    note_native_call();
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
    note_native_call();
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, *mut i64)>(ptr)(start, end, acc)
    }
}

/// Run a **parallel nested reduce**: for each `i` in `os..oe`, call the captured inner reduce
/// kernel `f(is, ie, init, caps) -> i64`, collecting the results in order (the outer map's result
/// array). `template` is the kernel's caps buffer with the loop-invariant array-base pointers
/// already in place and a placeholder at `scalar_pos`; each worker copies it and overwrites ONLY
/// `scalar_pos` with its own `i`, so no two workers share a mutable caps buffer. The all-pairs
/// shape `range(n).map(i => range(n).reduce(0,(acc,j)=>...codes[i]...codes[j]...))` lands here
/// with `template = [codes_ptr, <placeholder>]`, `scalar_pos = 1`.
///
/// Fully deterministic and identical to the sequential outer loop: each `i` is independent,
/// `into_par_iter().map().collect()` preserves order, every worker reads the SAME read-only array
/// bases (held alive by the caller across this call) plus its own `i` — no shared mutable state,
/// no `Rc` crosses threads — and i64 folds carry no float non-associativity. Parallel past the
/// shared threshold; below it a plain loop (byte-identical result). The scalar-only nested reduce
/// (no array caps) is just the degenerate case `template = [<placeholder>]`, `scalar_pos = 0`.
///
/// # Safety
/// `ptr` must be a finalized captured i64 reduce kernel from [`define_reduce_loop`] —
/// `extern "C" fn(i64, i64, i64, *const i64) -> i64` — whose caps arity equals `template.len()`
/// (`<= MAX_CAPTURES`); the array-base pointers in `template` must stay valid for the whole call
/// (the caller holds their `Rc`s), `scalar_pos < template.len()`, and every `i` in `os..oe` must
/// be a valid index into any scalar-indexed array (the VM's hoisted pre-check guarantees this).
#[allow(clippy::too_many_arguments)] // ptr + the 5 range/init scalars + caps template + scalar slot
pub unsafe fn run_nested_reduce_arrays(
    ptr: *const u8,
    os: i64,
    oe: i64,
    is: i64,
    ie: i64,
    init: i64,
    template: &[i64],
    scalar_pos: usize,
) -> Vec<i64> {
    note_native_call();
    // A finalized native fn pointer is `Send + Sync` (an address), so it may be shared across
    // rayon workers; capture `f` (Copy), never the raw `*const u8`. `template` is a read-only
    // `&[i64]` (Sync) shared by reference; each worker copies it into its own stack buffer.
    let f: extern "C" fn(i64, i64, i64, *const i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    // Spans in i128, clamped at 0 — the VM cap-check only bounds them from ABOVE, so a reverse
    // (empty) range `os > oe` reaches here; an i64 `oe - os` would OVERFLOW (a debug-profile
    // panic, diverging from the tree-walker's clean empty result). i128 + `.max(0)` yields
    // `n = 0` for a reverse range → the empty `(os..oe)` iteration, matching `int_range`.
    let n = ((oe as i128) - (os as i128)).max(0) as usize;
    // Parallelize on TOTAL work (`outer × inner`), not the outer count alone — a small outer
    // range over a large inner reduce (few points, long fold) is still worth splitting. Needs
    // at least 2 outer iterations to distribute.
    let inner_span = ((ie as i128) - (is as i128)).max(0) as usize;
    let total = n.saturating_mul(inner_span);
    let k = template.len();
    let run_one = |i: i64| -> i64 {
        // Per-worker caps buffer: array bases from `template`, `i` at the scalar slot.
        let mut buf = [0i64; crate::jit::MAX_CAPTURES];
        buf[..k].copy_from_slice(template);
        buf[scalar_pos] = i;
        f(is, ie, init, buf.as_ptr())
    };
    if n >= 2 && total >= crate::interp::PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        (os..oe).into_par_iter().map(run_one).collect()
    } else {
        (os..oe).map(run_one).collect()
    }
}

/// Run a native map kernel `f(src_ptr, dst_ptr, len, caps_ptr)` over `src`, producing a
/// fresh `Vec<D>` of the same length and order. **At or above `PAR_MATH_THRESHOLD` the
/// buffer is split into fixed chunks run on rayon workers.** This is safe *and* fully
/// deterministic for a map: each `dst[i]` depends only on `src[i]` and the read-only
/// `caps`, chunk `k` reads and writes the SAME index range `[k*CH, (k+1)*CH)` on both
/// sides (so output order is byte-identical to the sequential run), the `dst` chunks are
/// disjoint sub-slices of a freshly-owned `Vec` (no aliasing), and a map performs no
/// cross-element accumulation — the forbidden non-associative float reduction never
/// occurs here. The threshold is shared with the interpreter's own parallel map.
///
/// # Safety
/// `ptr` must be a finalized `extern "C" fn(*const S, *mut D, i64, *const C)` matching
/// `S`/`D`/`C`, and `caps` a valid `[C]` the kernel only reads.
unsafe fn run_map_chunked<S, D, C>(ptr: *const u8, src: &[S], caps: &[C]) -> Vec<D>
where
    S: Sync,
    D: Copy + Default + Send,
    C: Sync,
{
    let n = src.len();
    let mut dst: Vec<D> = vec![D::default(); n];
    if n == 0 {
        return dst;
    }
    note_native_call();
    // A finalized native fn pointer is `Send + Sync` (it's an address), so it may be
    // shared across rayon workers; the raw data pointers are re-derived per chunk INSIDE
    // the closure from the (Sync) slices, never captured across threads.
    let f: extern "C" fn(*const S, *mut D, i64, *const C) = unsafe { std::mem::transmute(ptr) };
    if n >= crate::interp::PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        const CH: usize = 1 << 14;
        src.par_chunks(CH)
            .zip(dst.par_chunks_mut(CH))
            .for_each(|(s, d)| f(s.as_ptr(), d.as_mut_ptr(), s.len() as i64, caps.as_ptr()));
    } else {
        f(src.as_ptr(), dst.as_mut_ptr(), n as i64, caps.as_ptr());
    }
    dst
}

/// Run a native map kernel over `src` (same length and order). Parallel past the shared
/// threshold — see [`run_map_chunked`]. SAFETY: `ptr` is a finalized
/// `extern "C" fn(*const i64,*mut i64,i64,*const i64)` from [`define_array_kernel`].
pub unsafe fn run_map_kernel(ptr: *const u8, src: &[i64], caps: &[i64]) -> Vec<i64> {
    unsafe { run_map_chunked::<i64, i64, i64>(ptr, src, caps) }
}

/// The `f64` map kernel: `dst[i] = body(src[i])` over an `f64` buffer, with `f64`
/// captures. A map has no cross-element accumulation, so the chunked-parallel form is
/// byte-identical to sequential (see [`run_map_chunked`]). SAFETY: as [`run_map_kernel`],
/// with an `fn(*const f64, *mut f64, i64, *const f64)` contract.
pub unsafe fn run_map_kernel_f64(ptr: *const u8, src: &[f64], caps: &[f64]) -> Vec<f64> {
    unsafe { run_map_chunked::<f64, f64, f64>(ptr, src, caps) }
}

/// The **mixed** map kernel: `dst[i] = body(src[i])` reading an `i64` buffer and writing
/// `f64` (Int source, float body), no captures. SAFETY: as [`run_map_kernel`], with an
/// `fn(*const i64, *mut f64, i64, *const i64)` contract; the caps pointer is a valid
/// (empty) slice the kernel never reads (mixed kernels are capture-free by construction).
pub unsafe fn run_map_kernel_mixed(ptr: *const u8, src: &[i64]) -> Vec<f64> {
    // Capture-free by construction — pass an empty caps slice the kernel never reads.
    let no_caps: [i64; 0] = [];
    unsafe { run_map_chunked::<i64, f64, i64>(ptr, src, &no_caps) }
}

/// Run a native filter kernel over `src`, returning the kept elements in order. SAFETY:
/// `ptr` is a finalized `extern "C" fn(*const i64,*mut i64,i64)->i64` (kept count) from
/// [`define_array_kernel`].
pub unsafe fn run_filter_kernel(ptr: *const u8, src: &[i64]) -> Vec<i64> {
    note_native_call();
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
    note_native_call();
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
    note_native_call();
    let f: extern "C" fn(*const i64, i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    f(src.as_ptr(), src.len() as i64, init)
}

/// Run a native **scalar `f64`** reduce over a `Float` buffer: `acc = init; for x in src
/// { acc = body(acc, x) }` with `fadd`/`fmul` left-to-right — bit-exact to the interpreter.
/// SAFETY: `ptr` is a finalized `extern "C" fn(*const f64, i64, f64) -> f64` from a
/// `float`-flagged `define_fused_kernel`, guaranteed by the VM's `Floats` source + `Float`
/// init check.
pub unsafe fn run_fused_reduce_f64(ptr: *const u8, src: &[f64], init: f64) -> f64 {
    note_native_call();
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
    note_native_call();
    let f: extern "C" fn(*const i64, i64, *mut i64) = unsafe { std::mem::transmute(ptr) };
    f(src.as_ptr(), src.len() as i64, acc)
}

/// Run a fused **f64 tuple**-`Reduce` over a `Float` buffer `src`: the kernel reads `f64`
/// elements and folds N `f64` accumulator slots packed as bit patterns in the `i64` `acc`
/// buffer (initial values in, folded result out — the same memory the VM packs/unpacks via
/// `acc_to_slots_f64`/`rebuild_acc_f64`). SAFETY: as [`run_fused_tuple_reduce`], but the
/// element pointer is `*const f64` and `ptr` is a `float`-flagged tuple kernel.
pub unsafe fn run_fused_tuple_reduce_f64(ptr: *const u8, src: &[f64], acc: *mut i64) {
    note_native_call();
    let f: extern "C" fn(*const f64, i64, *mut i64) = unsafe { std::mem::transmute(ptr) };
    f(src.as_ptr(), src.len() as i64, acc)
}

/// Run a fused array→`Count` pipeline over `src` (`fn(src,len,_)->count`). SAFETY: as
/// [`run_fused_collect`].
pub unsafe fn run_fused_count(ptr: *const u8, src: &[i64]) -> i64 {
    note_native_call();
    let f: extern "C" fn(*const i64, i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    f(src.as_ptr(), src.len() as i64, 0)
}

/// Call an `f64`-specialized JIT function. SAFETY: as [`call_i64`], with an
/// `extern "C" fn(f64×n)->f64` contract.
pub unsafe fn call_f64(ptr: *const u8, args: &[f64]) -> f64 {
    note_native_call();
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
            5 => std::mem::transmute::<*const u8, extern "C" fn(f64, f64, f64, f64, f64) -> f64>(
                ptr,
            )(args[0], args[1], args[2], args[3], args[4]),
            6 => std::mem::transmute::<*const u8, extern "C" fn(f64, f64, f64, f64, f64, f64) -> f64>(
                ptr,
            )(args[0], args[1], args[2], args[3], args[4], args[5]),
            _ => unreachable!("JIT arity is capped at {MAX_ARITY}"),
        }
    }
}

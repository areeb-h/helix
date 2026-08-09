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
/// kernel `f(start(i), end(i), init, caps) -> i64`, collecting the results in order (the outer
/// map's result array). `template` is the kernel's caps buffer with the loop-invariant array-base
/// pointers already in place and a placeholder at `scalar_pos`; each worker copies it and
/// overwrites ONLY `scalar_pos` with its own `i`, so no two workers share a mutable caps buffer.
/// The all-pairs shape `range(n).map(i => range(n).reduce(0,(acc,j)=>...codes[i]...codes[j]...))`
/// lands here with `template = [codes_ptr, <placeholder>]`, `scalar_pos = 1`.
///
/// The inner bounds are AFFINE in the outer index: `start(i) = sc * i + is`, `end(i) = ec * i + ie`
/// (the pushed `is`/`ie` are the bases). Each worker computes its OWN bounds from its OWN `i` —
/// nothing is hoisted — which is what admits a TRIANGULAR `range(i + 1, n)` (`sc = 1`, `ec = 0`).
/// A rectangular range has `sc = ec = 0`, giving `is`/`ie` verbatim for every `i`.
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
/// be a valid index into any scalar-indexed array. For every `i` in `os..oe` the VM's pre-check
/// must also guarantee that `sc*i + is` and `ec*i + ie` do not overflow `i64` and that each
/// counter-indexed array covers the WHOLE per-`i` range `[start(i), end(i))` — the union of those
/// ranges is what the pre-check bounds, since each worker's range now differs.
#[allow(clippy::too_many_arguments)] // ptr + the 5 range/init scalars + 2 affine coeffs + caps
pub unsafe fn run_nested_reduce_arrays(
    ptr: *const u8,
    os: i64,
    oe: i64,
    is: i64,
    ie: i64,
    sc: i64,
    ec: i64,
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
    // at least 2 outer iterations to distribute. The per-`i` span is itself affine now, so the
    // total is the TRAPEZOID over the clamped endpoint spans (exact for a rectangular range, and
    // for a triangular one it correctly gives ~n²/2 rather than the peak n²). This only picks
    // the parallel-vs-serial route — it can never change the RESULT.
    let span_at = |i: i128| {
        (((ec as i128) * i + (ie as i128)) - ((sc as i128) * i + (is as i128))).max(0)
    };
    let total = if n == 0 {
        0
    } else {
        let avg = (span_at(os as i128) + span_at(oe as i128 - 1)) / 2;
        (avg.saturating_mul(n as i128)).min(usize::MAX as i128) as usize
    };
    let k = template.len();
    let run_one = |i: i64| -> i64 {
        // Per-worker caps buffer: array bases from `template`, `i` at the scalar slot.
        let mut buf = [0i64; crate::jit::MAX_CAPTURES];
        buf[..k].copy_from_slice(template);
        buf[scalar_pos] = i;
        // This worker's OWN inner bounds, from its OWN `i` — the triangular case. The VM's
        // pre-check proved these fit `i64` for every `i` in `os..oe` (affine ⇒ monotone ⇒ the
        // endpoints bound the range), so the arithmetic is exact and cannot wrap.
        f(sc * i + is, ec * i + ie, init, buf.as_ptr())
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

/// [`run_map_chunked`] over a buffer the caller **uniquely owns**, reusing it as both source
/// and destination instead of allocating a second one.
///
/// A map over a real array allocated a fresh output while the input stayed live, so a chain
/// like `xs.map(f).map(g)` peaked at BOTH buffers even though the intermediate is dead the
/// moment `g` consumes it. Measured at n=20M (160 MB per buffer): 340 MB peak, identical to
/// keeping the source alive on purpose. It also pays to zero a fresh `Vec` that is about to be
/// overwritten in full.
///
/// Only the caller can know the buffer is dead, and it proves that with `Rc::get_mut` — so this
/// runs ONLY when no other handle to the array exists, and the mutation is therefore
/// unobservable. Values are unchanged either way; this is an allocation decision, not a
/// semantic one.
///
/// ALIASING is the sharp edge and it is safe by the map's own shape: `dst[i]` depends only on
/// `src[i]` and the read-only `caps`, so reading and writing the same index in the same
/// iteration reads the old value before storing the new one, and no iteration ever looks at an
/// index another iteration writes. Both pointers are derived from ONE `&mut` borrow rather than
/// from a `&`/`&mut` pair, so no aliasing rule is broken on the Rust side either. The parallel
/// form keeps that: `par_chunks_mut` hands out disjoint sub-slices, and each chunk aliases only
/// itself, at matching indices — which is also why the output stays byte-identical to the
/// sequential run. A body that reads a *captured* array is excluded by the caller (it would
/// carry `index_bounds`, and those are dischargeable only over a lazy range, which has no
/// buffer to reuse in the first place).
///
/// # Safety
/// As [`run_map_chunked`], with `S == D == T`: `ptr` must be a finalized
/// `extern "C" fn(*const T, *mut T, i64, *const C)`.
unsafe fn run_map_inplace<T, C>(ptr: *const u8, buf: &mut [T], caps: &[C])
where
    T: Send,
    C: Sync,
{
    let n = buf.len();
    if n == 0 {
        return;
    }
    note_native_call();
    let f: extern "C" fn(*const T, *mut T, i64, *const C) = unsafe { std::mem::transmute(ptr) };
    if n >= crate::interp::PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        const CH: usize = 1 << 14;
        buf.par_chunks_mut(CH).for_each(|d| {
            let p = d.as_mut_ptr();
            f(p as *const T, p, d.len() as i64, caps.as_ptr());
        });
    } else {
        let p = buf.as_mut_ptr();
        f(p as *const T, p, n as i64, caps.as_ptr());
    }
}

/// The `i64` map kernel run in place over a uniquely-owned buffer. SAFETY: as
/// [`run_map_kernel`], with the `fn(*const i64, *mut i64, i64, *const i64)` contract.
pub unsafe fn run_map_kernel_inplace(ptr: *const u8, buf: &mut [i64], caps: &[i64]) {
    unsafe { run_map_inplace::<i64, i64>(ptr, buf, caps) }
}

/// The `f64` map kernel run in place over a uniquely-owned buffer. SAFETY: as
/// [`run_map_kernel_f64`], with the `fn(*const f64, *mut f64, i64, *const f64)` contract.
pub unsafe fn run_map_kernel_f64_inplace(ptr: *const u8, buf: &mut [f64], caps: &[f64]) {
    unsafe { run_map_inplace::<f64, f64>(ptr, buf, caps) }
}

/// The RANGE-source twin of [`run_map_chunked`]: the same kernel, fed source values that are
/// COMPUTED rather than read from a materialized buffer.
///
/// A lazy range's element `j` is `start + step*j`, so handing the kernel a real array meant
/// building one purely to be read once — and it stayed live alongside the output, making a single
/// `(0..n).map(f)` peak at TWICE its result. Measured at n=20M (160 MB of payload): 328 MB peak
/// before, and the k1 dot product's documented ~400 MB overhead over C was exactly this, one
/// transient materialized range.
///
/// Each chunk's values are generated into a small scratch buffer (16K elements = 128 KB, so it
/// stays in cache) that is reused per chunk — peak becomes the output plus a scratch instead of
/// two full buffers. The element formula is `Value::range_at`'s verbatim, computed in `i128` so
/// the multiply cannot overflow before the truncation the interpreter also performs.
///
/// SAFETY: as [`run_map_chunked`] — `ptr` is a finalized
/// `extern "C" fn(*const i64,*mut D,i64,*const C)`, and the scratch outlives the call.
unsafe fn run_map_range_chunked<D, C>(
    ptr: *const u8,
    start: i64,
    step: i64,
    len: usize,
    caps: &[C],
) -> Vec<D>
where
    D: Copy + Default + Send,
    C: Sync,
{
    let mut dst: Vec<D> = vec![D::default(); len];
    if len == 0 {
        return dst;
    }
    note_native_call();
    let f: extern "C" fn(*const i64, *mut D, i64, *const C) = unsafe { std::mem::transmute(ptr) };
    const CH: usize = 1 << 14;
    let at = |j: usize| -> i64 { (start as i128 + step as i128 * j as i128) as i64 };
    if len >= crate::interp::PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        dst.par_chunks_mut(CH).enumerate().for_each(|(ci, d)| {
            let base = ci * CH;
            let scratch: Vec<i64> = (0..d.len()).map(|k| at(base + k)).collect();
            f(scratch.as_ptr(), d.as_mut_ptr(), d.len() as i64, caps.as_ptr());
        });
    } else {
        let mut scratch: Vec<i64> = Vec::with_capacity(CH.min(len));
        for base in (0..len).step_by(CH) {
            let n = CH.min(len - base);
            scratch.clear();
            scratch.extend((0..n).map(|k| at(base + k)));
            f(scratch.as_ptr(), dst[base..].as_mut_ptr(), n as i64, caps.as_ptr());
        }
    }
    dst
}

/// `run_map_kernel` over a lazy range, with no materialization. See [`run_map_range_chunked`].
pub unsafe fn run_map_kernel_range(
    ptr: *const u8,
    start: i64,
    step: i64,
    len: usize,
    caps: &[i64],
) -> Vec<i64> {
    unsafe { run_map_range_chunked::<i64, i64>(ptr, start, step, len, caps) }
}

/// The **mixed** kernel (i64 elements → f64 output) over a lazy range, with no materialization.
pub unsafe fn run_map_kernel_mixed_range(
    ptr: *const u8,
    start: i64,
    step: i64,
    len: usize,
    caps: &[i64],
) -> Vec<f64> {
    unsafe { run_map_range_chunked::<f64, i64>(ptr, start, step, len, caps) }
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
/// `f64` (Int source, float body). `caps` carries the loop-invariant captures as `i64`
/// slots: scalar values, and — for an INDEXED body — `f64`-array base pointers the VM
/// bounds-checked and type-checked (`Floats` only) before this call. Empty for the
/// capture-free unindexed form. SAFETY: as [`run_map_kernel`], with an
/// `fn(*const i64, *mut f64, i64, *const i64)` contract; the caller keeps the arrays
/// behind any base pointers alive across the call.
pub unsafe fn run_map_kernel_mixed(ptr: *const u8, src: &[i64], caps: &[i64]) -> Vec<f64> {
    unsafe { run_map_chunked::<i64, f64, i64>(ptr, src, caps) }
}

/// Run a native filter kernel over `src`, returning the kept elements in order. SAFETY:
/// `ptr` is a finalized `extern "C" fn(*const i64,*mut i64,i64)->i64` (kept count) from
/// [`define_array_kernel`].
/// The RANGE-source twin of [`run_filter_kernel`], for the same reason as
/// [`run_map_range_chunked`]: materializing a lazy range purely to be read once doubled peak
/// memory. Each chunk's values are generated into a reused scratch and filtered straight into
/// `dst` at the running offset, so survivors stay contiguous and in order. Serial by
/// construction — the filter kernel compacts, so chunk *i*'s output position depends on how many
/// elements chunks `0..i` kept, which is exactly the dependency that rules out parallelism here
/// (`run_filter_kernel` is serial for the same reason).
///
/// SAFETY: as [`run_filter_kernel`] — `ptr` is a finalized
/// `extern "C" fn(*const i64,*mut i64,i64,*const i64)->i64`, and `dst` has room for every element
/// because a filter can keep them all.
pub unsafe fn run_filter_kernel_range(
    ptr: *const u8,
    start: i64,
    step: i64,
    len: usize,
    caps: &[i64],
) -> Vec<i64> {
    note_native_call();
    let mut dst = vec![0i64; len];
    if len == 0 {
        return dst;
    }
    let f: extern "C" fn(*const i64, *mut i64, i64, *const i64) -> i64 =
        unsafe { std::mem::transmute(ptr) };
    const CH: usize = 1 << 14;
    let at = |j: usize| -> i64 { (start as i128 + step as i128 * j as i128) as i64 };
    let mut scratch: Vec<i64> = Vec::with_capacity(CH.min(len));
    let mut kept_total = 0usize;
    for base in (0..len).step_by(CH) {
        let n = CH.min(len - base);
        scratch.clear();
        scratch.extend((0..n).map(|k| at(base + k)));
        let kept =
            f(scratch.as_ptr(), dst[kept_total..].as_mut_ptr(), n as i64, caps.as_ptr());
        kept_total += kept as usize;
    }
    dst.truncate(kept_total);
    dst
}

/// Run a RAISING map kernel (body contains a rounder — see `ArrayKernel::raises`) over a
/// materialized buffer: `fn(src, dst, len, caps, poison_cell)`. Returns `None` when the
/// kernel set poison — some element's rounded result left the i64 range — and the caller
/// falls through to the bytecode loop, which re-runs and raises the exact interpreter
/// error. The whole output is discarded on poison, which is also why a raising kernel must
/// NEVER take the in-place buffer reuse: the fall-back needs the source intact.
///
/// SERIAL deliberately: the parallel form would need per-chunk poison cells to stay
/// race-free, and a raising map has shown no throughput demand — simplest-correct wins
/// until a profile says otherwise.
///
/// SAFETY: `ptr` is a finalized `extern "C" fn(*const S, *mut D, i64, *const i64, *mut i64)`
/// matching `S`/`D` from `define_array_kernel` with the poison signature.
unsafe fn run_map_poison<S: Sync, D: Copy + Default + Send>(
    ptr: *const u8,
    src: &[S],
    caps: &[i64],
) -> Option<Vec<D>> {
    note_native_call();
    let n = src.len();
    let mut dst: Vec<D> = vec![D::default(); n];
    if n == 0 {
        return Some(dst);
    }
    let f: extern "C" fn(*const S, *mut D, i64, *const i64, *mut i64) =
        unsafe { std::mem::transmute(ptr) };
    // PARALLEL past the threshold, with a poison cell PER CHUNK reduced by `|`. Sound for
    // the same reason the non-poison map is: chunk `k` reads and writes only its own index
    // range, so the output is byte-identical to the sequential run; and poison is a
    // monotonic flag, so OR-reducing it is order-independent. Every chunk runs even after
    // one poisons — the whole output is discarded either way, so the early exit the serial
    // form had bought nothing but a branch.
    let poison = if n >= crate::interp::PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        const CH: usize = 1 << 14;
        src.par_chunks(CH)
            .zip(dst.par_chunks_mut(CH))
            .map(|(s, d)| {
                let mut p: i64 = 0;
                f(s.as_ptr(), d.as_mut_ptr(), s.len() as i64, caps.as_ptr(), &mut p);
                p
            })
            .reduce(|| 0i64, |a, b| a | b)
    } else {
        let mut p: i64 = 0;
        f(src.as_ptr(), dst.as_mut_ptr(), n as i64, caps.as_ptr(), &mut p);
        p
    };
    (poison == 0).then_some(dst)
}

/// The i64-out raising map kernel (Int-rooted mixed body with a rounder). SAFETY: as
/// [`run_map_poison`] with `S = D = i64`.
pub unsafe fn run_map_kernel_int_poison(
    ptr: *const u8,
    src: &[i64],
    caps: &[i64],
) -> Option<Vec<i64>> {
    unsafe { run_map_poison::<i64, i64>(ptr, src, caps) }
}

/// The f64-out raising map kernel (Float-rooted mixed body with a rounder inside). SAFETY:
/// as [`run_map_poison`] with `S = i64, D = f64`.
pub unsafe fn run_map_kernel_mixed_poison(
    ptr: *const u8,
    src: &[i64],
    caps: &[i64],
) -> Option<Vec<f64>> {
    unsafe { run_map_poison::<i64, f64>(ptr, src, caps) }
}

/// The RANGE-source twin of [`run_map_poison`]: counter values are generated per chunk into
/// a reused scratch (so nothing is materialized), and generation STOPS at the first
/// poisoned chunk — a 20M-element map that raised in chunk one does not run to completion
/// before falling back. Serial for the same reason as the materialized form.
///
/// SAFETY: as [`run_map_poison`]; the element formula is `Value::range_at`'s verbatim in
/// `i128`, identical to `run_map_kernel_range`'s.
unsafe fn run_map_range_poison<D: Copy + Default + Send>(
    ptr: *const u8,
    start: i64,
    step: i64,
    len: usize,
    caps: &[i64],
) -> Option<Vec<D>> {
    note_native_call();
    let mut dst: Vec<D> = vec![D::default(); len];
    if len == 0 {
        return Some(dst);
    }
    let f: extern "C" fn(*const i64, *mut D, i64, *const i64, *mut i64) =
        unsafe { std::mem::transmute(ptr) };
    const CH: usize = 1 << 14;
    let at = |j: usize| -> i64 { (start as i128 + step as i128 * j as i128) as i64 };
    // Each chunk generates its own counter values and carries its own poison cell, reduced
    // by `|` — see [`run_map_poison`] for why that is sound and order-independent. The
    // per-chunk scratch is a fresh 16K buffer (128 KB) rather than one reused across the
    // loop: workers cannot share it, and the allocation is amortized over 16K elements of
    // native work.
    let poison = if len >= crate::interp::PAR_MATH_THRESHOLD {
        use rayon::prelude::*;
        dst.par_chunks_mut(CH)
            .enumerate()
            .map(|(ci, d)| {
                let base = ci * CH;
                let scratch: Vec<i64> = (0..d.len()).map(|k| at(base + k)).collect();
                let mut p: i64 = 0;
                f(scratch.as_ptr(), d.as_mut_ptr(), d.len() as i64, caps.as_ptr(), &mut p);
                p
            })
            .reduce(|| 0i64, |a, b| a | b)
    } else {
        let mut scratch: Vec<i64> = Vec::with_capacity(CH.min(len));
        let mut p: i64 = 0;
        for base in (0..len).step_by(CH) {
            let n = CH.min(len - base);
            scratch.clear();
            scratch.extend((0..n).map(|k| at(base + k)));
            f(scratch.as_ptr(), dst[base..].as_mut_ptr(), n as i64, caps.as_ptr(), &mut p);
            if p != 0 {
                break; // nothing downstream can un-poison it; the output is discarded
            }
        }
        p
    };
    (poison == 0).then_some(dst)
}

/// SAFETY: as [`run_map_range_poison`] with `D = i64`.
pub unsafe fn run_map_kernel_range_int_poison(
    ptr: *const u8,
    start: i64,
    step: i64,
    len: usize,
    caps: &[i64],
) -> Option<Vec<i64>> {
    unsafe { run_map_range_poison::<i64>(ptr, start, step, len, caps) }
}

/// SAFETY: as [`run_map_range_poison`] with `D = f64`.
pub unsafe fn run_map_kernel_range_mixed_poison(
    ptr: *const u8,
    start: i64,
    step: i64,
    len: usize,
    caps: &[i64],
) -> Option<Vec<f64>> {
    unsafe { run_map_range_poison::<f64>(ptr, start, step, len, caps) }
}

/// Run a native scan (prefix-fold) kernel over the range counter `[start, end)`, returning
/// the array of successive accumulators. SERIAL by definition — `out[i]` depends on
/// `out[i-1]` — so there is no parallel form and byte-identity needs no ordering argument.
///
/// SAFETY: `ptr` is a finalized `extern "C" fn(i64, i64, i64, *mut i64, *const i64)` from
/// `define_scan_loop`; `dst` is allocated here with exactly the `end - start` slots the
/// kernel writes (the VM capped the length before dispatch); `caps` is a valid slice the
/// kernel only reads.
pub unsafe fn run_scan_kernel_range(
    ptr: *const u8,
    start: i64,
    end: i64,
    init: i64,
    caps: &[i64],
) -> Vec<i64> {
    note_native_call();
    let n = (end as i128 - start as i128).max(0) as usize;
    let mut dst = vec![0i64; n];
    if n == 0 {
        return dst;
    }
    let f: extern "C" fn(i64, i64, i64, *mut i64, *const i64) =
        unsafe { std::mem::transmute(ptr) };
    f(start, end, init, dst.as_mut_ptr(), caps.as_ptr());
    dst
}

/// Run a native f64 filter kernel over `src`, or `None` if the kernel POISONED — an
/// ordering comparison met a NaN, which the interpreter treats as an error ("cannot
/// compare these values"), so the caller falls through to the bytecode loop and raises
/// it at the exact element. Writes to `dst` before the poison are discarded, and `src`
/// was never mutated, so the re-run sees pristine input. SERIAL, like
/// [`run_filter_kernel`], to preserve element order.
///
/// SAFETY: `ptr` is a finalized
/// `extern "C" fn(*const f64, *mut f64, i64, *const f64) -> i64` filter kernel from
/// `define_array_kernel`'s "filterf" pass; `dst` is allocated here with `src.len()`
/// slots, the most the kernel can write; `caps` is a valid slice the kernel only reads.
pub unsafe fn run_filter_kernel_f64(
    ptr: *const u8,
    src: &[f64],
    caps: &[f64],
) -> Option<Vec<f64>> {
    note_native_call();
    let mut dst = vec![0f64; src.len()];
    if src.is_empty() {
        return Some(dst);
    }
    let f: extern "C" fn(*const f64, *mut f64, i64, *const f64) -> i64 =
        unsafe { std::mem::transmute(ptr) };
    let kept = f(src.as_ptr(), dst.as_mut_ptr(), src.len() as i64, caps.as_ptr());
    if kept < 0 {
        return None;
    }
    dst.truncate(kept as usize);
    Some(dst)
}

pub unsafe fn run_filter_kernel(ptr: *const u8, src: &[i64], caps: &[i64]) -> Vec<i64> {
    note_native_call();
    let mut dst = vec![0i64; src.len()];
    if src.is_empty() {
        return dst;
    }
    let f: extern "C" fn(*const i64, *mut i64, i64, *const i64) -> i64 =
        unsafe { std::mem::transmute(ptr) };
    let kept = f(src.as_ptr(), dst.as_mut_ptr(), src.len() as i64, caps.as_ptr());
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

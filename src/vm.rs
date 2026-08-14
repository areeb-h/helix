//! Stack-machine virtual machine that executes [`crate::bytecode`] programs.
//!
//! The hot loop is flat: fetch an [`Op`], advance the instruction pointer, act.
//! Compared with the tree-walker it removes two structural costs — repeated AST
//! traversal and per-variable `String` hashing — by working over pre-resolved
//! slot indices and a contiguous operand stack.
//!
//! Crucially, **recursion lives on the heap, not the native stack**: a call
//! pushes a [`Frame`] onto a `Vec`, so recursion is bounded only by memory (with
//! a high guard that turns a runaway into a clean error). This is the proper
//! fix to the depth limit the tree-walker needs a 2 GiB thread to paper over.
//!
//! Semantics are inherited wholesale from the interpreter: arithmetic, unary
//! ops, boolean three-valued logic, and builtins all route through the very same
//! functions the tree-walker uses, so the VM is observationally identical.

use rustc_hash::FxHashMap;

use crate::ast::BinOp;
use crate::bytecode::{Op, Program};
use crate::error::HelixError;
use crate::interp::{as_bool, eval_binary, tri, Interp};
use crate::value::Value;

/// Scalar fast path for binary operators — the overwhelmingly common case in
/// hot loops. Skips `eval_binary`'s `missing` test and array/tensor broadcasting
/// dispatch for plain `Int`/`Float` operands, falling back to the full
/// implementation for everything else. Each fast case is byte-for-byte identical
/// to what `arith` / `compare` / `values_equal` produce (verified by the parity
/// tests), including release-mode integer wrap and the `f64`-based integer
/// ordering the tree-walker already uses.
#[inline(always)]
fn binary(op: &BinOp, a: Value, b: Value, line: usize, col: usize) -> Result<Value, HelixError> {
    use BinOp::*;
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => {
            let (x, y) = (*x, *y);
            match op {
                // wrapping + exact i64 ordering — identical to `arith`/`compare`
                // and to the JIT (the f64-cast ordering was lossy above 2^53).
                Add => return Ok(Value::Int(x.wrapping_add(y))),
                Sub => return Ok(Value::Int(x.wrapping_sub(y))),
                Mul => return Ok(Value::Int(x.wrapping_mul(y))),
                Lt => return Ok(Value::Bool(x < y)),
                Gt => return Ok(Value::Bool(x > y)),
                Le => return Ok(Value::Bool(x <= y)),
                Ge => return Ok(Value::Bool(x >= y)),
                Eq => return Ok(Value::Bool(x == y)),
                Ne => return Ok(Value::Bool(x != y)),
                // Mod/Div with a nonzero divisor match `arith`/`eval_binary`
                // exactly (Int `%` stays Int; `/` is always Float); a zero divisor
                // falls through to the error-raising full path. `wrapping_*_euclid`
                // (not `*_euclid`) so `i64::MIN % -1` / `i64::MIN // -1` wrap (to 0 /
                // i64::MIN) instead of the always-checked overflow panic — matching
                // the tree-walker (ops.rs) so the differential oracle stays green.
                Mod if y != 0 => return Ok(Value::Int(x.wrapping_rem_euclid(y))),
                FloorDiv if y != 0 => return Ok(Value::Int(x.wrapping_div_euclid(y))),
                Div if y != 0 => return Ok(Value::Float(x as f64 / y as f64)),
                // Integer bitwise — identical to `bitwise()` in ops.rs. Shifts only
                // shortcut for an in-range amount; an out-of-range shift falls to the
                // full path, which raises (never a panic/UB).
                BitAnd => return Ok(Value::Int(x & y)),
                BitOr => return Ok(Value::Int(x | y)),
                BitXor => return Ok(Value::Int(x ^ y)),
                Shl if (0..=63).contains(&y) => return Ok(Value::Int(x << y)),
                Shr if (0..=63).contains(&y) => return Ok(Value::Int(x >> y)),
                _ => {} // Div/Mod by zero, Pow, out-of-range shift → full path
            }
        }
        (Value::Float(x), Value::Float(y)) => {
            let (x, y) = (*x, *y);
            match op {
                Add => return Ok(Value::Float(x + y)),
                Sub => return Ok(Value::Float(x - y)),
                Mul => return Ok(Value::Float(x * y)),
                Eq => return Ok(Value::Bool(x == y)),
                Ne => return Ok(Value::Bool(x != y)),
                // Float `%` never errors on zero (yields NaN), matching `eval_binary`.
                Mod => return Ok(Value::Float(x.rem_euclid(y))),
                FloorDiv if y != 0.0 => return Ok(Value::Float(x.div_euclid(y))),
                Div if y != 0.0 => return Ok(Value::Float(x / y)),
                // Float ordering can hit NaN, which the full path turns into an
                // error — so don't shortcut comparisons here.
                _ => {}
            }
        }
        _ => {}
    }
    eval_binary(op, a, b, line, col)
}

/// Heap-frame recursion guard. Far higher than the tree-walker's stack-bound
/// limit (frames are small heap allocations), yet still catches infinite
/// recursion long before it can exhaust memory.
const VM_MAX_DEPTH: usize = crate::interp::MAX_CALL_DEPTH;

/// Upper bound on the memoization table, so a pathological program can't grow it
/// without limit; beyond this we simply stop caching (results stay correct, just
/// uncached). Memoizable functions have few distinct calls by design, so this is
/// only a safety backstop.
const MEMO_MAX_ENTRIES: usize = 5_000_000;

/// One scalar argument as a memo key. Floats are keyed by their **bit pattern**
/// (`to_bits`), which is exactly correct: bit-identical floats are the same value
/// (so the cached result is valid), distinct bits are distinct keys (so `+0.0`
/// vs `-0.0` are simply computed separately), and a NaN keys consistently against
/// itself. This makes pure float recursion (e.g. `fibf`) memoizable too.
#[derive(Hash, PartialEq, Eq, Clone)]
enum MemoArg {
    Int(i64),
    Float(u64),
}

/// Key into the memo table: a function index plus its scalar arguments. The 1- and
/// 2-argument cases (the overwhelming majority — `fib(n)`, `ackermann(m,n)`, …) are
/// stored **inline** so a memoizable call needs no heap allocation for its key; only
/// 3+ args fall back to a `Vec`.
#[derive(Hash, PartialEq, Eq, Clone)]
enum MemoKey {
    A1(usize, MemoArg),
    A2(usize, MemoArg, MemoArg),
    An(usize, Vec<MemoArg>),
}

/// The clean, catchable error for a collection that will not fit (ADR 0024: a limit is
/// an error, never a signal — never a dead process).
///
/// Shared by the VM's `map`/`filter` push sites and the tree-walker's, so all three
/// engines refuse the same program with the same words.
pub(crate) fn materialize_refused(
    lim: crate::value::MaterializeLimit,
    line: usize,
    col: usize,
) -> HelixError {
    use crate::value::MaterializeLimit;
    const LAZY: &str = "or stay lazy — a `map`/`filter` that feeds straight into \
                        `count`/`sum`/`first` does not materialize at all.";
    match lim {
        MaterializeLimit::Budget(bytes) => HelixError::new(
            format!(
                "this collection would hold more than {} MB of elements, which is too large",
                crate::value::MATERIALIZE_BUDGET / (1 << 20)
            ),
            line,
            col,
        )
        .hint(format!(
            "it had already reached ~{} MB. Keep the result under the limit, {}",
            bytes / (1 << 20),
            LAZY
        )),
        MaterializeLimit::Alloc(bytes) => HelixError::new(
            "there is not enough memory to hold this collection".to_string(),
            line,
            col,
        )
        .hint(format!(
            "it asked for {} MB in one block and the system refused. Build it in smaller \
             pieces, {}",
            bytes / (1 << 20),
            LAZY
        )),
    }
}

/// Project a (gated-scalar) argument value into a hashable memo argument.
fn memo_arg(v: &Value) -> MemoArg {
    match v {
        Value::Int(n) => MemoArg::Int(*n),
        Value::Float(f) => MemoArg::Float(f.to_bits()),
        _ => MemoArg::Int(0), // unreachable: gated by all_scalar
    }
}

/// What a comprehension iterates: a materialized array, or — for a fused
/// `range(...)` — a lazy integer counter (no array allocated at all).
enum CompSource {
    Array { arr: std::rc::Rc<crate::value::ArrayData>, idx: usize },
    Range { cur: i64, end: i64 },
}

/// Materialize a lazy `range` value (`ArrayData::Range`) on the top of the stack into a packed
/// `Ints` array. The JIT map/filter/fused kernels read `Ints`/`Floats` BUFFERS (via `as_ptr`), so
/// a lazy range must be densified before they can engage — this restores JIT execution on a range
/// value exactly as before ranges were lazy (the range's own O(1) methods — `first`/`count`/… —
/// never reach these ops, so they stay lazy). No-op unless the top is a `Range`; the resulting
/// `Ints` array is bit-identical to the elements the range represents, so both the native and the
/// bytecode fall-through paths see the same values.
fn densify_range_top(stack: &mut [Value]) {
    use crate::value::ArrayData;
    let ints = match stack.last() {
        Some(Value::Array(a)) if matches!(&**a, ArrayData::Range { .. }) => {
            Some(a.to_ints().expect("Range materializes to Ints").into_owned())
        }
        _ => None,
    };
    if let Some(ints) = ints {
        // `last_mut()` is `Some` — the match above already matched a top-of-stack array.
        *stack.last_mut().expect("stack top present") = Value::int_array(ints);
    }
}

/// Discharge an indexed `map` kernel's bounds obligations and marshal its `caps` slice
/// (array caps as base pointers, scalars as values), or `None` to fall back to the checked
/// bytecode loop — which raises the exact error, or wraps the exact way, that the native
/// kernel's UNCHECKED loads cannot. `src_range` is the receiver's range shape as read
/// BEFORE materialization; `None` means the source is an ordinary buffer.
///
/// `float_arrays` selects which SPECIALIZATION is being marshaled for: `false` = the i64
/// kernel (array caps must be `Ints`, elements load as I64), `true` = the mixed kernel
/// (array caps must be `Floats`, elements load as F64). This match-on-representation IS
/// the type guard — the one new hazard the f64 variant adds over the i64 one is an `Ints`
/// buffer reaching an F64 load, whose 8 bytes would reinterpret as a (usually tiny,
/// plausible-looking) float and corrupt results SILENTLY rather than crash. Declining
/// here, before any pointer is formed, is what makes that impossible; the dispatch tries
/// the other specialization or falls back to the bytecode loop.
///
/// The caller keeps `cap_vals` alive across the kernel call, which is what keeps the base
/// pointers valid.
fn map_index_caps(
    k: &crate::bytecode::ArrayKernel,
    cap_vals: &[Value],
    src_range: Option<(i64, i64, usize)>,
    float_arrays: bool,
) -> Option<Vec<i64>> {
    use crate::bytecode::{CaptureKind, IndexBound};
    use crate::value::ArrayData;
    let mut caps: Vec<i64> = Vec::with_capacity(cap_vals.len());
    let mut lens: Vec<i64> = Vec::with_capacity(cap_vals.len()); // array len; 0 for scalars
    for (cap, val) in k.captures.iter().zip(cap_vals.iter()) {
        match (cap.kind, val) {
            (CaptureKind::ArrayI64, Value::Array(a)) => match (&**a, float_arrays) {
                (ArrayData::Ints(v), false) => {
                    caps.push(v.as_ptr() as i64);
                    lens.push(v.len() as i64);
                }
                (ArrayData::Floats(v), true) => {
                    caps.push(v.as_ptr() as i64);
                    lens.push(v.len() as i64);
                }
                // The wrong representation for this specialization — decline; the
                // dispatch tries the other kernel or the checked loop.
                _ => return None,
            },
            // An INDEX scalar — always `i64` (an index is an integer). A `Value::Float` here
            // means the whole index arithmetic isn't `i64`; decline.
            (CaptureKind::Scalar, Value::Int(i)) => {
                caps.push(*i);
                lens.push(0);
            }
            // A VALUE scalar (SAXPY's coefficient). In the i64 kernel it must be `Int` and
            // rides as its value; in the mixed kernel it rides as `f64` BITS — a `Value::Int`
            // promoted to `f64` (matching the interpreter's `Int * Float` promotion) or a
            // `Value::Float` passed through. Reinterpreting those 8 bits as `f64` in the kernel
            // is exactly what the codegen's F64 load expects.
            (CaptureKind::ScalarValue, Value::Int(i)) => {
                if float_arrays {
                    caps.push((*i as f64).to_bits() as i64);
                } else {
                    caps.push(*i);
                }
                lens.push(0);
            }
            (CaptureKind::ScalarValue, Value::Float(f)) if float_arrays => {
                caps.push(f.to_bits() as i64);
                lens.push(0);
            }
            // A `Value::Float` value scalar for the i64 kernel (which has no f64 slot for it),
            // a non-`Int`/`Float` scalar, or an array cap bound to a non-array value → fall
            // back rather than guess.
            _ => return None,
        }
    }
    for bnd in &k.index_bounds {
        match *bnd {
            // `a[it]`. The binder is an ELEMENT of the source, not a counter, so this is
            // dischargeable ONLY over a lazy range — there the elements are exactly
            // `start + step*j` for `j in [0, len)`, which is monotone in `j`, so the two
            // ENDPOINTS bound the whole access set. That is the same proof
            // `IndexBound::Counter` uses on the reduce side, with `step` generalizing its
            // unit stride. Computed in `i128` so the CHECK itself cannot overflow.
            IndexBound::Counter { array } => {
                let (start, step, len) = src_range?;
                if len > 0 {
                    let first = start as i128;
                    let last = first + (step as i128) * (len as i128 - 1);
                    let (lo, hi) = if first <= last { (first, last) } else { (last, first) };
                    // `lo < 0` also rejects the NEGATIVE indices the interpreter Python-WRAPS:
                    // wrapping is legal Helix, and the kernel would read off the front instead.
                    if lo < 0 || hi >= lens[array as usize] as i128 {
                        return None;
                    }
                }
                // An empty range accesses nothing: vacuously in bounds.
            }
            // `a[i]` for a loop-invariant scalar `i` — a point check that says nothing about
            // the binder, so unlike `Counter` it holds over ANY source shape.
            IndexBound::Scalar { array, scalar } => {
                let iv = caps[scalar as usize];
                if iv < 0 || iv >= lens[array as usize] {
                    return None;
                }
            }
            // `a[base + coef*elem]` where `elem` ranges over the lazy source's
            // `start + step*j`, `j ∈ [0, len)`. Affine composed with affine is affine in
            // `j`, so the two ENDPOINT indices bound the whole access set — the `Counter`
            // proof with the step generalizing its unit stride and `base`/`coef` riding as
            // pre-evaluated Scalar caps. Computed in CHECKED i128: unlike the reduce's
            // affine (counter ≤ 2^63, so products fit ≤ 2^126), the composed magnitude
            // `coef*(start + step*j)` can exceed even i128 — and a value that large is
            // outside `[0, len)` by definition, so overflow DECLINES, identically to
            // out-of-range. The kernel then evaluates the original index expression in
            // wrapping i64 over the materialized (possibly wrapped) element; mod-2^64 is a
            // ring homomorphism, so wrap(base + coef*wrap(start + step*j)) equals the TRUE
            // i128 value whenever that value lies in [0, len) ⊂ [0, 2^63) — which is
            // exactly what was just checked, and the interpreter's own wrapping arith
            // agrees on every declined case via the fallback loop.
            IndexBound::Affine { array, base, coef } => {
                let (start, step, len) = src_range?;
                if len > 0 {
                    let b0 = caps[base as usize] as i128;
                    let c0 = caps[coef as usize] as i128;
                    let idx_at = |j: i128| -> Option<i128> {
                        let elem = (step as i128).checked_mul(j)?.checked_add(start as i128)?;
                        c0.checked_mul(elem)?.checked_add(b0)
                    };
                    let (Some(first), Some(last)) = (idx_at(0), idx_at(len as i128 - 1)) else {
                        return None;
                    };
                    let (lo, hi) = if first <= last { (first, last) } else { (last, first) };
                    if lo < 0 || hi >= lens[array as usize] as i128 {
                        return None;
                    }
                }
            }
        }
    }
    Some(caps)
}

/// Marshal an UNINDEXED kernel's captures as `i64`: every one must be a `Value::Int` at run
/// time, or the whole map declines. That runtime proof is precisely what lets the mixed
/// analysis type a free scalar as `i64` (see [`crate::jit::mixed_map_eligible`]) — a `Float`
/// in the slot would promote earlier in the kernel than in the interpreter, so declining is
/// the correctness rule, not a missed optimization. An empty list marshals to an empty vec,
/// which is how the capture-free mixed kernels that predate captures keep working unchanged.
fn int_scalar_caps(cap_vals: &[Value]) -> Option<Vec<i64>> {
    cap_vals.iter().map(|v| if let Value::Int(i) = v { Some(*i) } else { None }).collect()
}

/// Marshal captures as `f64` BITS for the value-scalar mixed kernel ("mapmv"): an `Int` is
/// promoted (the interpreter performs the same promotion at the use site — the `MixT`
/// analysis admitted each capture only where a genuine float forces it), a `Float` passes
/// its bits through, anything else declines. The exact `ScalarValue` marshalling
/// `map_index_caps` already uses, extracted for the unindexed variant.
fn value_scalar_caps(cap_vals: &[Value]) -> Option<Vec<i64>> {
    cap_vals
        .iter()
        .map(|v| match v {
            Value::Int(i) => Some((*i as f64).to_bits() as i64),
            Value::Float(f) => Some(f.to_bits() as i64),
            _ => None,
        })
        .collect()
}

/// The lazy-range `map` fast path: run the kernel over counter values GENERATED per chunk instead
/// of over a materialized buffer. `None` means no specialization matched, and the caller falls
/// back to the ordinary route (which materializes, exactly as before).
///
/// This exists because materializing a range purely to be read once left that buffer live
/// alongside the output, so a single `(0..n).map(f)` peaked at TWICE its result — measured 328 MB
/// for 160 MB of payload, and the k1 dot product's documented ~400 MB overhead over C was exactly
/// one such transient. A range's elements are `start + step*j`, so there is nothing to store.
///
/// `cap_vals` is borrowed, not cloned, so the `Rc`s behind any array base pointers stay alive for
/// the duration of the native call — the same lifetime rule the materializing path relies on.
fn try_map_range(
    jit: Option<&crate::jit::Jit>,
    k: &crate::bytecode::ArrayKernel,
    kidx: usize,
    cap_vals: &[Value],
    rng: (i64, i64, usize),
) -> Option<Value> {
    let j = jit?;
    let (start, step, len) = rng;
    // i64 specialization first, mirroring the materializing dispatch's order exactly.
    if let Some(p) = j.map_kernel(kidx) {
        let caps: Option<Vec<i64>> = if k.index_bounds.is_empty() {
            int_scalar_caps(cap_vals)
        } else {
            map_index_caps(k, cap_vals, Some(rng), false)
        };
        if let Some(c) = caps {
            let out = unsafe { crate::jit::run_map_kernel_range(p, start, step, len, &c) };
            return Some(Value::int_array(out));
        }
    }
    // then the mixed specialization (i64 elements -> f64 output).
    if let Some(p) = j.map_kernel_mixed(kidx) {
        if k.index_bounds.is_empty()
            && let Some(c) = int_scalar_caps(cap_vals)
        {
            // A RAISING body (rounder inside) takes the poison range wrapper; `None` means
            // some element left i64 range, and returning None here falls through to the
            // materializing path, whose poison wrapper declines again into the bytecode
            // loop — which re-runs and raises the exact interpreter error.
            if k.raises {
                let out = unsafe {
                    crate::jit::run_map_kernel_range_mixed_poison(p, start, step, len, &c)
                };
                return out.map(Value::float_array);
            }
            let out = unsafe { crate::jit::run_map_kernel_mixed_range(p, start, step, len, &c) };
            return Some(Value::float_array(out));
        }
        if !k.index_bounds.is_empty()
            && let Some(c) = map_index_caps(k, cap_vals, Some(rng), true)
        {
            // An indexed body can now RAISE too (division since Stage 3x): same poison
            // routing as the unindexed arm.
            if k.raises {
                let out = unsafe {
                    crate::jit::run_map_kernel_range_mixed_poison(p, start, step, len, &c)
                };
                return out.map(Value::float_array);
            }
            let out = unsafe { crate::jit::run_map_kernel_mixed_range(p, start, step, len, &c) };
            return Some(Value::float_array(out));
        }
    }
    // The VALUE-SCALAR variant: reached when the Int-proven marshal above declined because
    // some capture is a runtime `Float`. Same ABI as the mixed kernel; captures ride as f64
    // bits via `value_scalar_caps`.
    if let Some(p) = j.map_kernel_mixed_value(kidx)
        && k.index_bounds.is_empty()
        && let Some(c) = value_scalar_caps(cap_vals)
    {
        if k.raises {
            let out =
                unsafe { crate::jit::run_map_kernel_range_mixed_poison(p, start, step, len, &c) };
            return out.map(Value::float_array);
        }
        let out = unsafe { crate::jit::run_map_kernel_mixed_range(p, start, step, len, &c) };
        return Some(Value::float_array(out));
    }
    // The Int-ROOTED mixed specialization (i64 out through Float intermediates). Its ABI is
    // the plain i64 kernel's, so the i64 range runner produces the `Ints` result directly —
    // unless the body RAISES, in which case the poison signature and wrapper apply.
    if let Some(p) = j.map_kernel_mixed_int(kidx)
        && k.index_bounds.is_empty()
        && let Some(c) = int_scalar_caps(cap_vals)
    {
        if k.raises {
            let out =
                unsafe { crate::jit::run_map_kernel_range_int_poison(p, start, step, len, &c) };
            return out.map(Value::int_array);
        }
        let out = unsafe { crate::jit::run_map_kernel_range(p, start, step, len, &c) };
        return Some(Value::int_array(out));
    }
    None
}

/// Active comprehension iterator state (a stack, so comprehensions nest).
/// `cur_val` is the element just yielded (used by `filter`); `builder` collects
/// results for `map`/`filter` and is ignored by `reduce`.
struct CompIter {
    source: CompSource,
    cur_val: Value,
    builder: crate::value::ColumnBuilder,
}

/// One active function invocation.
struct Frame {
    /// Index into [`Program::funcs`] of the code being run.
    func: usize,
    /// Instruction pointer within that function's `code`.
    ip: usize,
    /// Start of this frame's locals within the shared `locals` stack.
    base: usize,
    /// If set, this call is a memoization miss: store its return value under this
    /// key when the frame returns.
    memo_key: Option<MemoKey>,
    /// The captured environment of the closure being run (empty for a plain
    /// function). `GetUpvalue` reads these; `MakeClosure` may copy from them.
    upvalues: std::rc::Rc<Vec<Value>>,
}

/// An active `try` error handler (from `Op::TryBegin`). Records the exact depths of
/// every VM stack at the point the `try` was entered, so an error anywhere inside —
/// however deeply nested the call chain — unwinds back to here and resumes at
/// `catch_ip`. A `Vec` of these is a LIFO stack, so the innermost `try` catches first.
struct Handler {
    stack_len: usize,
    frame_depth: usize,
    locals_len: usize,
    iters_len: usize,
    catch_ip: usize,
}

/// Read a tuple/record accumulator's N `i64` slots into a buffer (field/element order),
/// or `None` if it isn't an N-element Tuple/Record of all `Int`s — the marshalling for the
/// native multi-slot fold kernel (tuple and record accumulators share the kernel).
fn acc_to_slots(v: &Value, n: usize) -> Option<Vec<i64>> {
    let items: &[Value] = match v {
        Value::Tuple(t) if t.len() == n => t,
        Value::Record(r) if r.len() == n => {
            let mut buf = Vec::with_capacity(n);
            for (_, el) in r.iter() {
                match el {
                    Value::Int(i) => buf.push(*i),
                    _ => return None,
                }
            }
            return Some(buf);
        }
        _ => return None,
    };
    let mut buf = Vec::with_capacity(n);
    for el in items.iter() {
        match el {
            Value::Int(i) => buf.push(*i),
            _ => return None,
        }
    }
    Some(buf)
}

/// Rebuild an accumulator value from the folded slots, matching `template`'s shape: a
/// Record reuses its field symbols (in order), anything else becomes a Tuple.
fn rebuild_acc(template: &Value, buf: Vec<i64>) -> Value {
    match template {
        Value::Record(r) => {
            let fields: Vec<(crate::symbol::Symbol, Value)> =
                r.iter().zip(buf).map(|((sym, _), i)| (*sym, Value::Int(i))).collect();
            Value::Record(std::rc::Rc::new(fields))
        }
        _ => Value::Tuple(std::rc::Rc::new(buf.into_iter().map(Value::Int).collect())),
    }
}

/// Like [`acc_to_slots`], but for an **all-`Float`** accumulator: each slot's f64 bit pattern
/// is packed into the `i64` buffer (the f64-tuple kernel reads/writes these 8-byte slots as
/// `f64` at the same addresses). A non-`Float` slot falls back. Keeping the buffer `Vec<i64>`
/// reuses the existing tuple-reduce runner ABI (`*mut i64`) unchanged.
fn acc_to_slots_f64(v: &Value, n: usize) -> Option<Vec<i64>> {
    let items: &[Value] = match v {
        Value::Tuple(t) if t.len() == n => t,
        Value::Record(r) if r.len() == n => {
            let mut buf = Vec::with_capacity(n);
            for (_, el) in r.iter() {
                match el {
                    Value::Float(f) => buf.push(f.to_bits() as i64),
                    _ => return None,
                }
            }
            return Some(buf);
        }
        _ => return None,
    };
    let mut buf = Vec::with_capacity(n);
    for el in items.iter() {
        match el {
            Value::Float(f) => buf.push(f.to_bits() as i64),
            _ => return None,
        }
    }
    Some(buf)
}

/// Rebuild an **all-`Float`** accumulator from the folded slot bit patterns (`f64::from_bits`),
/// matching `template`'s shape (Record reuses its field symbols, else Tuple).
fn rebuild_acc_f64(template: &Value, buf: Vec<i64>) -> Value {
    let f = |i: i64| Value::Float(f64::from_bits(i as u64));
    match template {
        Value::Record(r) => {
            let fields: Vec<(crate::symbol::Symbol, Value)> =
                r.iter().zip(buf).map(|((sym, _), i)| (*sym, f(i))).collect();
            Value::Record(std::rc::Rc::new(fields))
        }
        _ => Value::Tuple(std::rc::Rc::new(buf.into_iter().map(f).collect())),
    }
}

/// Execute a compiled program, printing output and returning the first runtime
/// error (if any) for the caller to render. `jit`, when present, supplies native
/// code for eligible integer functions.
pub fn run(program: &Program, jit: Option<&crate::jit::Jit>) -> Result<(), HelixError> {
    exec(program, jit).map(|_| ())
}

/// The execution core. Returns the final operand stack so tests can inspect the
/// last computed value; [`run`] discards it.
fn exec(program: &Program, jit: Option<&crate::jit::Jit>) -> Result<Vec<Value>, HelixError> {
    // Resolve each user function index to its native entry points once (if
    // JIT'd), so the hot `CallFn` path is a single array lookup.
    let jit_for_idx: Vec<Option<crate::jit::NativeFn>> = program
        .func_names
        .iter()
        .map(|n| jit.and_then(|j| j.lookup(n)))
        .collect();
    // Tail loops compiled with the globals they read as trailing parameters. The capture
    // NAMES are resolved to global slots once, here, so the hot path is an index — and a
    // name that is not a global disables the specialization rather than guessing at it.
    let cap_for_idx: Vec<Option<(*const u8, Vec<usize>, usize)>> = program
        .func_names
        .iter()
        .map(|n| {
            let j = jit?;
            let (ptr, caps, arity) = j.capture_loop(n)?;
            let slots: Option<Vec<usize>> = caps
                .iter()
                .map(|c| program.global_names.iter().position(|g| g == c))
                .collect();
            Some((ptr, slots?, arity))
        })
        .collect();
    // A throwaway interpreter purely as the host for builtin dispatch (`print`,
    // math fns, `read_csv`, …) — builtins are pure functions of their args.
    let mut host = Interp::new();

    let mut globals: Vec<Value> = program.global_init.clone();
    let mut stack: Vec<Value> = Vec::with_capacity(256);
    let mut locals: Vec<Value> = Vec::with_capacity(256);
    let mut frames: Vec<Frame> = Vec::with_capacity(64);
    // Automatic memoization of pure, overlapping-recursive functions — the
    // "under the hood" cache. Safe because memoizable functions (per the
    // bytecode analysis) are pure functions of their integer arguments.
    let mut memo: FxHashMap<MemoKey, Value> = FxHashMap::default();
    let mut iters: Vec<CompIter> = Vec::new();
    // Active `try` handlers (LIFO). Empty in the overwhelmingly common case; an
    // error only consults it, so non-`try` programs pay nothing.
    let mut handlers: Vec<Handler> = Vec::new();
    // Shared empty upvalue list for every non-closure frame — cloning is a refcount
    // bump, so a plain call allocates nothing for upvalues.
    let no_upvalues: std::rc::Rc<Vec<Value>> = std::rc::Rc::new(Vec::new());

    let main = &program.funcs[0];
    locals.resize(main.n_locals as usize, Value::Unit);
    frames.push(Frame { func: 0, ip: 0, base: 0, memo_key: None, upvalues: no_upvalues.clone() });

    loop {
        let fi = frames.len() - 1;
        let func = frames[fi].func;
        let chunk = &program.funcs[func];
        let ip = frames[fi].ip;
        if ip >= chunk.code.len() {
            // Only `main` runs off the end (functions terminate with `Return`).
            break;
        }
        // Borrow the instruction rather than cloning it — this loop runs once
        // per executed op (tens of millions of times in a hot recursion), so a
        // per-dispatch clone is pure waste.
        let op = &chunk.code[ip];
        let (line, col) = {
            let (l, c) = chunk.pos[ip];
            (l as usize, c as usize)
        };
        frames[fi].ip = ip + 1; // default advance; control-flow ops overwrite it

        // Run the op inside a closure so an error can be CAUGHT (routed to the
        // nearest `try` handler) instead of propagating straight out of `exec`. An
        // in-arm `continue` becomes `return Ok(())` (nothing runs after the match, so
        // they're equivalent); a `return Err(..)` becomes the closure's error. LLVM
        // inlines this single-call closure, so the dispatch loop stays zero-overhead.
        let step: Result<(), HelixError> = (|| {
        match op {
            Op::Const(k) => stack.push(chunk.consts[*k as usize].clone()),
            Op::LoadLocal(slot) => {
                let base = frames[fi].base;
                stack.push(locals[base + *slot as usize].clone());
            }
            Op::ConcatIntoLocal(slot) => {
                // Cannot panic: `emit_reduce_body_and_store` compiles the argument
                // expression immediately before emitting this op, so exactly one value is on
                // the stack for it — the same compiler-maintained stack-shape invariant every
                // other op in this loop relies on for its own `pop`.
                let arg = stack.pop().unwrap();
                let base = frames[fi].base;
                let at = base + *slot as usize;
                // VALIDATE BEFORE TAKING. Both error paths must read the accumulator while
                // it is still in its slot, so a failing `concat` leaves the frame exactly as
                // the ordinary lowering would have — and word-for-word the same, because a
                // divergence here would be an error-text divergence, which the oracle counts.
                let Value::Array(add) = &arg else {
                    return Err(HelixError::new(
                        format!(
                            "`concat` expects arrays, but argument 1 is {}",
                            crate::value::with_article(arg.type_name())
                        ),
                        line,
                        col,
                    ));
                };
                if !matches!(locals[at], Value::Array(_)) {
                    return Err(HelixError::new(
                        format!(
                            "{} has no method `concat`",
                            crate::value::with_article(locals[at].type_name())
                        ),
                        line,
                        col,
                    ));
                }
                let add = add.clone();
                // Now the take: the slot's `Rc` loses its second owner, so an accumulator
                // nothing else aliases becomes unique and can be extended in place.
                let Value::Array(cur) = std::mem::replace(&mut locals[at], Value::Unit) else {
                    unreachable!("accumulator type checked immediately above")
                };
                locals[at] = Value::concat_in_place(cur, &add);
            }
            Op::InsertIntoLocal(slot) => {
                // Pushed key-then-value, so the value pops first. Both pops are guaranteed
                // by the compiler, exactly as in `ConcatIntoLocal`.
                let v = stack.pop().unwrap();
                let kv = stack.pop().unwrap();
                let base = frames[fi].base;
                let at = base + *slot as usize;
                // Validated before the take, so a bad key or a non-Dict accumulator leaves
                // the slot untouched and reports what the ordinary lowering reports.
                if !matches!(locals[at], Value::Dict(_)) {
                    return Err(HelixError::new(
                        format!(
                            "{} has no method `insert`",
                            crate::value::with_article(locals[at].type_name())
                        ),
                        line,
                        col,
                    ));
                }
                let k = crate::value::DictKey::from_value(&kv)
                    .map_err(|m| HelixError::new(m, line, col))?;
                let Value::Dict(cur) = std::mem::replace(&mut locals[at], Value::Unit) else {
                    unreachable!("accumulator type checked immediately above")
                };
                locals[at] = Value::insert_in_place(cur, k, v);
            }
            Op::StoreLocal(slot) => {
                let base = frames[fi].base;
                locals[base + *slot as usize] = stack.pop().unwrap();
            }
            Op::LoadGlobal(i) => stack.push(globals[*i as usize].clone()),
            Op::StoreGlobal(i) => globals[*i as usize] = stack.pop().unwrap(),
            Op::Unary(o) => {
                let v = stack.pop().unwrap();
                stack.push(host.eval_unary(o, v, line, col)?);
            }
            Op::Binary(o) => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(binary(o, a, b, line, col)?);
            }
            Op::LoadLocalBinary(a, c, o) => {
                let base = frames[fi].base;
                let av = locals[base + *a as usize].clone();
                let cv = locals[base + *c as usize].clone();
                stack.push(binary(o, av, cv, line, col)?);
            }
            Op::LoadLocalConstBinary(a, k, o) => {
                let base = frames[fi].base;
                let av = locals[base + *a as usize].clone();
                let cv = chunk.consts[*k as usize].clone();
                stack.push(binary(o, av, cv, line, col)?);
            }
            Op::ConstBinary(k, o) => {
                let v = stack.pop().unwrap();
                let cv = chunk.consts[*k as usize].clone();
                stack.push(binary(o, v, cv, line, col)?);
            }
            Op::Jump(t) => frames[fi].ip = *t as usize,
            Op::MatchArm(pat) => {
                // Reuse the tree-walker's matcher so the engines agree exactly. On a
                // match, push the bound values then `true`; else push `false`.
                let v = stack.pop().unwrap();
                match crate::interp::pattern_match(pat, &v) {
                    Some(binds) => {
                        for (_, val) in binds {
                            stack.push(val);
                        }
                        stack.push(Value::Bool(true));
                    }
                    None => stack.push(Value::Bool(false)),
                }
            }
            Op::JumpIfFalse(t) => {
                let c = stack.pop().unwrap();
                if matches!(c, Value::Missing) {
                    return Err(HelixError::new(
                        "`if` condition is `missing` — cannot choose a branch",
                        line,
                        col,
                    )
                    .hint("handle the missing case first, e.g. `if x.is_missing() then ... else ...`."));
                }
                let taken = as_bool(&c, line, col).map_err(|e| {
                    e.hint("an `if` condition must be a boolean, e.g. `if x > 0 then ... else ...`.")
                })?;
                if !taken {
                    frames[fi].ip = *t as usize;
                }
            }
            // `JumpIfFalse` for a `match` guard: same control flow, but the
            // guard-specific error wording, shared with the walker.
            Op::GuardCheck(t) => {
                let c = stack.pop().unwrap();
                if !crate::interp::guard_bool(&c, line, col)? {
                    frames[fi].ip = *t as usize;
                }
            }
            // Three-valued `and`: short-circuit on a determined `false`.
            Op::AndCheck(end) => {
                let ta = tri(stack.last().unwrap(), line, col)?;
                if ta == Some(false) {
                    *stack.last_mut().unwrap() = Value::Bool(false);
                    frames[fi].ip = *end as usize;
                }
            }
            Op::AndCombine => {
                let rb = stack.pop().unwrap();
                let ra = stack.pop().unwrap();
                let tb = tri(&rb, line, col)?;
                let ta = tri(&ra, line, col)?;
                stack.push(match (ta, tb) {
                    (_, Some(false)) => Value::Bool(false),
                    (Some(true), Some(true)) => Value::Bool(true),
                    _ => Value::Missing,
                });
            }
            // Three-valued `or`: short-circuit on a determined `true`.
            Op::OrCheck(end) => {
                let ta = tri(stack.last().unwrap(), line, col)?;
                if ta == Some(true) {
                    *stack.last_mut().unwrap() = Value::Bool(true);
                    frames[fi].ip = *end as usize;
                }
            }
            Op::OrCombine => {
                let rb = stack.pop().unwrap();
                let ra = stack.pop().unwrap();
                let tb = tri(&rb, line, col)?;
                let ta = tri(&ra, line, col)?;
                stack.push(match (ta, tb) {
                    (_, Some(true)) => Value::Bool(true),
                    (Some(false), Some(false)) => Value::Bool(false),
                    _ => Value::Missing,
                });
            }
            // `a ?? b`: keep `a` unless it's missing, in which case evaluate `b`.
            Op::CoalesceCheck(end) => {
                if matches!(stack.last().unwrap(), Value::Missing) {
                    stack.pop(); // drop the missing; fall through to `b`
                } else {
                    frames[fi].ip = *end as usize; // keep `a`, skip `b`
                }
            }
            Op::CallBuiltin { idx, nargs } => {
                let split = stack.len() - *nargs as usize;
                let args = stack.split_off(split);
                let name = &program.builtins[*idx as usize];
                let v = host.call_builtin(name, args, line, col)?;
                stack.push(v);
            }
            Op::CallFn { idx, nargs } => {
                let idx = *idx as usize;
                let nargs = *nargs as usize;
                let start = stack.len() - nargs;

                // Memoization fast path (preferred over the JIT for the pure,
                // overlapping-recursive functions the analysis flagged): a cache
                // hit returns instantly; a miss runs the bytecode body so its
                // recursive calls also hit this path, then stores the result on
                // return. This turns exponential recursion (e.g. `fib`) linear —
                // for integer *and* float arguments.
                if program.memoizable[idx]
                    && stack[start..]
                        .iter()
                        .all(|v| matches!(v, Value::Int(_) | Value::Float(_)))
                {
                    let kargs = &stack[start..];
                    let key = match kargs.len() {
                        1 => MemoKey::A1(idx, memo_arg(&kargs[0])),
                        2 => MemoKey::A2(idx, memo_arg(&kargs[0]), memo_arg(&kargs[1])),
                        _ => MemoKey::An(idx, kargs.iter().map(memo_arg).collect()),
                    };
                    if let Some(cached) = memo.get(&key) {
                        let cached = cached.clone();
                        stack.truncate(start);
                        stack.push(cached);
                        return Ok(());
                    }
                    let callee = &program.funcs[idx];
                    if nargs != callee.n_params as usize {
                        let np = callee.n_params as usize;
                        return Err(HelixError::new(
                            format!(
                                "`{}` expects {} argument{}, got {}",
                                program.func_names[idx],
                                np,
                                if np == 1 { "" } else { "s" },
                                nargs
                            ),
                            line,
                            col,
                        ));
                    }
                    if frames.len() > VM_MAX_DEPTH {
                        return Err(HelixError::new(
                            format!("maximum recursion depth ({}) exceeded", VM_MAX_DEPTH),
                            line,
                            col,
                        )
                        .hint("is the recursion missing a base case, or should this be a loop/comprehension?"));
                    }
                    let base = locals.len();
                    locals.extend(stack.drain(start..));
                    locals.resize(base + callee.n_locals as usize, Value::Unit);
                    frames.push(Frame { func: idx, ip: 0, base, memo_key: Some(key), upvalues: no_upvalues.clone() });
                    return Ok(());
                }

                // Native fast path: dispatch to the specialization matching the
                // argument types. All-Int + an i64 version → native i64 (Int
                // result). Otherwise all-numeric + an f64 version → native f64
                // (Float result; the float-only or mixed/float case). All of the
                // function's internal recursion then stays native.
                if let Some(nf) = jit_for_idx[idx]
                    && nargs == nf.arity
                {
                    let tail = &stack[start..];
                    // The f64 specialization always returns Float, so it is only
                    // valid when EVERY argument is Float (then every op, and
                    // returning a param, yields Float — matching the interpreter).
                    // For MIXED Int/Float args the result type depends on what the
                    // function does (e.g. `f(a,b)=b` keeps an Int `b`), so those
                    // fall through to the VM, which handles type-mixing correctly.
                    // These scans live here (not at the top of CallFn) so the
                    // common non-JIT / memoized call pays nothing for them.
                    let all_int = tail.iter().all(|v| matches!(v, Value::Int(_)));
                    let all_float = tail.iter().all(|v| matches!(v, Value::Float(_)));
                    if all_int && let Some(ptr) = nf.i64_ptr {
                        let iargs: Vec<i64> = tail
                            .iter()
                            .map(|v| if let Value::Int(n) = v { *n } else { 0 })
                            .collect();
                        stack.truncate(start);
                        let r = unsafe { crate::jit::call_i64(ptr, &iargs) };
                        stack.push(Value::Int(r));
                        return Ok(());
                    }
                    if all_float && let Some(ptr) = nf.f64_ptr {
                        let fargs: Vec<f64> = tail.iter().map(|v| v.as_f64().unwrap()).collect();
                        stack.truncate(start);
                        let r = unsafe { crate::jit::call_f64(ptr, &fargs) };
                        stack.push(Value::Float(r));
                        return Ok(());
                    }
                    // MIXED per-parameter specialization (annotation-typed tail-loop
                    // fns): taken only when every argument's RUNTIME type matches the
                    // compiled pattern — Float params cross the FFI as raw f64 bits in
                    // i64 slots, and a Float result comes back as bits (pure bit moves,
                    // bit-exact). Any other type pattern falls through to the VM, which
                    // handles dynamic mixing (and ignores annotations) as always. The
                    // trailing slot is the NaN-poison out-param (see `MixedFn`): the
                    // native code bails there on an unordered float compare, in which
                    // case the result is DISCARDED (stack untouched) and the ordinary
                    // bytecode call below re-runs and raises the interpreter's exact
                    // "cannot compare these values (NaN?)" error.
                    if let Some(m) = nf.mixed
                        && tail.iter().enumerate().all(|(j, v)| {
                            if m.float_mask >> j & 1 == 1 {
                                matches!(v, Value::Float(_))
                            } else {
                                matches!(v, Value::Int(_))
                            }
                        })
                    {
                        let mut iargs: Vec<i64> = tail
                            .iter()
                            .map(|v| match v {
                                Value::Int(n) => *n,
                                Value::Float(x) => x.to_bits() as i64,
                                _ => unreachable!("pattern checked above"),
                            })
                            .collect();
                        let mut poison: i8 = 0;
                        iargs.push(&raw mut poison as i64);
                        let r = unsafe { crate::jit::call_i64(m.ptr, &iargs) };
                        if poison == 0 {
                            stack.truncate(start);
                            stack.push(if m.ret_float {
                                Value::Float(f64::from_bits(r as u64))
                            } else {
                                Value::Int(r)
                            });
                            return Ok(());
                        }
                    }
                }
                // A tail loop that reads globals. Tried after the capture-free
                // specializations, so a function that compiled without captures keeps its
                // cheaper entry point. Every argument must be `Int` (same rule as the i64
                // path) AND every captured global must be `Int` right now — a global that
                // is missing, a Float, or not yet initialized declines to the VM, which
                // handles it correctly as always. Reading the globals HERE is what makes
                // this sound: nothing else runs during the native call, so a capture is
                // loop-invariant for the whole loop.
                if let Some((ptr, slots, arity)) = &cap_for_idx[idx]
                    && nargs == *arity
                {
                    let tail = &stack[start..];
                    if tail.iter().all(|v| matches!(v, Value::Int(_))) {
                        let mut iargs: Vec<i64> = Vec::with_capacity(nargs + slots.len());
                        for v in tail {
                            if let Value::Int(n) = v {
                                iargs.push(*n);
                            }
                        }
                        let all_int_caps = slots.iter().all(|s| {
                            if let Some(Value::Int(n)) = globals.get(*s) {
                                iargs.push(*n);
                                true
                            } else {
                                false
                            }
                        });
                        if all_int_caps {
                            stack.truncate(start);
                            let r = unsafe { crate::jit::call_i64(*ptr, &iargs) };
                            stack.push(Value::Int(r));
                            return Ok(());
                        }
                    }
                }
                let callee = &program.funcs[idx];
                if nargs != callee.n_params as usize {
                    let np = callee.n_params as usize;
                    return Err(HelixError::new(
                        format!(
                            "`{}` expects {} argument{}, got {}",
                            program.func_names[idx],
                            np,
                            if np == 1 { "" } else { "s" },
                            nargs
                        ),
                        line,
                        col,
                    ));
                }
                if frames.len() > VM_MAX_DEPTH {
                    return Err(HelixError::new(
                        format!("maximum recursion depth ({}) exceeded", VM_MAX_DEPTH),
                        line,
                        col,
                    )
                    .hint("is the recursion missing a base case, or should this be a loop/comprehension?"));
                }
                let base = locals.len();
                // `drain` moves the args straight into the callee's locals with
                // no intermediate allocation (vs `split_off`'s fresh `Vec`).
                locals.extend(stack.drain(start..));
                locals.resize(base + callee.n_locals as usize, Value::Unit);
                frames.push(Frame { func: idx, ip: 0, base, memo_key: None, upvalues: no_upvalues.clone() });
            }
            Op::TailCallFn { idx, nargs } => {
                let idx = *idx as usize;
                let nargs = *nargs as usize;
                let callee = &program.funcs[idx];
                if nargs != callee.n_params as usize {
                    let np = callee.n_params as usize;
                    return Err(HelixError::new(
                        format!(
                            "`{}` expects {} argument{}, got {}",
                            program.func_names[idx],
                            np,
                            if np == 1 { "" } else { "s" },
                            nargs
                        ),
                        line,
                        col,
                    ));
                }
                // Native fast path — the same specialization dispatch as `CallFn`.
                // Without it, a tail call INTO a JIT-compiled function (`fn escape(..) =
                // step(..)`, the natural wrapper idiom) silently ran the callee on the
                // interpreter. The current frame is dead (tail position), so a native
                // result is delivered exactly as `Op::Return` would deliver it: pop the
                // frame, truncate its locals, push the value — the caller resumes at its
                // already-advanced ip. (`TailCallFn` can never appear in `main`: the
                // peephole requires a `Return` successor and `main` has none, so the pop
                // always leaves the caller frame.) Like the frame-reuse path below, the
                // dead frame's `memo_key` obligation is dropped — memoization is a
                // pure-function cache, unobservable in values.
                if let Some(nf) = jit_for_idx[idx]
                    && nargs == nf.arity
                {
                    let start = stack.len() - nargs;
                    let tail = &stack[start..];
                    let all_int = tail.iter().all(|v| matches!(v, Value::Int(_)));
                    let all_float = tail.iter().all(|v| matches!(v, Value::Float(_)));
                    let native: Option<Value> = if all_int && let Some(ptr) = nf.i64_ptr {
                        let iargs: Vec<i64> = tail
                            .iter()
                            .map(|v| if let Value::Int(n) = v { *n } else { 0 })
                            .collect();
                        Some(Value::Int(unsafe { crate::jit::call_i64(ptr, &iargs) }))
                    } else if all_float && let Some(ptr) = nf.f64_ptr {
                        let fargs: Vec<f64> = tail.iter().map(|v| v.as_f64().unwrap()).collect();
                        Some(Value::Float(unsafe { crate::jit::call_f64(ptr, &fargs) }))
                    } else if let Some(m) = nf.mixed
                        && tail.iter().enumerate().all(|(j, v)| {
                            if m.float_mask >> j & 1 == 1 {
                                matches!(v, Value::Float(_))
                            } else {
                                matches!(v, Value::Int(_))
                            }
                        })
                    {
                        let mut iargs: Vec<i64> = tail
                            .iter()
                            .map(|v| match v {
                                Value::Int(n) => *n,
                                Value::Float(x) => x.to_bits() as i64,
                                _ => unreachable!("pattern checked above"),
                            })
                            .collect();
                        // NaN-poison slot (see `MixedFn`): a poisoned result is
                        // discarded (`None`) so the frame-reuse path below re-runs the
                        // call on bytecode and raises the interpreter's exact error.
                        let mut poison: i8 = 0;
                        iargs.push(&raw mut poison as i64);
                        let r = unsafe { crate::jit::call_i64(m.ptr, &iargs) };
                        if poison == 0 {
                            Some(if m.ret_float {
                                Value::Float(f64::from_bits(r as u64))
                            } else {
                                Value::Int(r)
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(ret) = native {
                        stack.truncate(start);
                        let frame = frames.pop().unwrap();
                        locals.truncate(frame.base);
                        stack.push(ret);
                        return Ok(());
                    }
                }
                // Reuse the CURRENT frame (`fi`) rather than pushing a new one — the call is
                // in tail position, so this frame is dead. Discard its locals, move the
                // already-evaluated args into the callee's parameter slots, and re-point the
                // frame at the callee from ip 0. `frames`/`locals` never grow, so tail
                // recursion (an accept loop, a state machine) is constant-space and can't hit
                // VM_MAX_DEPTH. `ip = 0` overrides the loop's default `ip + 1` advance.
                let start = stack.len() - nargs;
                let base = frames[fi].base;
                locals.truncate(base);
                locals.extend(stack.drain(start..));
                locals.resize(base + callee.n_locals as usize, Value::Unit);
                let frame = &mut frames[fi];
                frame.func = idx;
                frame.ip = 0;
                frame.memo_key = None;
                frame.upvalues = no_upvalues.clone();
            }
            Op::MakeFunc { idx, arity } => {
                stack.push(Value::VmFunc { idx: *idx, arity: *arity });
            }
            Op::MakeClosure(d) => {
                // Capture each upvalue's value from the current frame: an enclosing
                // local (`base + slot`) or one of this frame's own upvalues.
                let frame = frames.last().unwrap();
                let captured: Vec<Value> = d
                    .captures
                    .iter()
                    .map(|src| match src {
                        crate::bytecode::CaptureSrc::Local(slot) => {
                            locals[frame.base + *slot as usize].clone()
                        }
                        crate::bytecode::CaptureSrc::Upvalue(i) => {
                            frame.upvalues[*i as usize].clone()
                        }
                    })
                    .collect();
                stack.push(Value::Closure(std::rc::Rc::new(crate::value::ClosureData {
                    idx: d.idx,
                    arity: d.arity,
                    upvalues: std::rc::Rc::new(captured),
                })));
            }
            Op::GetUpvalue(i) => {
                let v = frames.last().unwrap().upvalues[*i as usize].clone();
                stack.push(v);
            }
            Op::CallValue(d) => {
                let name = &d.name;
                let nargs = d.nargs as usize;
                let start = stack.len() - nargs;
                // The function value sits just below the args (loaded first). A
                // plain `VmFunc` has no upvalues; a `Closure` carries its captured
                // environment, which becomes the new frame's upvalues.
                let (idx, frame_upvalues) = match &stack[start - 1] {
                    Value::VmFunc { idx, .. } => (*idx as usize, no_upvalues.clone()),
                    Value::Closure(c) => (c.idx as usize, c.upvalues.clone()),
                    other => {
                        return Err(HelixError::new(
                            format!("`{}` is {}, not a function", name, crate::value::with_article(other.type_name())),
                            line,
                            col,
                        )
                        .hint("only functions and the built-ins `print`/`dna`/`range` can be called."));
                    }
                };
                let callee = &program.funcs[idx];
                if nargs != callee.n_params as usize {
                    let np = callee.n_params as usize;
                    return Err(HelixError::new(
                        format!(
                            "`{}` expects {} argument{}, got {}",
                            name,
                            np,
                            if np == 1 { "" } else { "s" },
                            nargs
                        ),
                        line,
                        col,
                    ));
                }
                if frames.len() > VM_MAX_DEPTH {
                    return Err(HelixError::new(
                        format!("maximum recursion depth ({}) exceeded", VM_MAX_DEPTH),
                        line,
                        col,
                    )
                    .hint("is the recursion missing a base case, or should this be a loop/comprehension?"));
                }
                let base = locals.len();
                locals.extend(stack.drain(start..)); // args → callee locals
                stack.pop(); // discard the function value (now on top)
                locals.resize(base + callee.n_locals as usize, Value::Unit);
                frames.push(Frame { func: idx, ip: 0, base, memo_key: None, upvalues: frame_upvalues });
            }
            Op::Return => {
                let ret = stack.pop().unwrap();
                let frame = frames.pop().unwrap();
                locals.truncate(frame.base);
                // A memoization miss: record the result so future calls with the same
                // arguments return instantly. Bounded for safety — but on overflow, CLEAR and
                // start fresh rather than freezing: a frozen cache would pin 5M values in RAM
                // forever *and* stop memoizing, so a long-running process would lose the
                // speedup. A periodic reset keeps both memory and memoization healthy.
                if let Some(key) = frame.memo_key {
                    if memo.len() >= MEMO_MAX_ENTRIES {
                        memo.clear();
                    }
                    memo.insert(key, ret.clone());
                }
                stack.push(ret);
            }
            Op::MakeArray(n) => {
                let start = stack.len() - *n as usize;
                let items: Vec<Value> = stack.split_off(start);
                stack.push(Value::array_sniff(items));
            }
            Op::Index => {
                let idx = stack.pop().unwrap();
                let recv = stack.pop().unwrap();
                stack.push(crate::interp::eval_index(&recv, &idx, line, col)?);
            }
            Op::Interp(parts) => {
                // The hole values are READ IN PLACE off the stack and dropped in one
                // `truncate` at the end. `split_off` minted a fresh `Vec<Value>` for
                // every string built — one malloc/free per element of a 5M `map`, for
                // a buffer that never outlives the op. Leaving the operands on the
                // stack through the fallible middle is safe: `Handler` records the
                // stack depth at `try` entry and the catch truncates back to it, so an
                // error here unwinds exactly as it did before.
                let mut holes = 0usize;
                let mut cap = 0usize;
                for p in parts.iter() {
                    match p {
                        crate::ast::InterpPart::Lit(t) => cap += t.len(),
                        crate::ast::InterpPart::Expr(..) => holes += 1,
                    }
                }
                let base = stack.len() - holes;
                // One sized allocation instead of growing from empty. The per-hole
                // estimate is FOUR, not sixteen: `Value::Str` is an `Rc<String>`, so
                // whatever capacity this asks for is retained for the life of the
                // string, and asking 16 per hole put `"w{n}"` — five bytes — in a
                // 32-byte size class instead of an 8-byte one. Measured at 5M strings:
                // peak RSS 517 MB -> 411 MB, and a ~105-byte-per-string program went
                // 0.99s -> 0.77s of child CPU. Under-reserving is cheap (push_str grows
                // amortized); over-reserving is charged to every string that survives.
                let mut s = String::with_capacity(cap + holes * 4);
                let mut vi = base;
                for part in parts.iter() {
                    match part {
                        crate::ast::InterpPart::Lit(t) => s.push_str(t),
                        crate::ast::InterpPart::Expr(e, spec) => {
                            // Report at the hole expression's position, exactly like
                            // the walker (the parser relocates hole positions to the
                            // interpolated string's real source coordinates).
                            let (el, ec) = e.position();
                            match spec {
                                Some(fs) => s.push_str(
                                    &fs.apply(&stack[vi]).map_err(|m| HelixError::new(m, el, ec))?,
                                ),
                                // Hot path: format scalars straight into `s`, no throwaway String.
                                None => crate::value::write_value(&mut s, &stack[vi], el, ec)?,
                            }
                            vi += 1;
                        }
                    }
                    // Mirror the tree-walker's cap so a doubling loop errors cleanly
                    // and identically on both engines (parity) instead of aborting.
                    if s.len() > crate::interp::MAX_STRING_LEN {
                        return Err(HelixError::new(
                            format!(
                                "interpolated string exceeds {} bytes",
                                crate::interp::MAX_STRING_LEN
                            ),
                            line,
                            col,
                        )
                        .hint("build large text incrementally or write it to a file instead."));
                    }
                }
                stack.truncate(base);
                stack.push(Value::Str(std::rc::Rc::new(s)));
            }
            Op::MakeTuple(n) => {
                let start = stack.len() - *n as usize;
                let items: Vec<Value> = stack.split_off(start);
                stack.push(Value::Tuple(std::rc::Rc::new(items)));
            }
            Op::MakeRecord(names) => {
                let start = stack.len() - names.len();
                let vals: Vec<Value> = stack.split_off(start);
                // `names.iter().copied()` copies the interned `Symbol`s (a `u32`
                // each) — no per-record key allocation.
                let fields: Vec<(crate::symbol::Symbol, Value)> =
                    names.iter().copied().zip(vals).collect();
                stack.push(Value::Record(std::rc::Rc::new(fields)));
            }
            Op::UpdateRecord(names) => {
                let start = stack.len() - names.len();
                let vals: Vec<Value> = stack.split_off(start);
                let base = stack.pop().unwrap();
                let Value::Record(base_fields) = base else {
                    return Err(HelixError::new(
                        format!("`...` record update needs a record, got {}", crate::value::with_article(base.type_name())),
                        line,
                        col,
                    )
                    .hint("the spread base must be a record, e.g. `{ ...resp, status: 500 }`."));
                };
                // Clone the base, then set (override) or append each update field, in order.
                let mut out: Vec<(crate::symbol::Symbol, Value)> = (*base_fields).clone();
                for (name, val) in names.iter().copied().zip(vals) {
                    match out.iter_mut().find(|(s, _)| *s == name) {
                        Some(slot) => slot.1 = val,
                        None => out.push((name, val)),
                    }
                }
                stack.push(Value::Record(std::rc::Rc::new(out)));
            }
            Op::GetField(name) => {
                let recv = stack.pop().unwrap();
                stack.push(crate::interp::eval_field(&recv, *name, line, col)?);
            }
            Op::Slice(mask) => {
                // Bounds were pushed after the receiver in start/stop/step order;
                // pop the raw values back off in reverse, then resolve them in
                // forward order so error reporting matches the tree-walker exactly.
                let step_v = if mask & 4 != 0 { stack.pop() } else { None };
                let stop_v = if mask & 2 != 0 { stack.pop() } else { None };
                let start_v = if mask & 1 != 0 { stack.pop() } else { None };
                let recv = stack.pop().unwrap();
                let start = match &start_v {
                    Some(v) => crate::interp::slice_bound(v, line, col)?,
                    None => None,
                };
                let stop = match &stop_v {
                    Some(v) => crate::interp::slice_bound(v, line, col)?,
                    None => None,
                };
                let step = match &step_v {
                    Some(v) => crate::interp::slice_bound(v, line, col)?.unwrap_or(1),
                    None => 1,
                };
                if step == 0 {
                    return Err(HelixError::new("slice step cannot be zero", line, col));
                }
                stack.push(crate::interp::eval_slice(&recv, start, stop, step, line, col)?);
            }
            Op::Destructure(slots) => {
                let v = stack.pop().unwrap();
                let parts = crate::interp::destructure_parts(&v, slots.len(), line, col)?;
                for (slot, val) in slots.iter().zip(parts) {
                    globals[*slot as usize] = val;
                }
            }
            Op::DestructureBind(slots) => {
                // A comprehension multi-binder pattern: split the current element
                // into the named param locals (same helper the tree-walker uses).
                let v = stack.pop().unwrap();
                let parts = crate::interp::pattern_parts(&v, slots.len(), line, col)?;
                let base = frames[fi].base;
                for (slot, val) in slots.iter().zip(parts) {
                    locals[base + *slot as usize] = val;
                }
            }
            Op::Method(d) => {
                let (name, nargs) = (&d.name, &d.nargs);
                let split = stack.len() - *nargs as usize;
                let args: Vec<Value> = stack.split_off(split);
                let recv = stack.pop().unwrap();
                // `is_missing` is universal; DataFrame/GroupBy receivers bypass the
                // universal handler in `call_method`, so intercept it here (a
                // frame/group is never `missing` → `false`), matching the tree-walker.
                let result = if name.as_str() == "is_missing"
                    && matches!(recv, Value::DataFrame(_) | Value::GroupBy(_))
                {
                    crate::interp::df_is_missing(args.is_empty(), line, col)
                } else {
                    match &recv {
                        // Dispatch by receiver type, exactly as the tree-walker does.
                        Value::DataFrame(lf) => {
                            crate::interp::df_value_method(lf, name, args, line, col)
                        }
                        Value::GroupBy(_) => Err(HelixError::new(
                            format!("a GroupBy has no value-method `{}`", name),
                            line,
                            col,
                        )
                        .hint("aggregate with a column, e.g. `g.mean(col)`.")),
                        _ => crate::interp::call_method(&recv, name, args, line, col),
                    }
                }?;
                stack.push(result);
            }
            Op::DfColumnVerb(d) => {
                let (name, args, lbind) = (&d.name, &d.args, &d.locals);
                // A DataFrame column-verb (the type checker proved the receiver is a
                // DataFrame). `resolve_var` resolves a bare predicate name that
                // isn't a column to a local (via the captured slot map) or a global
                // — the same env the tree-walker uses.
                let recv = stack.pop().unwrap();
                let lf = match &recv {
                    Value::DataFrame(lf) => lf.clone(),
                    other => {
                        return Err(HelixError::new(
                            format!("expected a DataFrame, got {}", other.type_name()),
                            line,
                            col,
                        ))
                    }
                };
                let base = frames[fi].base;
                let resolve = |nm: &str| -> Option<Value> {
                    for (lname, slot) in lbind.iter().rev() {
                        if lname == nm {
                            return Some(locals[base + *slot as usize].clone());
                        }
                    }
                    program
                        .global_names
                        .iter()
                        .position(|g| g == nm)
                        .map(|i| globals[i].clone())
                };
                let result =
                    crate::interp::df_column_verb(&lf, name.as_str(), args.as_slice(), &resolve, line, col)?;
                stack.push(result);
            }
            Op::DfJoin { spec } => {
                // Stack order: left receiver pushed first, then the right operand.
                let right = stack.pop().unwrap();
                let left = stack.pop().unwrap();
                let result = match &left {
                    // DataFrame join: the right operand is another frame; keys are by-name.
                    Value::DataFrame(lf) => {
                        let rf = match &right {
                            Value::DataFrame(rf) => rf.clone(),
                            other => {
                                return Err(HelixError::new(
                                    format!(
                                        "`join` needs a DataFrame to join with, found {}",
                                        other.type_name()
                                    ),
                                    line,
                                    col,
                                ))
                            }
                        };
                        let (keys, how) = crate::interp::parse_join_spec(spec.as_slice(), line, col)?;
                        Value::dataframe(lf.join(&rf, &keys, &how, line, col)?)
                    }
                    // The receiver's static type was `Unknown` and turned out NOT to be a
                    // DataFrame — this is the value `xs.join(sep)` (an array of strings).
                    // Dispatch by runtime type exactly as the tree-walker does. `spec`
                    // (extra by-name key args) can't ride the value form: the walker
                    // evaluates every argument and hits `join`'s 1-arg arity check, so
                    // raise that same error instead of silently dropping the extras.
                    _ => {
                        if !spec.is_empty() {
                            return Err(HelixError::new(
                                format!("`join` takes 1 argument, got {}", 1 + spec.len()),
                                line,
                                col,
                            ));
                        }
                        crate::interp::call_method(&left, "join", vec![right], line, col)?
                    }
                };
                stack.push(result);
            }
            Op::GroupByAgg(d) => {
                let (name, args) = (&d.name, &d.args);
                let recv = stack.pop().unwrap();
                let (handle, keys) = match &recv {
                    Value::GroupBy(g) => (g.handle.clone(), g.keys.clone()),
                    other => {
                        return Err(HelixError::new(
                            format!("expected a GroupBy, got {}", other.type_name()),
                            line,
                            col,
                        ))
                    }
                };
                let result = crate::interp::groupby_agg(
                    &handle,
                    &keys,
                    name.as_str(),
                    args.as_slice(),
                    line,
                    col,
                )?;
                stack.push(result);
            }
            Op::CompInit(kind, missing_target) => {
                let v = stack.pop().unwrap();
                match v {
                    Value::Array(a) => {
                        iters.push(CompIter {
                            source: CompSource::Array { arr: a, idx: 0 },
                            cur_val: Value::Unit,
                            builder: crate::value::ColumnBuilder::default(),
                        });
                    }
                    // `missing.map(...)` etc. propagate (ADR-0001).
                    Value::Missing => frames[fi].ip = *missing_target as usize,
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "type {} has no method `{}`",
                                other.type_name(),
                                kind.method_name()
                            ),
                            line,
                            col,
                        )
                        .hint("`map`, `filter`, `where`, and `reduce` work on arrays."))
                    }
                }
            }
            Op::TryJitReduce { loop_idx, acc_slot, after } => {
                // Stack top: [.., start, end]; `acc_slot` already holds `init`.
                // Take the native loop only when start/end/init are all `Int` and
                // the range is within the materialization cap; anything else falls
                // through to the identical `CompInitRange` bytecode loop below — so
                // float accumulators, over-cap ranges (which must error exactly as
                // `CompInitRange` does), and no-JIT builds all take the same path
                // the tree-walker oracle matches.
                // A scalar accumulator has 1 body; a tuple accumulator N (the slot then
                // holds a `Tuple` of N `Int`s). A scalar body may carry `captures` — their
                // values sit above `[start, end]`; split them off (taken or not) so the
                // fall-through `CompInitRange` sees only `[start, end]`.
                let n_acc = program.reduce_loops[*loop_idx as usize].bodies.len();
                let n_caps = program.reduce_loops[*loop_idx as usize].captures.len();
                let cap_vals =
                    if n_caps > 0 { stack.split_off(stack.len() - n_caps) } else { Vec::new() };
                let base = frames[fi].base;
                let len = stack.len();
                let slot = base + *acc_slot as usize;
                let bounds = match (jit, &stack[len - 2], &stack[len - 1]) {
                    (Some(j), Value::Int(s), Value::Int(e))
                        if ((*e as i128) - (*s as i128)) <= 100_000_000 =>
                    {
                        j.reduce_loop(*loop_idx as usize).map(|ptr| (ptr, *s, *e))
                    }
                    _ => None,
                };
                let took_native = if let Some((ptr, s, e)) = bounds {
                    if n_acc == 1 && program.reduce_loops[*loop_idx as usize].float {
                        // A scalar f64 fold over the i64 counter: a `Float` init confirms the
                        // f64 ABI; anything else falls back to the VM loop. With captures it is
                        // the float dot-product — every cap is an `ArrayF64` base pointer, taken
                        // only after the same bounds pre-check as the i64 path (out-of-range or
                        // a non-`Floats` array falls back to the exact-erroring bytecode loop).
                        if let Value::Float(init) = locals[slot] {
                            if n_caps == 0 {
                                // A body containing `/` may divide by zero, where native `fdiv`
                                // yields inf/nan but the interpreter RAISES. Such a kernel carries
                                // a poison out-param the codegen sets on ANY zero divisor (every
                                // iteration, every division — regardless of whether a later op or
                                // iteration would rescue the inf); a set flag means fall back to
                                // the exact-erroring bytecode loop, while an unset flag guarantees
                                // no `/0` occurred so `r` is bit-exact to the interpreter. A
                                // non-dividing reduce uses the plain, poison-free kernel.
                                // The SAME field `define_reduce_loop` built the signature
                                // from — not a second derivation that has to agree with it.
                                if program.reduce_loops[*loop_idx as usize].raises {
                                    let mut poison: i8 = 0;
                                    let r = unsafe {
                                        crate::jit::call_reduce_f64_div(ptr, s, e, init, &mut poison)
                                    };
                                    if poison != 0 {
                                        false
                                    } else {
                                        locals[slot] = Value::Float(r);
                                        true
                                    }
                                } else {
                                    let r = unsafe { crate::jit::call_reduce_f64(ptr, s, e, init) };
                                    locals[slot] = Value::Float(r);
                                    true
                                }
                            } else {
                                use crate::bytecode::{CaptureKind, IndexBound};
                                use crate::value::ArrayData;
                                let rl = &program.reduce_loops[*loop_idx as usize];
                                let caps_meta = &rl.captures;
                                let mut caps: Vec<i64> = Vec::with_capacity(n_caps);
                                let mut lens: Vec<i64> = Vec::with_capacity(n_caps); // array len, 0 for scalars
                                let mut _keepalive: Vec<Value> = Vec::new();
                                let mut ok = true;
                                for (cap, val) in caps_meta.iter().zip(cap_vals.iter()) {
                                    match (cap.kind, val) {
                                        (CaptureKind::ArrayF64, Value::Array(a)) => {
                                            if let ArrayData::Floats(v) = &**a {
                                                caps.push(v.as_ptr() as i64);
                                                lens.push(v.len() as i64);
                                                _keepalive.push(val.clone());
                                            } else {
                                                ok = false;
                                                break;
                                            }
                                        }
                                        // A loop-invariant `i64` an affine index reads (or its
                                        // pre-computed base/coef). A non-`Int` value means the
                                        // body's index arithmetic wouldn't be i64 at all → fall
                                        // back to the bytecode loop rather than guess.
                                        (CaptureKind::Scalar, Value::Int(i)) => {
                                            caps.push(*i);
                                            lens.push(0);
                                        }
                                        // A VALUE scalar (the coefficient `c` in `s + c*a[i]`).
                                        // This kernel is monomorphically `f64`, so the slot rides
                                        // as `f64` BITS: an `Int` promoted (matching the
                                        // interpreter's `Int * Float` promotion, which
                                        // `infer_f64_indexed` only admitted where a genuine float
                                        // forces it) or a `Float` passed through.
                                        (CaptureKind::ScalarValue, Value::Int(i)) => {
                                            caps.push((*i as f64).to_bits() as i64);
                                            lens.push(0);
                                        }
                                        (CaptureKind::ScalarValue, Value::Float(f)) => {
                                            caps.push(f.to_bits() as i64);
                                            lens.push(0);
                                        }
                                        _ => {
                                            ok = false;
                                            break;
                                        }
                                    }
                                }
                                // Prove every `arr[…]` access is in bounds BEFORE the kernel's
                                // unchecked loads — the f64 twin of the i64 path's obligations.
                                if ok {
                                    for bnd in &rl.index_bounds {
                                        match *bnd {
                                            IndexBound::Counter { array } => {
                                                if s < 0 || e > lens[array as usize] {
                                                    ok = false;
                                                    break;
                                                }
                                            }
                                            IndexBound::Scalar { array, scalar } => {
                                                let iv = caps[scalar as usize];
                                                if iv < 0 || iv >= lens[array as usize] {
                                                    ok = false;
                                                    break;
                                                }
                                            }
                                            // `arr[base + coef*k]` for k in [s,e): the index is
                                            // monotone in k, so the two ENDPOINTS bracket every
                                            // access. Evaluated in i128 so the check cannot itself
                                            // overflow; an empty range accesses nothing.
                                            IndexBound::Affine { array, base, coef } => {
                                                if s >= e {
                                                    continue;
                                                }
                                                let bv = caps[base as usize] as i128;
                                                let cv = caps[coef as usize] as i128;
                                                let first = bv + cv * (s as i128);
                                                let last = bv + cv * ((e - 1) as i128);
                                                let (lo, hi) =
                                                    if first <= last { (first, last) } else { (last, first) };
                                                if lo < 0 || hi >= lens[array as usize] as i128 {
                                                    ok = false;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                if ok {
                                    let r = unsafe {
                                        crate::jit::call_reduce_f64_caps(ptr, s, e, init, caps.as_ptr())
                                    };
                                    locals[slot] = Value::Float(r);
                                    true
                                } else {
                                    false
                                }
                            }
                        } else {
                            false
                        }
                    } else if n_acc == 1 {
                        if let Value::Int(init) = locals[slot] {
                            if n_caps == 0 {
                                let r = unsafe { crate::jit::call_reduce(ptr, s, e, init) };
                                locals[slot] = Value::Int(r);
                                true
                            } else {
                                // Marshal captures by kind — the ordered `captures` list drives
                                // both this fill and the codegen's per-slot read, so neither
                                // ever infers a kind independently. A `Scalar` cap rides as its
                                // `i64` value; an `ArrayI64` cap rides as its packed base
                                // pointer, but ONLY after a bounds pre-check proves the whole
                                // counter range `[s, e)` is within `[0, len)` — the kernel does
                                // unchecked loads, so an out-of-range access, a negative start
                                // (which the interpreter would Python-wrap), or a non-`Int` /
                                // unpacked array must all fall back to the bytecode loop, which
                                // re-evaluates `arr[j]` via `Op::Index` and raises the exact OOB
                                // error. `_keepalive` holds the array `Rc`s alive across the FFI
                                // call (the base pointers alias into their buffers).
                                use crate::bytecode::{CaptureKind, IndexBound};
                                use crate::value::ArrayData;
                                let rl = &program.reduce_loops[*loop_idx as usize];
                                let mut caps: Vec<i64> = Vec::with_capacity(n_caps);
                                let mut lens: Vec<i64> = Vec::with_capacity(n_caps); // array len, 0 for scalars
                                let mut _keepalive: Vec<Value> = Vec::new();
                                let mut ok = true;
                                for (cap, val) in rl.captures.iter().zip(cap_vals.iter()) {
                                    match (cap.kind, val) {
                                        (CaptureKind::Scalar, Value::Int(i)) => {
                                            caps.push(*i);
                                            lens.push(0);
                                        }
                                        (CaptureKind::ArrayI64, Value::Array(a)) => {
                                            if let ArrayData::Ints(v) = &**a {
                                                caps.push(v.as_ptr() as i64);
                                                lens.push(v.len() as i64);
                                                _keepalive.push(val.clone());
                                            } else {
                                                ok = false;
                                                break;
                                            }
                                        }
                                        _ => {
                                            ok = false;
                                            break;
                                        }
                                    }
                                }
                                // Verify every array-index access is in bounds BEFORE the kernel's
                                // unchecked loads: a counter-indexed array needs the whole range
                                // `[s,e) ⊆ [0,len)`; a scalar-indexed array needs `0 <= i < len`.
                                // A negative counter/scalar (which the interpreter Python-wraps) or
                                // any over-range access → fall through to the exact-erroring loop.
                                if ok {
                                    for bnd in &rl.index_bounds {
                                        match *bnd {
                                            IndexBound::Counter { array } => {
                                                if s < 0 || e > lens[array as usize] {
                                                    ok = false;
                                                    break;
                                                }
                                            }
                                            IndexBound::Scalar { array, scalar } => {
                                                let iv = caps[scalar as usize];
                                                if iv < 0 || iv >= lens[array as usize] {
                                                    ok = false;
                                                    break;
                                                }
                                            }
                                            // The i64 collector (`value_eligible_cap_indexed`) only
                                            // admits `arr[counter]` / `arr[scalar]`, so it never
                                            // emits an affine obligation. Decline rather than run
                                            // unchecked loads if that ever changes.
                                            IndexBound::Affine { .. } => {
                                                ok = false;
                                                break;
                                            }
                                        }
                                    }
                                }
                                if ok {
                                    let r = unsafe {
                                        crate::jit::call_reduce_caps(ptr, s, e, init, caps.as_ptr())
                                    };
                                    locals[slot] = Value::Int(r);
                                    true
                                } else {
                                    false
                                }
                            }
                        } else {
                            false
                        }
                    } else {
                        // Marshal the N-slot tuple/record into a buffer, fold natively, rebuild
                        // the same-shaped accumulator. An f64 accumulator packs each slot's
                        // bit pattern (the kernel reads/writes them as f64 — same `*mut i64`
                        // runner); the i64 path takes Int slots. A wrong-typed slot falls back.
                        // (`tmpl` clone is an Rc bump, freeing the locals borrow.)
                        let tmpl = locals[slot].clone();
                        let is_float = program.reduce_loops[*loop_idx as usize].float;
                        let slots =
                            if is_float { acc_to_slots_f64(&tmpl, n_acc) } else { acc_to_slots(&tmpl, n_acc) };
                        match slots {
                            Some(mut buf) => {
                                unsafe { crate::jit::call_tuple_reduce(ptr, s, e, buf.as_mut_ptr()) };
                                locals[slot] =
                                    if is_float { rebuild_acc_f64(&tmpl, buf) } else { rebuild_acc(&tmpl, buf) };
                                true
                            }
                            None => false,
                        }
                    }
                } else {
                    false
                };
                if took_native {
                    stack.pop(); // end
                    stack.pop(); // start
                    frames[fi].ip = *after as usize;
                }
                // else: fall through (ip already advanced) to CompInitRange.
            }
            Op::TryJitMap { kernel_idx, after } => {
                // The captured values (if any) sit on top of the receiver array. Pop them
                // off (taken or not) so the bytecode fall-through sees only the array, then
                // dispatch by element type: an `Int` array → the `i64` kernel with `i64`
                // captures (a non-`Int` capture falls through); a `Float` array → the `f64`
                // kernel with `f64` captures (Int/Float coerce). Anything else falls
                // through to the identical `CompInit` bytecode loop (the oracle path).
                use crate::value::ArrayData;
                let kidx = *kernel_idx as usize;
                let n_caps = program.map_kernels[kidx].captures.len();
                let split = stack.len() - n_caps;
                let cap_vals = stack.split_off(split);
                // Read the receiver's range shape BEFORE materializing it. An indexed body's
                // `Counter` bound is discharged by the range's endpoints, and densifying erases
                // exactly that: afterwards a range is indistinguishable from any other `Ints`
                // buffer, and the elements-are-the-counter fact the proof rests on is gone. See
                // `ArrayKernel::index_bounds`.
                let src_range: Option<(i64, i64, usize)> = match stack.last() {
                    Some(Value::Array(a)) => match &**a {
                        ArrayData::Range { start, step, len } => Some((*start, *step, *len)),
                        _ => None,
                    },
                    _ => None,
                };
                // FAST PATH: a lazy range needs no buffer at all — the kernel's inputs are
                // `start + step*j`, generated a chunk at a time. Tried BEFORE
                // `densify_range_top`, since that call is precisely what allocates the second
                // full-size buffer this avoids.
                let range_out = src_range
                    .and_then(|r| try_map_range(jit, &program.map_kernels[kidx], kidx, &cap_vals, r));
                if let Some(v) = range_out {
                    stack.pop(); // the lazy range receiver
                    stack.push(v);
                    frames[fi].ip = *after as usize;
                } else {
                    // A lazy `range` source has no buffer for the native map kernel; materialize it so
                    // the JIT engages (as before ranges were lazy). The receiver is now the stack top.
                    densify_range_top(&mut stack);
                    enum Pick {
                        I64(*const u8, Vec<i64>),
                        F64(*const u8, Vec<f64>),
                        Mixed(*const u8, Vec<i64>),
                        No,
                    }
                    let pick = match (jit, stack.last()) {
                        (Some(j), Some(Value::Array(a))) => match &**a {
                            ArrayData::Ints(_) => {
                                // The SAME stored kernel may carry TWO specializations: an i64
                                // build (array caps marshaled from `Ints`) and a mixed build
                                // (array caps from `Floats`, f64 result). Try i64 first; for an
                                // indexed body the marshal's representation check routes to
                                // whichever matches the runtime captures, and a mismatch with
                                // BOTH declines to the bytecode loop.
                                let k = &program.map_kernels[kidx];
                                let i64_pick = j.map_kernel(kidx).and_then(|p| {
                                    let caps: Option<Vec<i64>> = if k.index_bounds.is_empty() {
                                        // plain i64 kernel: every capture must be an `Int`
                                        int_scalar_caps(&cap_vals)
                                    } else {
                                        // A body reading a captured array: every `a[…]` becomes an
                                        // UNCHECKED native load, so prove them all in bounds first
                                        // or decline.
                                        map_index_caps(k, &cap_vals, src_range, false)
                                    };
                                    caps.map(|c| Pick::I64(p, c))
                                });
                                match (i64_pick, j.map_kernel_mixed(kidx)) {
                                    (Some(pk), _) => pk,
                                    // unindexed mixed (`range.map(j => j*0.001)`), with or
                                    // without `Int` scalar captures (`range.map(j => c*j*0.5)`).
                                    (None, Some(p)) if k.index_bounds.is_empty() => {
                                        match int_scalar_caps(&cap_vals) {
                                            Some(c) => Pick::Mixed(p, c),
                                            None => Pick::No,
                                        }
                                    }
                                    // indexed mixed: f64-array caps, the same bounds discharge.
                                    (None, Some(p)) if !k.index_bounds.is_empty() => {
                                        match map_index_caps(k, &cap_vals, src_range, true) {
                                            Some(c) => Pick::Mixed(p, c),
                                            None => Pick::No,
                                        }
                                    }
                                    _ if !k.index_bounds.is_empty() => Pick::No,
                                    // Unindexed, and the Int-proven marshal declined (some
                                    // capture is a runtime `Float`). Try the VALUE-SCALAR
                                    // variant — same ABI as the mixed kernel, caps as f64
                                    // bits — then the Int-ROOTED one, which shares the i64
                                    // kernel's ABI and so rides `Pick::I64`, dead-buffer
                                    // reuse included.
                                    _ => {
                                        let vs = j
                                            .map_kernel_mixed_value(kidx)
                                            .zip(value_scalar_caps(&cap_vals));
                                        let mi = j
                                            .map_kernel_mixed_int(kidx)
                                            .zip(int_scalar_caps(&cap_vals));
                                        match (vs, mi) {
                                            (Some((p, c)), _) => Pick::Mixed(p, c),
                                            (None, Some((p, c))) => Pick::I64(p, c),
                                            (None, None) => Pick::No,
                                        }
                                    }
                                }
                            }
                            ArrayData::Floats(_) => {
                                let caps: Option<Vec<f64>> = cap_vals.iter().map(|v| v.as_f64()).collect();
                                match (caps, j.map_kernel_f64(kidx)) {
                                    (Some(c), Some(p)) => Pick::F64(p, c),
                                    _ => Pick::No,
                                }
                            }
                            _ => Pick::No,
                        },
                        _ => Pick::No,
                    };
                    match pick {
                        // `Rc::get_mut` succeeds only when this handle is the ONLY one, i.e. the
                        // receiver is a dead temporary (`xs.map(f).map(g)`'s intermediate). Then
                        // the kernel writes back into it instead of allocating and zeroing a
                        // second full-size buffer — see `run_map_kernel_inplace` for why
                        // aliasing src and dst is sound for a map. A named source keeps a second
                        // `Rc` alive, so it takes the allocating path and is never mutated under
                        // the program's feet. `index_bounds` must be empty: a body reading a
                        // captured array is the one shape where an iteration could touch an
                        // index another iteration writes. (Such bodies are already range-only,
                        // so this cannot trigger today — but the guard is local, and the
                        // alternative is a safety argument that lives in another function.)
                        Pick::I64(ptr, caps) => {
                            let mut arr = stack.pop().unwrap();
                            let unindexed = program.map_kernels[kidx].index_bounds.is_empty();
                            // A RAISING kernel (rounder body, poison signature) must never
                            // take the in-place branch: on poison the fall-back re-runs the
                            // body over the SOURCE, which in-place reuse would have already
                            // overwritten. It calls the poison wrapper instead, whose `None`
                            // (some element left i64 range) falls through to the bytecode
                            // loop for the exact interpreter error.
                            let raises = program.map_kernels[kidx].raises;
                            let ran = if unindexed
                                && !raises
                                && let Value::Array(a) = &mut arr
                                && let Some(ArrayData::Ints(v)) = std::rc::Rc::get_mut(a)
                            {
                                unsafe { crate::jit::run_map_kernel_inplace(ptr, v, &caps) };
                                true
                            } else if let Value::Array(a) = &arr
                                && let ArrayData::Ints(v) = &**a
                            {
                                if raises {
                                    match unsafe {
                                        crate::jit::run_map_kernel_int_poison(ptr, v, &caps)
                                    } {
                                        Some(out) => {
                                            arr = Value::int_array(out);
                                            true
                                        }
                                        None => false,
                                    }
                                } else {
                                    let out =
                                        unsafe { crate::jit::run_map_kernel(ptr, v, &caps) };
                                    arr = Value::int_array(out);
                                    true
                                }
                            } else {
                                false
                            };
                            stack.push(arr);
                            if ran {
                                frames[fi].ip = *after as usize;
                            }
                        }
                        Pick::F64(ptr, caps) => {
                            let mut arr = stack.pop().unwrap();
                            let unindexed = program.map_kernels[kidx].index_bounds.is_empty();
                            let ran = if unindexed
                                && let Value::Array(a) = &mut arr
                                && let Some(ArrayData::Floats(v)) = std::rc::Rc::get_mut(a)
                            {
                                unsafe { crate::jit::run_map_kernel_f64_inplace(ptr, v, &caps) };
                                true
                            } else if let Value::Array(a) = &arr
                                && let ArrayData::Floats(v) = &**a
                            {
                                let out = unsafe { crate::jit::run_map_kernel_f64(ptr, v, &caps) };
                                arr = Value::float_array(out);
                                true
                            } else {
                                false
                            };
                            stack.push(arr);
                            if ran {
                                frames[fi].ip = *after as usize;
                            }
                        }
                        Pick::Mixed(ptr, caps) => {
                            let arr = stack.pop().unwrap();
                            if let Value::Array(a) = &arr
                                && let ArrayData::Ints(v) = &**a
                            {
                                // `cap_vals` (holding the Rcs behind any base pointers in `caps`)
                                // is still in scope — dropped only when this arm ends.
                                // A RAISING body takes the poison wrapper; its `None` (a
                                // rounder left i64 range) falls through to the bytecode loop.
                                if program.map_kernels[kidx].raises {
                                    match unsafe {
                                        crate::jit::run_map_kernel_mixed_poison(ptr, v, &caps)
                                    } {
                                        Some(out) => {
                                            stack.push(Value::float_array(out));
                                            frames[fi].ip = *after as usize;
                                        }
                                        None => stack.push(arr),
                                    }
                                } else {
                                    let out =
                                        unsafe { crate::jit::run_map_kernel_mixed(ptr, v, &caps) };
                                    stack.push(Value::float_array(out));
                                    frames[fi].ip = *after as usize;
                                }
                            } else {
                                stack.push(arr);
                            }
                        }
                        Pick::No => {} // fall through to the bytecode loop
                    }
                }
            }
            Op::TryJitFilter { kernel_idx, after } => {
                // Same fast path as `TryJitMap`: a lazy range's elements are `start + step*j`, so
                // the kernel can be fed generated values instead of a materialized buffer that
                // would sit live beside the output. Tried BEFORE `densify_range_top`, which is
                // what allocates it.
                // A capturing predicate (`it % k == 0`) pushed its captures above the receiver;
                // pop them whether or not the kernel is taken, so the bytecode fall-through
                // sees only the array. Each must be an `Int` at run time — the same proof the
                // map path uses, and what lets the predicate read them as `i64`.
                let fkidx = *kernel_idx as usize;
                let n_fcaps = program.filter_kernels[fkidx].captures.len();
                let fsplit = stack.len() - n_fcaps;
                let fcap_vals = stack.split_off(fsplit);
                let fcaps = int_scalar_caps(&fcap_vals);
                let filt_range: Option<(i64, i64, usize)> = match stack.last() {
                    Some(Value::Array(a)) => match &**a {
                        crate::value::ArrayData::Range { start, step, len } => {
                            Some((*start, *step, *len))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                let range_taken = if let (Some(j), Some((rs, rstep, rlen))) = (jit, filt_range)
                    && let Some(p) = j.filter_kernel(fkidx)
                    && let Some(c) = fcaps.as_deref()
                {
                    let out =
                        unsafe { crate::jit::run_filter_kernel_range(p, rs, rstep, rlen, c) };
                    stack.pop(); // the lazy range receiver
                    stack.push(Value::int_array(out));
                    frames[fi].ip = *after as usize;
                    true
                } else {
                    false
                };
                if !range_taken {
                    densify_range_top(&mut stack); // materialize so the native filter engages
                    let ptr = match (jit, stack.last(), fcaps.as_deref()) {
                        (Some(j), Some(Value::Array(a)), Some(_))
                            if matches!(&**a, crate::value::ArrayData::Ints(_)) =>
                        {
                            j.filter_kernel(fkidx)
                        }
                        _ => None,
                    };
                    if let Some(ptr) = ptr {
                        let c = fcaps.as_deref().unwrap_or(&[]);
                        let arr = stack.pop().unwrap();
                        if let Value::Array(a) = &arr
                            && let crate::value::ArrayData::Ints(v) = &**a
                        {
                            let out = unsafe { crate::jit::run_filter_kernel(ptr, v, c) };
                            stack.push(Value::int_array(out));
                            frames[fi].ip = *after as usize;
                        } else {
                            stack.push(arr);
                        }
                    }
                    // A `Floats` source dispatches the f64 specialization. Captures
                    // marshal through `as_f64`: an `Int` capture is promoted with exactly
                    // the conversion the interpreter performs where the (F64Proof-checked)
                    // predicate uses it, a `Float` passes through, anything else declines
                    // to the bytecode loop. `None` from the runner means the kernel
                    // POISONED — an ordering comparison met a NaN — and the bytecode
                    // fall-through raises the interpreter's exact error at the exact
                    // element.
                    if let (Some(j), Some(Value::Array(a))) = (jit, stack.last())
                        && matches!(&**a, crate::value::ArrayData::Floats(_))
                        && let Some(fptr) = j.filter_kernel_f64(fkidx)
                        && let Some(fc) =
                            fcap_vals.iter().map(|v| v.as_f64()).collect::<Option<Vec<f64>>>()
                    {
                        // Cannot fail: the `stack.last()` pattern two lines up proved the
                        // top exists — the same argument as the Ints arm's pop above.
                        let arr = stack.pop().unwrap();
                        if let Value::Array(a) = &arr
                            && let crate::value::ArrayData::Floats(v) = &**a
                        {
                            match unsafe { crate::jit::run_filter_kernel_f64(fptr, v, &fc) } {
                                Some(out) => {
                                    stack.push(Value::float_array(out));
                                    frames[fi].ip = *after as usize;
                                }
                                None => stack.push(arr),
                            }
                        } else {
                            stack.push(arr);
                        }
                    }
                }
            }
            Op::TryJitScan { loop_idx, after } => {
                // Operands `[start, end, init]` plus any capture values sit on the stack, and
                // are consumed WHETHER OR NOT the native path is taken (`TryJitFused`'s
                // protocol — the fall-through is a fresh recompile of the whole scan, which
                // re-evaluates everything itself). Native requires: a kernel compiled, all of
                // start/end/init `Int`, every capture an `Int` (the same runtime proof every
                // capturing kernel uses), and the length within the same cap as the reduce
                // guard — an over-cap scan must take the bytecode path so it errors (or
                // builds) exactly as `CompInit` does.
                let rl = &program.scan_loops[*loop_idx as usize];
                let n_caps = rl.captures.len();
                let cap_vals =
                    if n_caps > 0 { stack.split_off(stack.len() - n_caps) } else { Vec::new() };
                // These three cannot fail: `TryJitScan` is emitted at exactly one site
                // (`compile_scan`'s guard), which pushes `[start, end, init]` immediately
                // before it — and the capture split above only removed what the same site
                // pushed above them. Counted in the ADR-0024 budget on that argument.
                let init_v = stack.pop().unwrap();
                let end_v = stack.pop().unwrap();
                let start_v = stack.pop().unwrap();
                let native: Option<Vec<i64>> = (|| {
                    let ptr = jit?.scan_loop(*loop_idx as usize)?;
                    let (Value::Int(s), Value::Int(e), Value::Int(init)) =
                        (&start_v, &end_v, &init_v)
                    else {
                        return None;
                    };
                    if (*e as i128) - (*s as i128) > 100_000_000 {
                        return None;
                    }
                    let caps = int_scalar_caps(&cap_vals)?;
                    Some(unsafe { crate::jit::run_scan_kernel_range(ptr, *s, *e, *init, &caps) })
                })();
                if let Some(out) = native {
                    stack.push(Value::int_array(out));
                    frames[fi].ip = *after as usize;
                }
            }
            Op::TryJitFused { kernel_idx, after } => {
                use crate::bytecode::FusionSink;
                // The pipeline's source operands sit on the stack (array, or [start,end];
                // plus init for a Reduce sink). Run the single native loop when they are
                // all `Int` (range within the 100M cap) and a kernel compiled; otherwise
                // consume the same operands and fall through to the per-stage chain.
                let kern = &program.fused_kernels[*kernel_idx as usize];
                let n = kern.n_operands();
                let len = stack.len();
                // Read a tuple-accumulator init (`want` `Int` slots) into a buffer.
                let result: Option<Value> = jit
                    .and_then(|j| j.fused_kernel(*kernel_idx as usize))
                    .and_then(|ptr| {
                        let ops = &stack[len - n..];
                        if kern.source_is_range {
                            // ops = [start, end] (+ init for Reduce). Cap-check the span.
                            let (Value::Int(s), Value::Int(e)) = (&ops[0], &ops[1]) else {
                                return None;
                            };
                            if (*e as i128 - *s as i128) > 100_000_000 {
                                return None;
                            }
                            match &kern.sink {
                                FusionSink::Reduce { bodies, float: false, .. } if bodies.len() == 1 => {
                                    match &ops[2] {
                                        Value::Int(init) => Some(Value::Int(unsafe {
                                            crate::jit::call_reduce(ptr, *s, *e, *init)
                                        })),
                                        _ => None,
                                    }
                                }
                                // tuple/record accumulator: ops[2] is its N-Int value.
                                FusionSink::Reduce { bodies, .. } => {
                                    acc_to_slots(&ops[2], bodies.len()).map(|mut buf| {
                                        unsafe {
                                            crate::jit::call_tuple_reduce(ptr, *s, *e, buf.as_mut_ptr())
                                        };
                                        rebuild_acc(&ops[2], buf)
                                    })
                                }
                                // count: the kernel ignores the third arg.
                                FusionSink::Count => Some(Value::Int(unsafe {
                                    crate::jit::call_reduce(ptr, *s, *e, 0)
                                })),
                                FusionSink::Collect => None,
                            }
                        } else if let Value::Array(a) = &ops[0]
                            && let crate::value::ArrayData::Ints(v) = &**a
                        {
                            match &kern.sink {
                                FusionSink::Collect => Some(Value::int_array(unsafe {
                                    crate::jit::run_fused_collect(ptr, v)
                                })),
                                FusionSink::Count => Some(Value::Int(unsafe {
                                    crate::jit::run_fused_count(ptr, v)
                                })),
                                FusionSink::Reduce { bodies, float: false, .. } if bodies.len() == 1 => {
                                    match &ops[1] {
                                        Value::Int(init) => Some(Value::Int(unsafe {
                                            crate::jit::run_fused_reduce(ptr, v, *init)
                                        })),
                                        _ => None,
                                    }
                                }
                                // tuple/record accumulator: ops[1] is its N-Int value.
                                FusionSink::Reduce { bodies, .. } => {
                                    acc_to_slots(&ops[1], bodies.len()).map(|mut buf| {
                                        unsafe {
                                            crate::jit::run_fused_tuple_reduce(ptr, v, buf.as_mut_ptr())
                                        };
                                        rebuild_acc(&ops[1], buf)
                                    })
                                }
                            }
                        } else if let Value::Array(a) = &ops[0]
                            && let crate::value::ArrayData::Floats(v) = &**a
                        {
                            // An `f64` reduce over a `Float` array: scalar (1 body, `Float`
                            // init → `run_fused_reduce_f64`) or tuple (N bodies, all-`Float`
                            // accumulator marshalled as bit patterns → `run_fused_tuple_reduce`,
                            // the same `*mut i64` runner the kernel reads/writes as f64).
                            match &kern.sink {
                                FusionSink::Reduce { bodies, float: true, .. } if bodies.len() == 1 => {
                                    match &ops[1] {
                                        Value::Float(init) => Some(Value::Float(unsafe {
                                            crate::jit::run_fused_reduce_f64(ptr, v, *init)
                                        })),
                                        _ => None,
                                    }
                                }
                                FusionSink::Reduce { bodies, float: true, .. } => {
                                    acc_to_slots_f64(&ops[1], bodies.len()).map(|mut buf| {
                                        unsafe {
                                            crate::jit::run_fused_tuple_reduce_f64(ptr, v, buf.as_mut_ptr())
                                        };
                                        rebuild_acc_f64(&ops[1], buf)
                                    })
                                }
                                _ => None,
                            }
                        } else {
                            None
                        }
                    });
                stack.truncate(len - n);
                if let Some(v) = result {
                    stack.push(v);
                    frames[fi].ip = *after as usize;
                }
            }
            Op::TryJitNestedReduce { inner_loop_idx, after } => {
                // Stack top: [os, oe, is, ie, init]; BELOW them sit the `K` loop-invariant array
                // bases `[arr_1 .. arr_K]` the inner reduce indexes (pushed in `captures` order —
                // empty for the scalar-only shape). Run the outer range in PARALLEL over the native
                // inner captured-reduce kernel (one call per `i`, order-preserving collect —
                // deterministic and identical to the serial outer map) when a kernel exists, the
                // five range/init operands are `Int`, both spans are within the 100M cap, the inner
                // is the single-body i64 shape (exactly one Scalar cap = the outer binder `i`, plus
                // zero or more ArrayI64 bases), every array cap is a packed `Ints`, and the HOISTED
                // bounds pre-check passes. Otherwise pop everything and fall through to the ordinary
                // map-of-reduce (the oracle path), which raises any exact OOB error.
                use crate::bytecode::{CaptureKind, IndexBound};
                use crate::value::ArrayData;
                let inner = &program.reduce_loops[*inner_loop_idx as usize];
                let n_scalar =
                    inner.captures.iter().filter(|c| c.kind == CaptureKind::Scalar).count();
                let n_arrays =
                    inner.captures.iter().filter(|c| c.kind == CaptureKind::ArrayI64).count();
                let scalar_pos = inner.captures.iter().position(|c| c.kind == CaptureKind::Scalar);
                // Only Scalar + ArrayI64 caps (no ArrayF64 / other): the counts must exhaust the
                // list. A single Scalar (the outer `i`) drives `scalar_pos`.
                let ok_shape = !inner.float
                    && inner.bodies.len() == 1
                    && n_scalar == 1
                    && n_scalar + n_arrays == inner.captures.len()
                    && scalar_pos.is_some();
                let len = stack.len();
                let result: Option<Value> = if ok_shape && len >= 5 + n_arrays {
                    jit.and_then(|j| j.reduce_loop(*inner_loop_idx as usize)).and_then(|ptr| {
                        let ops = &stack[len - 5..];
                        let (
                            Value::Int(os),
                            Value::Int(oe),
                            Value::Int(is),
                            Value::Int(ie),
                            Value::Int(init),
                        ) = (&ops[0], &ops[1], &ops[2], &ops[3], &ops[4])
                        else {
                            return None;
                        };
                        let (os, oe, is, ie, init) = (*os, *oe, *is, *ie, *init);
                        // The inner bounds are AFFINE in the outer index: `start(i) = sc*i + is`,
                        // `end(i) = ec*i + ie`, where the `is`/`ie` operands are the BASES. Both
                        // coeffs `0` ⇒ the rectangular case ⇒ the bounds are `is`/`ie` for every
                        // `i`, exactly as before. Affine ⇒ MONOTONE in `i`, so the extremes over
                        // the iterated `i` sit at the two endpoints — which is what keeps these
                        // obligations checkable ONCE, here, instead of per worker.
                        let (sc, ec) = (inner.inner_start_coeff, inner.inner_end_coeff);
                        // Match the fallback's cap: an over-cap outer range ERRORs in the
                        // fallback, so declining here keeps the two paths identical.
                        if (oe as i128 - os as i128) > 100_000_000 {
                            return None;
                        }
                        // Union `[inner_lo, inner_hi)` of every per-`i` inner range.
                        let (mut inner_lo, mut inner_hi) = (0i128, 0i128);
                        if oe > os {
                            let (i_lo, i_hi) = (os as i128, oe as i128 - 1);
                            let start_at = |i: i128| (sc as i128) * i + (is as i128);
                            let end_at = |i: i128| (ec as i128) * i + (ie as i128);
                            let (sa, sb) = (start_at(i_lo), start_at(i_hi));
                            let (ea, eb) = (end_at(i_lo), end_at(i_hi));
                            // Every per-`i` bound must fit `i64`: the workers compute them in i64,
                            // where an overflow would WRAP (diverging from the fallback, which
                            // evaluates `i + 1` on the bytecode loop). Declining keeps them equal.
                            let fits = |v: i128| v >= i64::MIN as i128 && v <= i64::MAX as i128;
                            if !fits(sa) || !fits(sb) || !fits(ea) || !fits(eb) {
                                return None;
                            }
                            // The per-`i` SPAN is affine in `i` as well, so its maximum is at an
                            // endpoint. Every span must be within the 100M cap — an over-cap span
                            // at ANY `i` would ERROR in the fallback at that `i`.
                            if (ea - sa).max(eb - sb) > 100_000_000 {
                                return None;
                            }
                            inner_lo = sa.min(sb);
                            inner_hi = ea.max(eb);
                        }
                        // Resolve the `K` array bases (below the five scalars, in `captures` order)
                        // into the caps `template`; the scalar slot stays a placeholder each worker
                        // overwrites with its own `i`. `keepalive` holds the array `Rc`s alive
                        // across the parallel region (the base pointers alias into their buffers).
                        let arr_vals = &stack[len - 5 - n_arrays..len - 5];
                        let scalar_pos = scalar_pos.unwrap();
                        let n_caps = inner.captures.len();
                        let mut template = vec![0i64; n_caps];
                        let mut lens = vec![0i64; n_caps]; // array len per slot, 0 for the scalar
                        let mut keepalive: Vec<Value> = Vec::new();
                        let mut arr_iter = arr_vals.iter();
                        for (pos, cap) in inner.captures.iter().enumerate() {
                            match cap.kind {
                                CaptureKind::Scalar => {} // filled per-worker with `i`
                                CaptureKind::ArrayI64 => {
                                    let val = arr_iter.next()?;
                                    let Value::Array(a) = val else { return None };
                                    let ArrayData::Ints(v) = &**a else { return None };
                                    template[pos] = v.as_ptr() as i64;
                                    lens[pos] = v.len() as i64;
                                    keepalive.push(val.clone());
                                }
                                // `ScalarValue` is map-only and `ArrayF64` is the f64 reduce
                                // variant this parallel path doesn't build — neither can appear on
                                // an eligible inner reduce; decline rather than assume.
                                CaptureKind::ArrayF64 | CaptureKind::ScalarValue => return None,
                            }
                        }
                        // Bounds pre-check (ONCE, before the parallel region): a counter-indexed
                        // array must cover the whole inner range of EVERY worker. The per-`i`
                        // ranges now differ (the triangular case), so the obligation is on their
                        // UNION: `[min_i start(i), max_i end(i)) ⊆ [0, len)`. That union is a
                        // conservative superset — it ignores that an EMPTY per-`i` range loads
                        // nothing — so it can only ever decline a safe shape to the serial path,
                        // never admit an out-of-bounds load. The triangular `range(i+1, n)` over
                        // `i in [0,n)` gives `[1, n) ⊆ [0, n)`: it passes. A scalar(`i`)-indexed
                        // array needs EVERY outer `i` valid, i.e. `[os,oe) ⊆ [0,len)` — exactly
                        // the serial per-`i` check `0 <= i < len` taken over all i in `[os,oe)`.
                        // Any negative or over-range → decline (fall back to the exact-erroring
                        // serial map-of-reduce, which Python-wraps a negative index or raises the
                        // precise OOB error).
                        for bnd in &inner.index_bounds {
                            match *bnd {
                                IndexBound::Counter { array } => {
                                    if oe > os
                                        && (inner_lo < 0 || inner_hi > lens[array as usize] as i128)
                                    {
                                        return None;
                                    }
                                }
                                IndexBound::Scalar { array, .. } => {
                                    if os < 0 || oe > lens[array as usize] {
                                        return None;
                                    }
                                }
                                // #31's parallel nested reduce is driven by the i64 collector, which
                                // never emits an affine obligation; and its outer binder sweeps the
                                // scalar cap, so an affine bound's endpoints would have to be
                                // re-proved per outer index. Decline until that is designed.
                                IndexBound::Affine { .. } => return None,
                            }
                        }
                        let results = unsafe {
                            crate::jit::run_nested_reduce_arrays(
                                ptr, os, oe, is, ie, sc, ec, init, &template, scalar_pos,
                            )
                        };
                        drop(keepalive); // arrays no longer aliased (results is owned) — release
                        Some(Value::int_array(results))
                    })
                } else {
                    None
                };
                stack.truncate(len - 5 - n_arrays);
                if let Some(v) = result {
                    stack.push(v);
                    frames[fi].ip = *after as usize;
                }
            }
            Op::CompInitRange => {
                // Pop [start, end], validate as integers (matching `range`), and
                // iterate lazily — no array. Validate start first (arg order).
                let end_v = stack.pop().unwrap();
                let start_v = stack.pop().unwrap();
                let start = crate::interp::as_int(&start_v, "range", line, col)?;
                let end = crate::interp::as_int(&end_v, "range", line, col)?;
                let len = (end as i128) - (start as i128);
                if len > 100_000_000 {
                    return Err(HelixError::new(
                        format!("`range` would build {} elements, which is too large", len.max(0)),
                        line,
                        col,
                    )
                    .hint("ranges are materialized eagerly — keep them under 100 million elements."));
                }
                iters.push(CompIter {
                    source: CompSource::Range { cur: start, end },
                    cur_val: Value::Unit,
                    builder: crate::value::ColumnBuilder::default(),
                });
            }
            Op::CompNext(binder, end_target, keep_cur) => {
                let li = iters.len() - 1;
                let next = match &mut iters[li].source {
                    CompSource::Array { arr, idx } => {
                        if *idx < arr.len() {
                            let el = arr.get(*idx);
                            *idx += 1;
                            Some(el)
                        } else {
                            None
                        }
                    }
                    CompSource::Range { cur, end } => {
                        if *cur < *end {
                            let el = Value::Int(*cur);
                            *cur += 1;
                            Some(el)
                        } else {
                            None
                        }
                    }
                };
                match next {
                    Some(el) => {
                        // `cur_val` is read only by `CompFilterPush`, so only filter/where
                        // (`keep_cur`) pay the clone; map/reduce/scan/any/all skip it.
                        if *keep_cur {
                            iters[li].cur_val = el.clone();
                        }
                        let base = frames[fi].base;
                        locals[base + *binder as usize] = el;
                    }
                    None => frames[fi].ip = *end_target as usize,
                }
            }
            Op::CompMapPush => {
                let v = stack.pop().unwrap();
                if let Err(lim) = iters.last_mut().unwrap().builder.push(v) {
                    return Err(materialize_refused(lim, line, col));
                }
            }
            Op::CompFilterPush(kind) => {
                let keep = stack.pop().unwrap();
                let it = iters.last_mut().unwrap();
                match keep {
                    Value::Bool(true) => {
                        // `cur_val` is dead after this (the next `CompNext` overwrites it),
                        // so move it out — no refcount bump — leaving the `Unit` placeholder.
                        let el = std::mem::replace(&mut it.cur_val, Value::Unit);
                        if let Err(lim) = it.builder.push(el) {
                            return Err(materialize_refused(lim, line, col));
                        }
                    }
                    Value::Bool(false) => {}
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "`{}` expects a yes/no test, but the expression produced {}",
                                kind.method_name(),
                                crate::value::with_article(other.type_name())
                            ),
                            line,
                            col,
                        )
                        .hint("write a comparison, e.g. `xs.filter(it > 50)`."))
                    }
                }
            }
            Op::CompEnd => {
                let it = iters.pop().unwrap();
                stack.push(it.builder.finish());
            }
            Op::CompEndDiscard => {
                iters.pop();
            }
            Op::CompFindTest { want, idx_slot, short_target } => {
                // ADR 0024: this `unwrap` cannot fire. `CompFindTest` is emitted at
                // exactly one site — `compile_position`, immediately after
                // `compile_expr(body)` — and compiling an expression always leaves
                // exactly one value on the stack. Identical to `CompBoolTest` below,
                // which is emitted the same way for the same reason.
                let v = stack.pop().unwrap();
                let base = frames[fi].base;
                // The index BEFORE the bump is the one that matched.
                let i = match &locals[base + *idx_slot as usize] {
                    Value::Int(n) => *n,
                    // Unreachable: the slot is initialized to `Int(0)` and only ever
                    // written here. Treated as "no match yet" rather than panicking.
                    _ => 0,
                };
                if matches!(v, Value::Bool(b) if b == *want) {
                    frames[fi].ip = *short_target as usize;
                } else {
                    locals[base + *idx_slot as usize] = Value::Int(i + 1);
                }
            }
            Op::CompBoolTest(is_all, sm_slot, short_target) => {
                let v = stack.pop().unwrap();
                match v {
                    Value::Bool(bv) => {
                        // `all` short-circuits on false; `any` on true.
                        if (*is_all && !bv) || (!*is_all && bv) {
                            frames[fi].ip = *short_target as usize;
                        }
                    }
                    Value::Missing => {
                        let base = frames[fi].base;
                        locals[base + *sm_slot as usize] = Value::Bool(true);
                    }
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "`{}` expects a yes/no test, but the expression produced {}",
                                if *is_all { "all" } else { "any" },
                                crate::value::with_article(other.type_name())
                            ),
                            line,
                            col,
                        )
                        .hint("write a comparison, e.g. `xs.any(it > 0)`."))
                    }
                }
            }
            Op::Raise(d) => {
                return Err(HelixError::new((*d.msg).clone(), line, col).hint((*d.hint).clone()));
            }
            Op::TryBegin(catch_ip) => {
                handlers.push(Handler {
                    stack_len: stack.len(),
                    frame_depth: frames.len(),
                    locals_len: locals.len(),
                    iters_len: iters.len(),
                    catch_ip: *catch_ip as usize,
                });
            }
            Op::TryOk(end_ip) => {
                // Body finished normally: drop the handler, wrap the value in the
                // ok-record, and jump past the catch.
                handlers.pop();
                let v = stack.pop().unwrap();
                stack.push(crate::interp::try_ok(v));
                frames[fi].ip = *end_ip as usize;
            }
            Op::TryErr => {
                // Reached via an unwind, which pushed the error message: wrap it in
                // the err-record. (The handler was already popped during the unwind.)
                let msg = match stack.pop().unwrap() {
                    Value::Str(s) => (*s).clone(),
                    other => other.to_string(),
                };
                stack.push(crate::interp::try_err(msg));
            }
            Op::Pop => {
                stack.pop();
            }
        }
        Ok(())
        })();
        if let Err(e) = step {
            match handlers.pop() {
                // Caught by the nearest active `try`: unwind every VM stack to the
                // depths recorded at `TryBegin`, resume at its catch handler with the
                // error message on the operand stack (consumed by `Op::TryErr`).
                Some(h) => {
                    stack.truncate(h.stack_len);
                    frames.truncate(h.frame_depth);
                    locals.truncate(h.locals_len);
                    iters.truncate(h.iters_len);
                    let tf = frames.len() - 1;
                    frames[tf].ip = h.catch_ip;
                    stack.push(Value::Str(std::rc::Rc::new(e.message)));
                }
                // No active `try`: propagate out of the VM as a real error.
                None => return Err(e),
            }
        }
    }

    Ok(stack)
}


#[cfg(test)]
mod tests;

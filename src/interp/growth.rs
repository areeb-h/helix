//! Size guards for the operations whose result can be much LARGER than their inputs.
//!
//! ADR 0024: a limit is an error, never a signal — never a dead process. A program that
//! doubles a value in a loop (`s.replace("a", "aa")`, `xs.concat(xs)`, `[...xs, ...xs]`)
//! used to grow it until the allocator refused and Rust's `handle_alloc_error` aborted the
//! process (`memory allocation of N bytes failed`, SIGABRT). Every such operation now works
//! out the size of its result BEFORE allocating it — exactly, or by a cheap upper bound that
//! only falls back to an exact count when the bound is not already under the cap — and
//! refuses with an ordinary, catchable `HelixError` when it would pass the line the project
//! already draws:
//!
//! - one string (and one `Bytes`) holds at most [`MAX_STRING_LEN`] bytes — `repeat`'s and
//!   interpolation's cap;
//! - one array holds at most [`MATERIALIZE_BUDGET`] bytes of element slots — the budget
//!   `map`/`filter` materialization already enforces through `ColumnBuilder`.
//!
//! No new limits are invented here. The error text is built in this one place and every
//! engine reaches it through the shared method implementations (or, for the reduce fast
//! paths `Op::ConcatIntoLocal` and its walker twin, by calling [`concat_one_guard`]
//! directly), so all three engines refuse the same program with the same words.
//!
//! A result UNDER a cap still needs memory of its own, and under a memory limit the system
//! can refuse it; where these operations allocate a large result they reserve it fallibly
//! ([`text_buffer`], [`push`], `value::reserve_more`), so that is a clean error too.

use crate::error::HelixError;
use crate::interp::MAX_STRING_LEN;
use crate::value::{ArrayData, MATERIALIZE_BUDGET, Value};

/// One boxed element slot of an array.
pub(crate) const SLOT: usize = std::mem::size_of::<Value>();

/// One element of a packed `Ints`/`Floats` column.
pub(crate) const PACKED: usize = 8;

/// One freshly allocated string element, before its text: the slot plus an allocation
/// header. The same floor `value::element_heap_estimate` charges a heap-valued element, so
/// `s.chars()` and `s.chars().map((c) => c)` are measured alike.
pub(crate) const STR_ELEMENT: usize = SLOT + 32;

/// Refuse a string (or `Bytes`) result of `bytes` bytes from `op` when it would pass
/// [`MAX_STRING_LEN`]. O(1): the caller has already worked out the size.
#[inline]
pub(crate) fn check_text(op: &str, bytes: usize, line: usize, col: usize) -> Result<(), HelixError> {
    if bytes > MAX_STRING_LEN { Err(text_too_long(op, line, col)) } else { Ok(()) }
}

/// The string-cap error, worded like `repeat`'s (`` `repeat` would exceed N bytes ``).
#[cold]
pub(crate) fn text_too_long(op: &str, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("`{op}` would exceed {MAX_STRING_LEN} bytes"), line, col).hint(
        "one string holds at most 1 GiB. Work on the text in pieces, or write each piece \
         to a file as you go.",
    )
}

/// An empty string with room for exactly `bytes` — or, when the allocator refuses, a clean
/// error rather than the abort `String::with_capacity` would raise.
///
/// The second half of ADR 0024's pair: the cap says what is too LARGE, and this says what
/// does not FIT. A result under the cap still needs its own allocation, on top of its inputs,
/// and under a memory limit that block can be refused. Used where the size is already known,
/// so it costs exactly what `with_capacity` did.
pub(crate) fn text_buffer(op: &str, bytes: usize, line: usize, col: usize) -> Result<String, HelixError> {
    let mut out = String::new();
    out.try_reserve_exact(bytes).map_err(|_| no_memory_for(op, bytes, line, col))?;
    Ok(out)
}

/// The refusal [`text_buffer`] reports, worded like `materialize_refused`'s `Alloc` arm.
#[cold]
pub(crate) fn no_memory_for(op: &str, bytes: usize, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("there is not enough memory to hold the result of `{op}`"), line, col).hint(
        format!(
            "it asked for {} MB in one block and the system refused. Work on the data in \
             pieces, or write each piece to a file as you go.",
            bytes / (1 << 20)
        ),
    )
}

/// The capacity a buffer that is being held to [`MAX_STRING_LEN`] should grow to so that
/// `need` bytes fit: doubling, as `Vec` does, but never past the cap. Plain doubling would
/// take a 600 MB buffer to a 1.2 GB allocation to hold 610 MB — the very allocation that
/// aborts under a memory limit, made by a result that is legitimately under the cap.
#[inline]
pub(crate) fn capped_capacity(cap: usize, need: usize) -> usize {
    cap.saturating_mul(2).min(MAX_STRING_LEN).max(need)
}

/// Refuse an array result whose element slots would take `bytes` bytes, when that passes
/// [`MATERIALIZE_BUDGET`]. O(1): the caller has already worked out the size.
#[inline]
pub(crate) fn check_array(op: &str, bytes: usize, line: usize, col: usize) -> Result<(), HelixError> {
    if bytes > MATERIALIZE_BUDGET { Err(array_too_large(op, bytes, line, col)) } else { Ok(()) }
}

#[cold]
fn array_too_large(op: &str, bytes: usize, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!(
            "`{op}` would build an array of more than {} MB, which is too large",
            MATERIALIZE_BUDGET / (1 << 20)
        ),
        line,
        col,
    )
    .hint(format!(
        "it would need ~{} MB. Work on the data in pieces, or stay lazy — a `map`/`filter` \
         that feeds straight into `count`/`sum`/`first` does not materialize at all.",
        bytes / (1 << 20)
    ))
}

/// Bytes per element of the array `concat` (or `flatten`) would build from these parts:
/// packed when every part is an `Int` column (or a range), or every part a `Float` column —
/// the cases the packed fast paths keep packed — and a boxed slot otherwise.
fn element_bytes<'a>(mut parts: impl Iterator<Item = &'a ArrayData> + Clone) -> usize {
    let ints = parts.clone().all(|a| matches!(a, ArrayData::Ints(_) | ArrayData::Range { .. }));
    if ints || parts.all(|a| matches!(a, ArrayData::Floats(_))) { PACKED } else { SLOT }
}

/// The guard every spelling of `concat` shares: the method (and so an array spread
/// `[...xs, ...ys]`, which the parser lowers to `concat`), the VM's `Op::ConcatIntoLocal`
/// and the walker's reduce fast path. O(number of arguments) — lengths are stored.
///
/// A call with an argument that is not an array passes: `concat` refuses that itself, in
/// its own words, and a size error must not jump ahead of it.
pub(crate) fn concat_guard(
    recv: &ArrayData,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    if !args.iter().all(|a| matches!(a, Value::Array(_))) {
        return Ok(());
    }
    let arrays = args.iter().filter_map(|a| match a {
        Value::Array(a) => Some(&**a),
        _ => None,
    });
    let len = arrays.clone().fold(recv.len(), |n, a| n.saturating_add(a.len()));
    let per = element_bytes(std::iter::once(recv).chain(arrays));
    check_array("concat", len.saturating_mul(per), line, col)
}

/// [`concat_guard`] for the reduce fast paths, which hold the accumulator and the one
/// argument as arrays already (`Op::ConcatIntoLocal` and the walker's twin).
pub(crate) fn concat_one_guard(
    cur: &ArrayData,
    add: &ArrayData,
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    let len = cur.len().saturating_add(add.len());
    check_array("concat", len.saturating_mul(element_bytes([cur, add].into_iter())), line, col)
}

/// `flatten`'s guard: one level of nesting spread out, scalars kept. O(outer length), which
/// is the outer loop `flatten` runs anyway — the inner lengths are stored.
pub(crate) fn flatten_guard(items: &[Value], line: usize, col: usize) -> Result<(), HelixError> {
    let mut len = 0usize;
    let mut all_arrays = true;
    for v in items {
        match v {
            Value::Array(a) => len = len.saturating_add(a.len()),
            _ => {
                all_arrays = false;
                len = len.saturating_add(1);
            }
        }
    }
    let per = if all_arrays && !items.is_empty() {
        element_bytes(items.iter().filter_map(|v| match v {
            Value::Array(a) => Some(&**a),
            _ => None,
        }))
    } else {
        SLOT
    };
    check_array("flatten", len.saturating_mul(per), line, col)
}

/// `s.replace(from, to)`, held to the string cap.
///
/// The bound `s.len() + max_matches * growth` settles every ordinary call in O(1); only a
/// call whose WORST case passes the cap counts its matches, to refuse it or size it exactly.
/// The text is then built by `str::replace`'s own loop, but into a buffer that grows
/// FALLIBLY and never past the cap — a result under the cap still needs its own memory, and
/// near the cap the allocator can refuse (the `html_escape(s.replace("a", "&"))` doubling
/// loop died in exactly this copy, whose result was no larger than its input).
pub(crate) fn replace_all(
    op: &str,
    s: &str,
    from: &str,
    to: &str,
    line: usize,
    col: usize,
) -> Result<String, HelixError> {
    let size = if to.len() == from.len() {
        s.len()
    } else if to.len() < from.len() {
        // Shrinks: grown on demand, as `str::replace` does.
        0
    } else {
        let grow = to.len() - from.len();
        // An empty pattern matches at every character boundary: at most `len + 1` times.
        let most = if from.is_empty() { s.len() + 1 } else { s.len() / from.len() };
        if s.len().saturating_add(most.saturating_mul(grow)) <= MAX_STRING_LEN {
            s.len()
        } else {
            let hits = s.matches(from).count();
            let bytes = s.len().saturating_add(hits.saturating_mul(grow));
            check_text(op, bytes, line, col)?;
            bytes
        }
    };
    let mut out = text_buffer(op, size, line, col)?;
    let mut last = 0;
    for (at, hit) in s.match_indices(from) {
        push(op, &mut out, &s[last..at], line, col)?;
        push(op, &mut out, to, line, col)?;
        last = at + hit.len();
    }
    push(op, &mut out, &s[last..], line, col)?;
    Ok(out)
}

/// `out.push_str(piece)`, growing as `String` does but fallibly and never past the cap —
/// a push that would pass the cap is refused with `op`'s cap error.
#[inline]
pub(crate) fn push(op: &str, out: &mut String, piece: &str, line: usize, col: usize) -> Result<(), HelixError> {
    let need = out.len().saturating_add(piece.len());
    if need > MAX_STRING_LEN {
        return Err(text_too_long(op, line, col));
    }
    if need > out.capacity() {
        let to = capped_capacity(out.capacity(), need);
        out.try_reserve_exact(to - out.len()).map_err(|_| no_memory_for(op, to, line, col))?;
    }
    out.push_str(piece);
    Ok(())
}

/// Standard base64 with padding, into a buffer reserved fallibly — `base64_encode` and
/// `Bytes.to_base64`. The length is exact: four characters for every three bytes, rounded up.
pub(crate) fn base64(op: &str, bytes: &[u8], line: usize, col: usize) -> Result<String, HelixError> {
    use base64::Engine;
    let len = bytes.len().div_ceil(3).saturating_mul(4);
    check_text(op, len, line, col)?;
    let mut out = text_buffer(op, len, line, col)?;
    base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut out);
    Ok(out)
}

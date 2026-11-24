//! Values of any depth: walking one, and freeing one, without overflowing the native stack.
//!
//! A Helix program can build a value nested as deep as its memory allows — a tail-recursive
//! loop that wraps an accumulator, `build(n - 1, [acc])` or `{next: acc}`, makes a million
//! levels in well under a second. Everything the runtime did to such a value recursed once
//! per level on the native stack: printing it, interpolating it, `==`, `to_json`, `unique`,
//! sorting, arithmetic over nested arrays — and, last of all, FREEING it, which is
//! compiler-generated drop glue nobody wrote. At 1M levels the first group overflowed the
//! evaluator's 128 MiB stack, and at ~3M (a chain of dicts far sooner) merely letting the
//! value go out of scope did. The process died with `fatal runtime error: stack overflow`,
//! on all three engines, for a program that had done nothing wrong. ADR 0024: a limit is an
//! error, never a signal — and depth was never a limit the language stated.
//!
//! Two tools, one per half of the problem:
//!
//! * [`nested`] wraps each step a recursive WALK takes into a child container. Below a red
//!   zone it continues on a fresh heap-allocated stack segment (`stacker`), exactly as the
//!   tree-walker already does for deep Helix calls (`STACK_RED_ZONE` in `interp.rs`). A walk
//!   costs stack in proportion to the depth it has reached, and that stack comes from the
//!   heap like the value it walks: a program that could build the value can walk it.
//! * Freeing is the `Drop` of each container's PAYLOAD — [`crate::value::Items`] (a tuple's),
//!   [`crate::value::Fields`] (a record's), `ArrayData`, `Dict`, `ClosureData` and `FuncVal`
//!   — which runs only when the last reference to that payload goes. Freeing cannot borrow
//!   stack the way a walk does (the glue that frees a payload's children runs after `drop`
//!   returns, outside any closure), so the payload moves its contents out and frees them
//!   itself, counting how many such frees are in progress on this thread. Up to
//!   [`FREE_DEPTH`] that is ordinary recursion, exactly what the glue did; past it the
//!   contents are parked on a per-thread list, and the outermost free lets go of them once
//!   it is back at the top of the stack. Freeing any value therefore uses bounded stack.
//!
//! Both are pay-as-you-go. `nested` is a stack-pointer comparison. A free costs nothing at
//! all for a scalar or for a reference to a container something else still holds — the hook
//! is on the payload, so it never runs for those — and a small payload of leaves returns
//! after looking at its few children; anything larger pays two thread-local counter updates
//! and a move of its contents, and allocates only if something is actually parked.

use std::rc::Rc;

use crate::symbol::Symbol;
use crate::value::{ArrayData, DictKey, DictMap, Value};

/// Run `f` — one level of a recursive walk over a nested value — with enough native stack.
///
/// Call it where a walk descends into a CHILD container, not at every scalar, so the walk's
/// flat work stays as fast as it was. The red zone is the stack one level may use before the
/// next check; a walker's frame is a few KB, far inside it.
#[inline]
pub(crate) fn nested<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(crate::interp::STACK_RED_ZONE, crate::interp::STACK_SEGMENT, f)
}

/// How many container frees deep a free may go on the native stack before it parks the rest
/// for later. A level costs well under a kilobyte of stack, so this stays far inside the red
/// zone every caller leaves ([`crate::interp::STACK_RED_ZONE`]).
const FREE_DEPTH: u32 = 512;

/// The most children a container's contents may have for a free to look at each of them
/// first: a small set of leaves cannot nest, so the glue frees it one level deep, uncounted,
/// as it always did. Past this, counting costs less than the scan would.
const LEAF_SCAN: usize = 8;

thread_local! {
    /// Container frees in progress on this thread, outermost first.
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Whether [`PENDING`] holds anything — a plain flag, so the outermost free can check it
    /// without touching the list itself.
    static HAS_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Contents met [`FREE_DEPTH`] frees down, waiting for the outermost free to let go of
    /// them from the top of the stack.
    static PENDING: std::cell::RefCell<Vec<Garbage>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A container's contents, moved out of it as its last owner lets go.
/// Nothing reads them: dropping one is the point.
#[allow(dead_code, reason = "held only so that dropping it frees the contents")]
enum Garbage {
    Values(Vec<Value>),
    Fields(Vec<(Symbol, Value)>),
    Named(Vec<(String, Value)>),
    Map(DictMap),
    Log(Vec<(DictKey, Option<Value>)>),
    /// A lazy view's source array, taken out of its `Rc` (whose own free is then empty).
    Array(ArrayData),
}

/// Whether a value is a container that can hold further values. A packed numeric array
/// cannot, however large, so it does not count.
#[inline(always)]
pub(crate) fn is_container(v: &Value) -> bool {
    match v {
        Value::Array(a) => !matches!(&**a, ArrayData::Ints(_) | ArrayData::Floats(_) | ArrayData::Range { .. }),
        Value::Tuple(_) | Value::Record(_) | Value::Dict(_) | Value::Closure(_) | Value::Function(_) => true,
        _ => false,
    }
}

/// Free `g` one level deeper, or — that deep already — park it for the outermost free.
///
/// Called from a container payload's `Drop`, which runs only when the LAST reference to the
/// payload goes. Dropping a scalar, or a reference to a container something else still holds,
/// never reaches here: it costs exactly what it always did.
#[inline(never)]
fn free(g: Garbage) {
    let depth = DEPTH.get();
    if depth >= FREE_DEPTH {
        // Should the list be gone (a thread tearing down its locals) this drops `g` in place,
        // as the glue would have.
        let _ = PENDING.try_with(|p| p.borrow_mut().push(g));
        HAS_PENDING.set(true);
        return;
    }
    DEPTH.set(depth + 1);
    drop(g);
    if depth == 0 && HAS_PENDING.get() {
        // The outermost free: what was parked further down is freed from here, each item on
        // a counted stack of its own, until nothing is left.
        while let Some(g) = PENDING.try_with(|p| p.borrow_mut().pop()).ok().flatten() {
            drop(g);
        }
        HAS_PENDING.set(false);
    }
    DEPTH.set(depth);
}

/// A tuple's or an array's elements, as the last owner lets go of them.
#[inline]
pub(crate) fn free_values(v: &mut Vec<Value>) {
    if v.len() <= LEAF_SCAN && !v.iter().any(is_container) {
        return;
    }
    free(Garbage::Values(std::mem::take(v)));
}

/// A record's fields.
#[inline]
pub(crate) fn free_fields(v: &mut Vec<(Symbol, Value)>) {
    if v.len() <= LEAF_SCAN && !v.iter().any(|(_, x)| is_container(x)) {
        return;
    }
    free(Garbage::Fields(std::mem::take(v)));
}

/// A function's captured values (the tree-walker's closures).
#[inline]
pub(crate) fn free_named(v: &mut Vec<(String, Value)>) {
    if v.len() <= LEAF_SCAN && !v.iter().any(|(_, x)| is_container(x)) {
        return;
    }
    free(Garbage::Named(std::mem::take(v)));
}

/// Whether a dict's map can nest: a few leaves cannot.
#[inline]
pub(crate) fn map_nests(m: &DictMap) -> bool {
    m.len() > LEAF_SCAN || m.values().any(is_container)
}

/// Whether a dict's log can nest.
#[inline]
pub(crate) fn log_nests(l: &[(DictKey, Option<Value>)]) -> bool {
    l.len() > LEAF_SCAN || l.iter().any(|(_, v)| v.as_ref().is_some_and(is_container))
}

/// A dict's map (its base, or its merged cache), as its last owner lets go of it.
pub(crate) fn free_map(m: DictMap) {
    free(Garbage::Map(m));
}

/// A dict's log of inserts and removals.
pub(crate) fn free_log(l: Vec<(DictKey, Option<Value>)>) {
    free(Garbage::Log(l));
}

/// A lazy view's source array. A chain of views holds its sources directly, with no `Value`
/// between them, so each source is taken out of its `Rc` here — swapped for an empty packed
/// array, which does not allocate — and freed one counted level deeper, where its own `Drop`
/// does the same for the source beneath it.
pub(crate) fn free_source(rc: &mut Rc<ArrayData>) {
    if let Some(data) = Rc::get_mut(rc)
        && matches!(data, ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } | ArrayData::Shared { .. })
    {
        free(Garbage::Array(std::mem::replace(data, ArrayData::Ints(Vec::new()))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    /// Runs `f` on a thread with a deliberately small stack — far less than a recursive free
    /// or walk of the values built below would need — so each test fails by overflowing if
    /// the code it checks recursed once per level.
    fn on_small_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new().stack_size(1 << 20).spawn(f).unwrap().join().unwrap();
    }

    const DEEP: usize = 200_000;

    fn nest(wrap: impl Fn(Value) -> Value) -> Value {
        let mut v = Value::Int(0);
        for _ in 0..DEEP {
            v = wrap(v);
        }
        v
    }

    fn shapes() -> Vec<(&'static str, fn(Value) -> Value)> {
        vec![
            ("array", |v| Value::array(vec![v, Value::Int(1)])),
            ("tuple", |v| Value::tuple(vec![v])),
            ("record", |v| Value::record(vec![(Symbol::intern("next"), v)])),
            ("dict", |v| {
                let mut m = std::collections::BTreeMap::new();
                m.insert(DictKey::Int(1), v);
                Value::Dict(Rc::new(crate::value::Dict::from_map(m)))
            }),
            ("closure", |v| {
                Value::Closure(Rc::new(crate::value::ClosureData { idx: 0, arity: 1, upvalues: Rc::new(vec![v]) }))
            }),
            ("lazy view", |v| match &v {
                Value::Array(a) => Value::Array(Rc::new(ArrayData::Enumerate { inner: a.clone() })),
                _ => Value::array(vec![v]),
            }),
        ]
    }

    #[test]
    fn freeing_a_deep_value_takes_one_level_of_stack() {
        for (name, wrap) in shapes() {
            on_small_stack(move || {
                let v = nest(wrap);
                drop(v);
            });
            eprintln!("freed a {DEEP}-deep {name}");
        }
    }

    #[test]
    fn walking_a_deep_value_grows_the_stack_it_needs() {
        on_small_stack(|| {
            let a = nest(|v| Value::array(vec![v]));
            let b = nest(|v| Value::array(vec![v]));
            assert_eq!(crate::interp::ops::eq3(&a, &b), Some(true));
            assert!(crate::interp::ops::values_equal(&a, &b));
            let text = crate::value::display_value(&a, 1, 1).unwrap();
            assert_eq!(text.len(), 2 * DEEP + 1);
            assert_eq!(a.to_string(), text);
            assert_eq!(crate::json::stringify(&a).unwrap().len(), 2 * DEEP + 1);
        });
    }

    /// Only what a value owns ALONE is taken apart: a nested child something else still
    /// holds keeps every level of its contents.
    #[test]
    fn a_shared_child_survives_its_parent() {
        let inner = Value::array(vec![Value::array(vec![Value::Int(7)])]);
        let keep = inner.clone();
        drop(Value::tuple(vec![Value::record(vec![(Symbol::intern("x"), inner)])]));
        assert_eq!(keep.to_string(), "[[7]]");
    }
}

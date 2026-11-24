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
//! * [`release`] is `Value`'s `Drop`. Freeing cannot borrow stack the same way — the glue
//!   that frees a container's children runs AFTER `drop` returns, outside any closure — so
//!   a container that is the last owner of its contents moves them out and frees them
//!   itself, counting how many such frees are in progress on this thread. Up to
//!   [`FREE_DEPTH`] that is ordinary recursion, exactly what the glue did; past it the
//!   contents are parked on a per-thread list, and the outermost free lets go of them once
//!   it is back at the top of the stack. Freeing any value therefore uses bounded stack.
//!
//! Both are pay-as-you-go. `nested` is a stack-pointer comparison. `release` returns at once
//! for a scalar or a shared container (dropping it only decrements a count, as it always
//! did); a sole owner pays two thread-local counter updates and a move of its contents —
//! no scan of its children, and no allocation unless something is actually parked.

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

/// How many solely-owned containers deep a free may go on the native stack before it hands
/// the rest to [`PENDING`]. A level of drop costs well under a kilobyte of stack, so this
/// stays far inside the red zone every caller leaves ([`crate::interp::STACK_RED_ZONE`]).
const FREE_DEPTH: u32 = 512;

thread_local! {
    /// Solely-owned containers being freed on this thread, outermost first.
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Whether [`PENDING`] holds anything — a plain flag, so the outermost free can check it
    /// without touching the list itself.
    static HAS_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Contents found [`FREE_DEPTH`] levels down, waiting for the outermost free to let go of
    /// them from the top of the stack.
    static PENDING: std::cell::RefCell<Vec<Garbage>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A container's contents, moved out of it so they can be freed under the depth count.
/// Nothing reads them: dropping one is the point.
#[allow(dead_code, reason = "held only so that dropping it frees the contents")]
enum Garbage {
    Values(Vec<Value>),
    Fields(Vec<(Symbol, Value)>),
    Named(Vec<(String, Value)>),
    Map(DictMap),
    Log(Vec<(DictKey, Option<Value>)>),
}

/// `Value`'s `Drop`: free a value of any depth on bounded native stack.
///
/// This runs for EVERY value the runtime drops, so the test it makes inline is one load: the
/// reference count of a container. A scalar, or a container something else still holds,
/// goes no further — dropping it only decrements a count, as it always did.
#[inline]
pub(crate) fn release(v: &mut Value) {
    let sole = match v {
        Value::Array(rc) => Rc::strong_count(rc) == 1,
        Value::Tuple(rc) => Rc::strong_count(rc) == 1,
        Value::Record(rc) => Rc::strong_count(rc) == 1,
        Value::Dict(rc) => Rc::strong_count(rc) == 1,
        Value::Closure(rc) => Rc::strong_count(rc) == 1,
        Value::Function(rc) => Rc::strong_count(rc) == 1,
        _ => false,
    };
    if sole {
        release_sole(v);
    }
}

/// A sole owner is letting go: free its contents here, counting how deep this free has
/// gone, rather than leaving them to the glue (which would recurse uncounted).
/// Whether a value is one of the containers that can hold further values. The six variants
/// are adjacent in `Value`, so this is a range check on the tag.
#[inline(always)]
fn is_container(v: &Value) -> bool {
    matches!(
        v,
        Value::Array(_) | Value::Tuple(_) | Value::Record(_) | Value::Dict(_) | Value::Closure(_) | Value::Function(_)
    )
}

/// The most children a container may have for [`release_sole`] to look at each of them
/// before deciding: a small container of leaves cannot nest, and the glue frees it one level
/// deep as it always did. Past this the scan would cost more than the counted free it saves.
const LEAF_SCAN: usize = 8;

#[inline(never)]
fn release_sole(v: &mut Value) {
    // The hot case — a small record or tuple of scalars, or a packed numeric array — cannot
    // nest, so it needs no counting: return and let the glue free it.
    let flat = match v {
        Value::Tuple(rc) => rc.len() <= LEAF_SCAN && !rc.iter().any(is_container),
        Value::Record(rc) => rc.len() <= LEAF_SCAN && !rc.iter().any(|(_, x)| is_container(x)),
        Value::Array(rc) => match &**rc {
            ArrayData::Values(xs) => xs.len() <= LEAF_SCAN && !xs.iter().any(is_container),
            ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => false,
            _ => true,
        },
        _ => false,
    };
    if flat {
        return;
    }
    let depth = DEPTH.get();
    if depth >= FREE_DEPTH {
        // Deep enough: hand the contents to the outermost free instead of descending. Should
        // the list be gone (a thread tearing down its locals), drop in place, as the glue would.
        let mut parked = false;
        take(v, |g| {
            parked = PENDING.try_with(|p| p.borrow_mut().push(g)).is_ok();
        });
        if parked {
            HAS_PENDING.set(true);
        }
        return;
    }
    DEPTH.set(depth + 1);
    take(v, drop);
    if depth == 0 && HAS_PENDING.get() {
        // The outermost free: what was parked further down is freed from here, each item
        // again on a counted stack, until nothing is left.
        loop {
            let next = PENDING.try_with(|p| p.borrow_mut().pop()).ok().flatten();
            match next {
                Some(g) => drop(g),
                None => break,
            }
        }
        HAS_PENDING.set(false);
    }
    DEPTH.set(depth);
}

/// Move the contents out of a solely-owned container and hand them to `sink` — after which
/// the container itself holds nothing, and its own drop is one level deep.
#[inline(always)]
fn take(v: &mut Value, mut sink: impl FnMut(Garbage)) {
    match v {
        Value::Array(rc) => {
            if let Some(data) = Rc::get_mut(rc) {
                take_array(data, &mut sink);
            }
        }
        Value::Tuple(rc) => {
            if let Some(xs) = Rc::get_mut(rc)
                && !xs.is_empty()
            {
                sink(Garbage::Values(std::mem::take(xs)));
            }
        }
        Value::Record(rc) => {
            if let Some(fs) = Rc::get_mut(rc)
                && !fs.is_empty()
            {
                sink(Garbage::Fields(std::mem::take(fs)));
            }
        }
        Value::Closure(rc) => {
            if let Some(c) = Rc::get_mut(rc)
                && let Some(ups) = Rc::get_mut(&mut c.upvalues)
                && !ups.is_empty()
            {
                sink(Garbage::Values(std::mem::take(ups)));
            }
        }
        Value::Function(rc) => {
            if let Some(f) = Rc::get_mut(rc) {
                if let Some(c) = Rc::get_mut(&mut f.captured)
                    && !c.is_empty()
                {
                    sink(Garbage::Named(std::mem::take(c)));
                }
                if let Some(d) = Rc::get_mut(&mut f.defaults)
                    && !d.is_empty()
                {
                    sink(Garbage::Values(std::mem::take(d)));
                }
            }
        }
        Value::Dict(rc) => {
            if let Some(d) = Rc::get_mut(rc) {
                let (map, log, cached) = d.take_contents();
                if !log.is_empty() {
                    sink(Garbage::Log(log));
                }
                if !map.is_empty() {
                    sink(Garbage::Map(map));
                }
                if !cached.is_empty() {
                    sink(Garbage::Map(cached));
                }
            }
        }
        _ => {}
    }
}

/// An array's contents. A lazy view (`Enumerate`, `Zip`) holds its source arrays directly,
/// with no `Value` between them, so a chain of views is unwound here, iteratively, down to
/// whatever arrays of values it ends in.
#[inline(always)]
fn take_array(data: &mut ArrayData, sink: &mut impl FnMut(Garbage)) {
    match data {
        ArrayData::Values(v) => {
            if !v.is_empty() {
                sink(Garbage::Values(std::mem::take(v)));
            }
        }
        ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => {
            let mut sources = Vec::new();
            let mut node = std::mem::replace(data, ArrayData::Ints(Vec::new()));
            loop {
                match node {
                    ArrayData::Enumerate { inner } => sources.push(inner),
                    ArrayData::Zip { a, b, .. } => {
                        sources.push(a);
                        sources.push(b);
                    }
                    ArrayData::Values(v) => {
                        if !v.is_empty() {
                            sink(Garbage::Values(v));
                        }
                    }
                    _ => {}
                }
                let Some(mut rc) = sources.pop() else { break };
                // A shared source is someone else's to free: nothing to unwind there.
                node = match Rc::get_mut(&mut rc) {
                    Some(d) => std::mem::replace(d, ArrayData::Ints(Vec::new())),
                    None => ArrayData::Ints(Vec::new()),
                };
            }
        }
        _ => {}
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
            ("tuple", |v| Value::Tuple(Rc::new(vec![v]))),
            ("record", |v| Value::Record(Rc::new(vec![(Symbol::intern("next"), v)]))),
            ("dict", |v| {
                let mut m = std::collections::BTreeMap::new();
                m.insert(DictKey::Int(1), v);
                Value::Dict(Rc::new(crate::value::Dict::from_map(m)))
            }),
            ("closure", |v| {
                Value::Closure(Rc::new(crate::value::ClosureData { idx: 0, arity: 1, upvalues: Rc::new(vec![v]) }))
            }),
            ("lazy view", |v| match v.into_array() {
                Ok(a) => Value::Array(Rc::new(ArrayData::Enumerate { inner: a })),
                Err(other) => Value::array(vec![other]),
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
        drop(Value::Tuple(Rc::new(vec![Value::Record(Rc::new(vec![(Symbol::intern("x"), inner)]))])));
        assert_eq!(keep.to_string(), "[[7]]");
    }

    /// The in-place fast paths depend on a sole owner staying the sole owner when its payload
    /// is taken out of a `Value` (`into_array` and friends).
    #[test]
    fn taking_a_payload_keeps_it_uniquely_owned() {
        let v = Value::array(vec![Value::Str(Rc::new("a".into()))]);
        let mut a = v.into_array().ok().unwrap();
        assert!(Rc::get_mut(&mut a).is_some());
        let s = Value::Str(Rc::new("x".into()));
        let mut s = s.into_str().ok().unwrap();
        assert!(Rc::get_mut(&mut s).is_some());
    }
}

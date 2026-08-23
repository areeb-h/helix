//! Runtime values.
//!
//! Collections are wrapped in `Rc` so that binding, passing, and returning are
//! cheap O(1) clones rather than deep copies. Because everything is immutable,
//! this sharing is always safe — a first taste of the "zero-copy where possible"
//! principle (Arrow-backed columns come in a later phase).

use std::borrow::Cow;
use std::fmt;
use std::rc::Rc;

use ndarray::ArrayD;

use crate::ast::Expr;
use crate::backend::Df;
use crate::error::HelixError;
use crate::symbol::Symbol;

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    /// An exact arbitrary-precision rational (numerator/denominator, always in
    /// lowest terms with a positive denominator) - for exact coefficients
    /// (PSLQ/lll), exact fractions, etc. Built with `rational(n, d)`. Boxed (one
    /// word) to keep `Value` at 16 bytes; never overflows (BigInt-backed).
    Rational(Rc<num_rational::BigRational>),
    Str(Rc<String>),
    Bool(bool),
    /// An immutable array. Held behind [`ArrayData`], which stores a homogeneous
    /// numeric array as a packed `Vec<i64>`/`Vec<f64>` (half the memory of boxed
    /// `Value`s, and cache/SIMD-friendly) and falls back to `Vec<Value>` for
    /// heterogeneous/nested data — a scientific language's central data structure.
    Array(Rc<ArrayData>),
    /// A fixed-size, heterogeneous tuple: `(1, "a", true)`.
    Tuple(Rc<Vec<Value>>),
    /// HTTP headers — ordered `(name, value)` pairs whose LOOKUP is case-insensitive
    /// (RFC 9110 §5.1: field names are case-insensitive, and HTTP/2 lowercases them
    /// on the wire, so one program sees both spellings from different servers).
    /// Pairs rather than a map: insertion order is wire order, and a repeated name —
    /// `Set-Cookie` legitimately repeats — survives where a Dict collapsed it.
    /// Runtime-only (built by `http_request` responses and `conn.request()`); the
    /// checker sees `Unknown`, the opaque-type pattern shared with Dict and Net.
    Headers(Rc<Vec<(String, String)>>),
    /// An ordered record with identifier keys: `{name: "Ada", age: 41}`. Keys are
    /// interned [`Symbol`]s, so field lookup and dispatch compare a single `u32`
    /// (not a heap string), the names cost no per-record allocation, and equal
    /// field names across the whole program share one entry. The text is recovered
    /// only on cold paths (display, errors, JSON) via [`Symbol::as_str`].
    Record(Rc<Vec<(Symbol, Value)>>),
    /// A dense n-dimensional `f64` tensor (ndarray-backed). See ADR 0007.
    Tensor(Rc<ArrayD<f64>>),
    /// A columnar DataFrame, held behind the engine-agnostic backend seam (ADR
    /// 0012) as a **lazy** query plan. Verbs (`where`/`select`/`sort`/`group`)
    /// extend the plan; it materializes only at `print`/`count` — so a chain fuses
    /// into one multi-threaded pass and (with the default Polars backend's
    /// streaming engine) can run over data far larger than RAM.
    ///
    /// Wrapped in a second `Rc` so this variant is **one word** (`Df` is a fat
    /// `Rc<dyn DataHandle>` = two words): that keeps `Value` at 16 bytes for the
    /// hot scalar/array paths. Cloning is still O(1) (outer `Rc` bump); the only
    /// cost is one extra allocation at frame *creation* (a cold path) and one extra
    /// indirection on access — both negligible since DataFrames never live in hot
    /// VM loops.
    DataFrame(Rc<Df>),
    /// The intermediate produced by `df.group(keys)`, consumed by an aggregation
    /// like `.mean(col)`. Boxed behind an `Rc` because it's a rare, transient value
    /// (never stored in arrays or hot loops); inlining its two-word payload would
    /// bloat *every* `Value` — and thus every VM stack push/pop/clone — for nothing.
    GroupBy(Rc<GroupByData>),
    /// A function value — from `fn name(p) = expr` or an anonymous `p => expr`.
    /// Used by the tree-walker (which evaluates `body` directly). The two `Rc`s are
    /// collapsed behind one (`Rc<FuncVal>`) so this variant is one word, keeping
    /// `Value` at 16 bytes; the VM uses the inline `VmFunc` instead, so this never
    /// touches the bytecode hot path.
    Function(Rc<FuncVal>),
    /// A function value in the **VM**: a reference to a compiled chunk
    /// (`program.funcs[idx]`) plus its arity. Equivalent to `Function` for the user
    /// (same `<function/N>` rendering and `Function` type name); the VM uses a chunk
    /// reference instead of an AST body. No captured environment — Helix's free
    /// variables in a function value resolve to globals (the type checker rejects
    /// local-capture and higher-order calls), and globals are shared across frames.
    VmFunc {
        idx: u32,
        arity: u32,
    },
    /// A VM **closure**: a compiled chunk plus the values it captured from its
    /// enclosing scope (upvalues). `VmFunc` covers a non-capturing lambda; this
    /// variant carries the captured environment for one that closes over enclosing
    /// locals. Boxed behind an `Rc` so `Value` stays 16 bytes; renders identically
    /// to any other function.
    Closure(Rc<ClosureData>),
    /// A validated DNA sequence (uppercase A/C/G/T).
    Dna(Rc<String>),
    /// Absent data — distinct from any real value and from float `NaN`.
    /// See ADR 0001. Propagates through arithmetic/comparison; tested with
    /// `.is_missing()`, never `==`.
    Missing,
    /// The result of statements that produce no value (e.g. `print`).
    Unit,
    /// An opaque handle to a Python value (a module, function, or object) from the
    /// embedded-CPython bridge. Held behind `crate::python::PyHandle` so all pyo3
    /// contact stays in `src/python.rs`; cloning shares one strong Python reference
    /// and dropping releases it. See ADR/Phase 6. The `python` global is a handle
    /// too (a namespace marker). Always compiled; only its bridge body is gated.
    PyObject(Rc<crate::python::PyHandle>),
    /// A node in a reverse-mode autodiff graph (`src/autodiff.rs`): a tracked
    /// scalar/tensor that records how it was computed so `gradient(...)` can
    /// backpropagate. Created by `variable(x)`; one word (`Rc`), so `Value` stays
    /// 16 bytes. Pure at the surface — the graph's interior mutability is hidden.
    Node(Rc<crate::autodiff::Node>),
    /// A keyed map with O(log n) lookup (ADR 0020) — the escape from O(n) `.where`/
    /// `.contains` scans for counting, vocabularies, bigram tables, etc. A `BTreeMap`
    /// (not a hash map) so iteration order is **deterministic** (sorted by key),
    /// matching Helix's reproducibility guarantee. Keys are hashable scalars
    /// ([`DictKey`]: int, string, bool, or DNA); values are any `Value`. Immutable —
    /// `insert`/`remove` return a new dict (copy-on-write when uniquely owned).
    Dict(Rc<std::collections::BTreeMap<DictKey, Value>>),
    /// An opaque network handle — a bound HTTP listener or a single accepted
    /// connection (see `src/serve.rs`). Held behind an `Rc` so the variant is one
    /// word. Effect-only (created by `listen`, consumed by `accept`/`respond`); never
    /// compared, serialized, or sent through a computed result, so — like `PyObject`
    /// — it falls through every value-equality / structural path with no extra arms.
    Net(Rc<crate::serve::NetHandle>),
}

/// A [`Value::Dict`] key: a hashable, totally-orderable scalar. Floats are excluded
/// (NaN has no total order and float-equality keys are a footgun), as are arrays,
/// records, and other compound values — so a key is always cheap to compare and the
/// map's ordering is well-defined. The variant order here defines the cross-type sort
/// order used for deterministic iteration.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DictKey {
    Bool(bool),
    Int(i64),
    Str(Rc<String>),
    Dna(Rc<String>),
}

impl DictKey {
    /// Convert a value into a dict key, rejecting unhashable kinds with a message.
    pub fn from_value(v: &Value) -> Result<DictKey, String> {
        match v {
            Value::Int(i) => Ok(DictKey::Int(*i)),
            Value::Bool(b) => Ok(DictKey::Bool(*b)),
            Value::Str(s) => Ok(DictKey::Str(s.clone())),
            Value::Dna(s) => Ok(DictKey::Dna(s.clone())),
            other => Err(format!(
                "a dict key must be an int, string, bool, or DNA sequence, not {}",
                crate::value::with_article(other.type_name())
            )),
        }
    }

    /// Recover the key as a `Value` (for `keys()` and display).
    pub fn to_value(&self) -> Value {
        match self {
            DictKey::Bool(b) => Value::Bool(*b),
            DictKey::Int(i) => Value::Int(*i),
            DictKey::Str(s) => Value::Str(s.clone()),
            DictKey::Dna(s) => Value::Dna(s.clone()),
        }
    }
}

/// The backing store of a [`Value::Array`]. Homogeneous numeric arrays are kept as
/// packed primitive `Vec`s — half the footprint of `Vec<Value>` (8 vs 16 bytes per
/// element) and contiguous, so reductions are cache-friendly and vectorizable.
/// Anything heterogeneous, nested, or containing `missing`/strings/bools uses
/// `Values`. Element access materializes a scalar `Value` on demand.
#[derive(Debug, Clone)]
pub enum ArrayData {
    /// General case: heterogeneous, nested, or non-numeric elements.
    Values(Vec<Value>),
    /// Homogeneous `Int` (an all-int literal, a materialized range, an int column).
    Ints(Vec<i64>),
    /// Homogeneous `Float`.
    Floats(Vec<f64>),
    /// A **lazy** arithmetic integer range: element `i` is `start + step * i` for `i` in
    /// `0..len` — the value `range(...)` returns, WITHOUT materializing the `Vec<i64>`. This
    /// makes `range(N).first()/.last()/.count()/.length()/.take()/.drop()` O(1) (the eager-
    /// materialization pain: `range(20M).first()` no longer allocates 160 MB). Behaviourally
    /// IDENTICAL to the equivalent `Ints` array — `get`/`len`/`to_values` produce the same
    /// elements — so any consumer without a lazy fast path reads it element-wise or materializes
    /// via [`ArrayData::to_ints`]/[`ArrayData::densify`], staying bit-identical across all three
    /// engines. Invariant (upheld by `int_range`/`int_range_step`): `start + step*(len-1)` fits
    /// `i64`, so no element computation overflows.
    Range { start: i64, step: i64, len: usize },
    /// A **lazy** `enumerate()`: element `i` is the tuple `(i, inner[i])`, produced on
    /// demand — the value `xs.enumerate()` returns WITHOUT materializing the `Vec` of
    /// `Vec<Value>` tuples. `xs.enumerate().map((i, x) => …)` / `.filter(…)` iterate one
    /// transient tuple at a time (the VM's `CompNext` and the tree-walker's `iter_values`
    /// read element-wise), so a 100M-element enumerate no longer allocates ~5 GB of boxed
    /// tuples. Behaviourally IDENTICAL to the materialized tuple array — `get`/`len`/
    /// `to_values` yield the same `(index, element)` tuples — so any consumer without a
    /// lazy fast path materializes via [`ArrayData::to_values`] and stays bit-identical
    /// across all three engines. `inner` shares the receiver's `Rc` (no element copy).
    Enumerate { inner: std::rc::Rc<ArrayData> },
    /// A **lazy** `zip()`: element `i` is the tuple `(a[i], b[i])`, produced on demand —
    /// [`Enumerate`](ArrayData::Enumerate)'s symmetric twin, and for the same reason.
    /// `a.zip(a).length()` on a 5M range cost **631 MB** where the enumerate analogue cost
    /// 15 MB, because `zip` materialized a `Vec<Value>` of `Rc<Vec<Value>>` tuples before
    /// anything downstream could decline it. Both exist only to pair elements.
    ///
    /// Behaviourally IDENTICAL to that materialized tuple array by construction:
    /// `to_values()` IS `(0..len).map(get)`, and `get(i)` builds exactly the tuple the eager
    /// path built.
    ///
    /// TWO INVARIANTS, both load-bearing:
    ///
    /// 1. **`len` is `min(a.len(), b.len())`, computed ONCE at construction.** Not derived on
    ///    demand: `z = z.zip(z)` is legal, and a recursive `min` over a doubling chain is
    ///    exponential. It is also what makes truncation ("zip stops at the shorter side")
    ///    a stored fact rather than a re-derived one — `first`/`last` must read this `len`,
    ///    never `a.len()`, or a truncating zip indexes `b` out of bounds and aborts.
    /// 2. **Both `Rc`s are clones held for this value's lifetime.** So every `Rc::get_mut`
    ///    in-place optimizer in the tree declines on both sources, and neither buffer's
    ///    contents nor length can change underneath the frozen `len`.
    Zip { a: std::rc::Rc<ArrayData>, b: std::rc::Rc<ArrayData>, len: usize },
}

impl ArrayData {
    pub fn len(&self) -> usize {
        match self {
            ArrayData::Values(v) => v.len(),
            ArrayData::Ints(v) => v.len(),
            ArrayData::Floats(v) => v.len(),
            ArrayData::Range { len, .. } => *len,
            ArrayData::Enumerate { inner } => inner.len(),
            // The FROZEN length — see the variant's invariant 1. Never `a.len().min(b.len())`.
            ArrayData::Zip { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The element at `i` as a `Value` (materializes one scalar for typed arrays).
    /// Callers index in range (bounds are checked at the language level first).
    pub fn get(&self, i: usize) -> Value {
        match self {
            ArrayData::Values(v) => v[i].clone(),
            ArrayData::Ints(v) => Value::Int(v[i]),
            ArrayData::Floats(v) => Value::Float(v[i]),
            // i128 intermediate so `step * i` can't overflow before the (in-range by the
            // invariant) result is cast back to `i64`.
            ArrayData::Range { start, step, .. } => {
                Value::Int((*start as i128 + *step as i128 * i as i128) as i64)
            }
            // Lazy `enumerate`: the `(index, element)` tuple, built on demand — identical
            // to the materialized `Value::Tuple([Int(i), inner[i]])`.
            ArrayData::Enumerate { inner } => {
                Value::Tuple(std::rc::Rc::new(vec![Value::Int(i as i64), inner.get(i)]))
            }
            // Lazy `zip`: the `(a[i], b[i])` pair, built on demand — identical to the
            // materialized `Value::Tuple([a[i], b[i]])` the eager path built.
            ArrayData::Zip { a, b, .. } => {
                Value::Tuple(std::rc::Rc::new(vec![a.get(i), b.get(i)]))
            }
        }
    }

    /// The `i`-th element of a lazy range as a raw `i64` (helper for the typed fast paths).
    #[inline]
    fn range_at(start: i64, step: i64, i: usize) -> i64 {
        (start as i128 + step as i128 * i as i128) as i64
    }

    /// View a lazy [`ArrayData::Range`] (or an already-`Ints` array) as packed `i64`s — the
    /// `Ints` case borrows (zero-copy), the `Range` case materializes. `None` for `Floats`/
    /// `Values`. The escape hatch a consumer without a lazy fast path uses to treat a range as
    /// an ordinary `Int` array.
    pub fn to_ints(&self) -> Option<Cow<'_, [i64]>> {
        match self {
            ArrayData::Ints(v) => Some(Cow::Borrowed(v)),
            ArrayData::Range { start, step, len } => {
                Some(Cow::Owned((0..*len).map(|i| Self::range_at(*start, *step, i)).collect()))
            }
            _ => None,
        }
    }

    /// View the elements as `[Value]`. **Zero-cost** for the general (`Values`)
    /// case — a borrow — and materializes a fresh `Vec<Value>` only for a typed
    /// array. The fallback path for any operation without a typed fast path.
    pub fn to_values(&self) -> Cow<'_, [Value]> {
        match self {
            ArrayData::Values(v) => Cow::Borrowed(v),
            ArrayData::Ints(v) => Cow::Owned(v.iter().map(|&n| Value::Int(n)).collect()),
            ArrayData::Floats(v) => Cow::Owned(v.iter().map(|&f| Value::Float(f)).collect()),
            ArrayData::Range { .. } | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => {
                Cow::Owned((0..self.len()).map(|i| self.get(i)).collect())
            }
        }
    }

    /// Iterate the elements as owned `Value`s **without materializing a `Vec`** — the
    /// per-element hot-path alternative to `to_values()` (broadcast, elementwise ops)
    /// where the intermediate `Vec<Value>` would be allocated only to be iterated and
    /// dropped. Boxes one scalar (or clones one `Value`) on demand. Index-map form so it
    /// is a single concrete iterator type across the variants (no `Box<dyn>`).
    pub fn iter_values(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }
}

/// An incremental, type-adaptive array builder. It accumulates packed `Int`/`Float`
/// while the pushed elements stay homogeneous, and promotes to a general
/// `Vec<Value>` on the first non-matching element. `finish()` yields the same array
/// as collecting a `Vec<Value>` and calling [`Value::array_sniff`] — but a numeric
/// comprehension result (`xs.map(...)`) never materializes the intermediate boxed
/// `Value`s, halving the transient memory of building a numeric column.
#[derive(Default)]
pub enum ColumnKind {
    #[default]
    Empty,
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Values(Vec<Value>),
}

/// The budget, in bytes of *estimated element heap*, for one materialized collection
/// of heap-valued elements. See [`ColumnBuilder::over_budget`].
///
/// This is a POLICY NUMBER, not a law of nature. 1 GiB is the line the project already
/// draws for a single oversized value (`repeat`'s scalar-string cap), so collection
/// materialization uses the same one rather than inventing a second.
pub const MATERIALIZE_BUDGET: usize = 1 << 30;

/// A conservative O(1) estimate of the heap a single element owns.
///
/// **Shallow on purpose.** A nested array counts its own buffer but not its children's,
/// because a deep walk would be O(n) per push and turn materialization quadratic. It is
/// therefore an UNDER-estimate for deeply nested elements — see the residual risk note
/// in `docs/v0.2.1-fix-plan.md` (B3).
fn element_heap_estimate(v: &Value) -> usize {
    // Every heap-valued element costs at least its `Value` slot plus an allocation
    // header; `[i]` measured ~64 B in practice, so the floor is deliberately not 0.
    const SLOT: usize = std::mem::size_of::<Value>() + 32;
    SLOT + match v {
        Value::Str(s) => s.len(),
        Value::Array(a) => a.len() * std::mem::size_of::<Value>(),
        Value::Record(fields) => fields.len() * (std::mem::size_of::<Value>() + 8),
        _ => 0,
    }
}

/// Why a materialization was refused. Both arms are ordinary Helix errors — ADR 0024:
/// a limit is reported, never signalled by killing the process.
pub enum MaterializeLimit {
    /// The *estimated* element heap passed [`MATERIALIZE_BUDGET`]. This is the guard for
    /// many small heap-valued elements — 100M one-element arrays are only 2.4 GB of
    /// `Vec<Value>` slots, so no single allocation is outrageous and the allocator would
    /// happily grant them one at a time until the machine died.
    Budget(usize),
    /// The allocator refused to grow the buffer: this one allocation was too big for the
    /// machine (or for a `ulimit`). Caught with `try_reserve`, which is exactly the case
    /// a byte budget CANNOT model, because the limit is the environment's, not ours.
    Alloc(usize),
}

/// Reserve room for one more element, doubling as `Vec` would, but *fallibly*.
///
/// `Vec::push` grows by reallocating to twice the capacity, and during the move both
/// buffers are live — so the transient peak is ~3x the current buffer. That overshoot is
/// what actually aborted: a 100M-element `i64` column is a legitimate 800 MB, but growing
/// it reallocates to 1 GiB, and `handle_alloc_error` aborts the process rather than
/// returning. `try_reserve` turns exactly that failure into a value we can report.
fn reserve_one<T>(v: &mut Vec<T>) -> Result<(), MaterializeLimit> {
    if v.len() == v.capacity() {
        let more = v.capacity().max(1);
        v.try_reserve(more).map_err(|_| {
            MaterializeLimit::Alloc((v.capacity() + more) * std::mem::size_of::<T>())
        })?;
    }
    Ok(())
}

/// Promote a packed buffer to boxed `Value`s, reserving fallibly up front.
fn promote<T: Copy>(
    src: Vec<T>,
    wrap: fn(T) -> Value,
    extra: Value,
) -> Result<Vec<Value>, MaterializeLimit> {
    let mut vals: Vec<Value> = Vec::new();
    vals.try_reserve(src.len() + 1).map_err(|_| {
        MaterializeLimit::Alloc((src.len() + 1) * std::mem::size_of::<Value>())
    })?;
    vals.extend(src.into_iter().map(wrap));
    vals.push(extra);
    Ok(vals)
}

#[derive(Default)]
pub struct ColumnBuilder {
    kind: ColumnKind,
    /// Running estimate of heap owned by `Values` elements. Stays 0 for the packed
    /// `Ints`/`Floats` arms, whose cost is 8 bytes/element in one contiguous buffer and
    /// so is caught by [`reserve_one`]'s fallible growth instead — which is why the
    /// numeric hot path (`xs.map(i => i * 2)`) pays NOTHING for the estimate and keeps
    /// its packed representation.
    heap_bytes: usize,
}

impl ColumnBuilder {
    /// Append one element, refusing rather than aborting when it will not fit.
    ///
    /// On `Err` the partially built buffer has already been dropped, which frees the
    /// memory before the caller renders the error — deliberate, since we are by
    /// definition under memory pressure at that moment.
    pub fn push(&mut self, v: Value) -> Result<(), MaterializeLimit> {
        match (std::mem::take(&mut self.kind), v) {
            (ColumnKind::Empty, Value::Int(i)) => self.kind = ColumnKind::Ints(vec![i]),
            (ColumnKind::Empty, Value::Float(f)) => self.kind = ColumnKind::Floats(vec![f]),
            (ColumnKind::Empty, other) => {
                self.heap_bytes += element_heap_estimate(&other);
                self.kind = ColumnKind::Values(vec![other]);
            }
            (ColumnKind::Ints(mut v), Value::Int(i)) => {
                reserve_one(&mut v)?;
                v.push(i);
                self.kind = ColumnKind::Ints(v);
            }
            (ColumnKind::Floats(mut v), Value::Float(f)) => {
                reserve_one(&mut v)?;
                v.push(f);
                self.kind = ColumnKind::Floats(v);
            }
            (ColumnKind::Values(mut v), other) => {
                reserve_one(&mut v)?;
                self.heap_bytes += element_heap_estimate(&other);
                v.push(other);
                self.kind = ColumnKind::Values(v);
            }
            // Homogeneity broken: promote the packed buffer to boxed `Value`s.
            (ColumnKind::Ints(v), other) => {
                let n = v.len();
                let add = element_heap_estimate(&other);
                let vals = promote(v, Value::Int, other)?;
                self.heap_bytes = n * std::mem::size_of::<Value>() + add;
                self.kind = ColumnKind::Values(vals);
            }
            (ColumnKind::Floats(v), other) => {
                let n = v.len();
                let add = element_heap_estimate(&other);
                let vals = promote(v, Value::Float, other)?;
                self.heap_bytes = n * std::mem::size_of::<Value>() + add;
                self.kind = ColumnKind::Values(vals);
            }
        }
        match self.over_budget() {
            Some(bytes) => Err(MaterializeLimit::Budget(bytes)),
            None => Ok(()),
        }
    }

    /// `Some(bytes)` once the estimated element heap exceeds [`MATERIALIZE_BUDGET`].
    ///
    /// The caller turns this into a clean `HelixError` at its own source position.
    /// It has to be a PRE-EMPTIVE budget rather than a fallible allocation: by the
    /// time the process actually runs out, the allocation that fails is a *tiny* one
    /// (48 bytes, deep inside an unrelated `Rc`), and Rust's `handle_alloc_error`
    /// aborts the process — `try_reserve` on the result vector cannot see it coming.
    #[inline]
    pub fn over_budget(&self) -> Option<usize> {
        (self.heap_bytes > MATERIALIZE_BUDGET).then_some(self.heap_bytes)
    }

    pub fn finish(self) -> Value {
        match self.kind {
            ColumnKind::Empty => Value::array(Vec::new()),
            ColumnKind::Ints(v) => Value::int_array(v),
            ColumnKind::Floats(v) => Value::float_array(v),
            ColumnKind::Values(v) => Value::array(v),
        }
    }
}

/// The payload of a [`Value::GroupBy`] — the grouped frame plus its key columns,
/// held behind an `Rc` so the `Value` variant is one word wide.
pub struct GroupByData {
    pub handle: Df,
    pub keys: Rc<Vec<String>>,
}

/// The payload of a [`Value::Function`] (tree-walker function value), held behind
/// an `Rc` so the variant is one word wide.
pub struct FuncVal {
    pub params: Rc<Vec<String>>,
    pub body: Rc<Expr>,
    /// Variables captured from the enclosing scope when this was created — a
    /// closure's lexical environment, snapshotted by value (Helix locals are
    /// immutable, so by-value capture is exact). Empty for a top-level `fn`, whose
    /// free names are globals resolved at call time. Installed under the parameters
    /// when the function is applied, so a returned/stored closure still sees them.
    pub captured: Rc<Vec<(String, Value)>>,
    /// The name this was DECLARED under by a top-level `fn` statement (`None`
    /// for a `=>` lambda). Drives the walker's tail-call optimization: a tail
    /// call is frame-reused only when it targets an unshadowed top-level `fn`
    /// **called by its declared name** — the exact shape the VM's
    /// `CallFn`→`TailCallFn` peephole optimizes. The name (not a bool) matters
    /// because an immutable-global ALIAS (`h = id`) shares this value, and the
    /// VM dispatches `h(..)` dynamically (`resolve` prefers globals), never
    /// frame-reusing it — the walker must refuse the same call.
    pub decl_name: Option<std::rc::Rc<str>>,
}

/// The payload of a [`Value::Closure`] — a compiled-chunk index plus the values it
/// closed over (upvalues), captured by value at creation (Helix locals are
/// immutable, so by-value capture is exact). The VM's analogue of [`FuncVal`].
pub struct ClosureData {
    pub idx: u32,
    pub arity: u32,
    /// Captured values (`Rc` so a call can share them into the frame with no copy).
    pub upvalues: Rc<Vec<Value>>,
}

// The interpreter copies `Value`s constantly (every VM stack op, every binding),
// so its size is a hot-path constant. Keep it small: scalars and Rc-wrapped
// collections fit in two words. A regression here (e.g. inlining a fat variant)
// would silently tax every clone.
const _: () = assert!(
    std::mem::size_of::<Value>() <= 16,
    "Value grew past 2 words — box the offending variant",
);

impl Value {
    /// Wrap a backend DataFrame handle into a `Value`. The extra `Rc` keeps the
    /// `DataFrame` variant one word wide (see the variant's doc); construct through
    /// here so the wrapping lives in one place.
    pub fn dataframe(df: Df) -> Value {
        Value::DataFrame(Rc::new(df))
    }

    /// A general (`Values`) array. The default array constructor — use the typed
    /// ones below only when the elements are known-homogeneous primitives.
    pub fn array(items: Vec<Value>) -> Value {
        Value::Array(Rc::new(ArrayData::Values(items)))
    }

    /// A packed `Int` array (half the memory of boxed `Value`s).
    pub fn int_array(items: Vec<i64>) -> Value {
        Value::Array(Rc::new(ArrayData::Ints(items)))
    }

    /// A packed `Float` array.
    pub fn float_array(items: Vec<f64>) -> Value {
        Value::Array(Rc::new(ArrayData::Floats(items)))
    }

    /// A **lazy** integer range `[start, start+step, …]` of `len` elements — what `range(...)`
    /// returns without materializing the `Vec<i64>`. See [`ArrayData::Range`].
    pub fn lazy_range(start: i64, step: i64, len: usize) -> Value {
        Value::Array(Rc::new(ArrayData::Range { start, step, len }))
    }

    /// Build an array, **packing** it into a typed `Int`/`Float` column when the
    /// elements are homogeneous primitives (half the memory) — otherwise a general
    /// `Values` array. One O(n) scan; use for array literals and other places that
    /// build a `Vec<Value>` that is often homogeneous.
    pub fn array_sniff(items: Vec<Value>) -> Value {
        if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Int(_))) {
            let ints = items
                .iter()
                .map(|v| if let Value::Int(i) = v { *i } else { 0 })
                .collect();
            Value::int_array(ints)
        } else if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Float(_))) {
            let floats = items
                .iter()
                .map(|v| if let Value::Float(f) = v { *f } else { 0.0 })
                .collect();
            Value::float_array(floats)
        } else {
            Value::array(items)
        }
    }

    /// `cur.concat(add)` where `cur` is OWNED — extending it in place when nothing else
    /// holds a reference. Backs [`crate::bytecode::Op::ConcatIntoLocal`]; see that op for
    /// why the caller owning `cur` is the whole point.
    ///
    /// The fast paths are deliberately narrow, and each carries a REPRESENTATION proof.
    /// `array_sniff` REPACKS — a `Values` array whose contents happen to be homogeneous
    /// comes back as `Ints`, and an empty result comes back as `Values` — so extending in
    /// place is legal only where the sniffed result is provably the same representation:
    /// same packed form on both sides with a non-empty result (the two packed arms), or a
    /// `Values` accumulator gaining a NON-NUMERIC element, which makes repacking
    /// impossible by construction (the `Values` arm). That is not cosmetic: the
    /// representation decides which packed kernels a later stage can take, so the two
    /// spellings would silently diverge in performance and, through the packed paths,
    /// potentially in more than that. Everything else falls through to the identical
    /// `to_values` + `array_sniff` `concat` performs.
    pub fn concat_in_place(cur: Rc<ArrayData>, add: &ArrayData) -> Value {
        let mut cur = cur;
        match (Rc::get_mut(&mut cur), add) {
            (Some(ArrayData::Ints(v)), ArrayData::Ints(w)) if !v.is_empty() || !w.is_empty() => {
                v.extend_from_slice(w);
                return Value::Array(cur);
            }
            (Some(ArrayData::Floats(v)), ArrayData::Floats(w)) if !v.is_empty() || !w.is_empty() => {
                v.extend_from_slice(w);
                return Value::Array(cur);
            }
            // A `Values` accumulator whose argument carries a NON-NUMERIC witness. The
            // representation argument that keeps this fast path honest: `array_sniff`
            // returns `Values` unless ALL elements are Int (or all Float), and the
            // witness makes that impossible for the combined result — so extending in
            // place is representation-identical to the `to_values` + `array_sniff` path
            // below, provably, not incidentally. A numeric-only `add` still falls
            // through: an all-int result must sniff back to packed `Ints`. The guard is
            // O(|add|) — the payload's own cost. This is what makes the fold spelling
            // `lines.reduce([], (acc, s) => acc.concat([s]))` linear for strings and
            // records (measured 218.5s -> sub-second at 256k pieces): without it every
            // step re-boxed the whole accumulator just to append one element.
            (Some(ArrayData::Values(v)), add)
                if add
                    .to_values()
                    .iter()
                    .any(|x| !matches!(x, Value::Int(_) | Value::Float(_))) =>
            {
                v.extend(add.to_values().iter().cloned());
                return Value::Array(cur);
            }
            _ => {}
        }
        let mut out = cur.to_values().into_owned();
        out.extend(add.to_values().iter().cloned());
        Value::array_sniff(out)
    }

    /// `cur.insert(k, v)` where `cur` is OWNED — mutating in place when nothing else holds a
    /// reference. Backs [`crate::bytecode::Op::InsertIntoLocal`]. `insert` otherwise clones
    /// the whole `BTreeMap` per call, which is what made building a dictionary in a fold
    /// quadratic. No representation subtlety here (unlike the array case): a `BTreeMap` has
    /// one form, so the fast and slow paths are the same map either way.
    pub fn insert_in_place(
        cur: Rc<std::collections::BTreeMap<DictKey, Value>>,
        k: DictKey,
        v: Value,
    ) -> Value {
        let mut cur = cur;
        match Rc::get_mut(&mut cur) {
            Some(m) => {
                m.insert(k, v);
                Value::Dict(cur)
            }
            None => {
                let mut new = (*cur).clone();
                new.insert(k, v);
                Value::Dict(Rc::new(new))
            }
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::Rational(_) => "Rational",
            Value::Str(_) => "String",
            Value::Bool(_) => "Bool",
            Value::Array(_) => "Array",
            Value::Tuple(_) => "Tuple",
            Value::Record(_) => "Record",
            Value::Tensor(_) => "Tensor",
            Value::DataFrame(_) => "DataFrame",
            Value::GroupBy(_) => "GroupBy",
            Value::Function(_) => "Function",
            Value::VmFunc { .. } => "Function",
            Value::Closure(_) => "Function",
            Value::Dna(_) => "Dna",
            Value::Missing => "Missing",
            Value::Unit => "Unit",
            Value::PyObject(_) => "PyObject",
            Value::Node(_) => "Node",
            Value::Headers(_) => "Headers",
            Value::Dict(_) => "Dict",
            Value::Net(_) => "Net",
        }
    }

    /// Numeric view, for arithmetic and stats.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Rational(r) => {
                use num_traits::ToPrimitive;
                r.to_f64()
            }
            _ => None,
        }
    }
}

/// Format a float so integral values still read as floats (`2.0`, not `2`).
pub fn fmt_float(x: f64) -> String {
    if x.is_finite() && x == x.trunc() {
        format!("{:.1}", x)
    } else {
        let s = format!("{}", x);
        s
    }
}

// Manual Debug: a DataFrame handle isn't `Debug`, and we don't want `{:?}` to
// execute a query as a side effect. Scalars/arrays delegate to Display.
impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::DataFrame(_) => write!(f, "DataFrame(<lazy plan>)"),
            Value::GroupBy(g) => write!(f, "GroupBy(keys={:?})", g.keys),
            Value::Function(g) => write!(f, "Function(params={:?})", g.params),
            Value::VmFunc { arity, .. } => write!(f, "Function(arity={})", arity),
            Value::Closure(c) => write!(f, "Function(arity={})", c.arity),
            Value::Tensor(t) => write!(f, "Tensor(shape={:?})", t.shape()),
            Value::PyObject(h) => write!(f, "PyObject({})", h.repr()),
            other => write!(f, "{}", other),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(x) => write!(f, "{}", fmt_float(*x)),
            Value::Rational(r) => {
                if r.is_integer() {
                    write!(f, "{}", r.numer())
                } else {
                    write!(f, "{}/{}", r.numer(), r.denom())
                }
            }
            Value::Str(s) => write!(f, "{}", s),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Dna(s) => write!(f, "{}", s),
            Value::Tensor(t) => write!(f, "{}", t),
            // Printing is the materialization point: execute the lazy plan. The
            // text is the FROZEN Helix format (framefmt, ADR 0033 Stage 0), never
            // an engine's own Display.
            Value::DataFrame(df) => write!(f, "{}", crate::framefmt::frame_text_lossy(&***df)),
            Value::GroupBy(g) => write!(f, "<grouped by {}>", g.keys.join(", ")),
            Value::Function(g) => write!(f, "<function/{}>", g.params.len()),
            Value::VmFunc { arity, .. } => write!(f, "<function/{}>", arity),
            Value::Closure(c) => write!(f, "<function/{}>", c.arity),
            Value::Missing => write!(f, "missing"),
            Value::Unit => write!(f, "()"),
            Value::Net(h) => match &**h {
                crate::serve::NetHandle::Listener(_) => write!(f, "<listener>"),
                crate::serve::NetHandle::Conn { .. } => write!(f, "<connection>"),
                crate::serve::NetHandle::HttpStream { .. } => write!(f, "<http-stream>"),
                crate::serve::NetHandle::CookieJar(j) => write!(f, "<cookie-jar: {} cookies>", j.len()),
            },
            Value::PyObject(h) => write!(f, "{}", h.repr()),
            // A tracked value prints as its forward value (the graph stays hidden).
            Value::Node(n) => write!(f, "{}", crate::autodiff::node_value(n)),
            Value::Array(items) => {
                write!(f, "[")?;
                for (i, v) in items.to_values().iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    // Strings inside arrays are quoted for clarity.
                    match v {
                        Value::Str(s) => write!(f, "\"{}\"", s)?,
                        other => write!(f, "{}", other)?,
                    }
                }
                write!(f, "]")
            }
            Value::Tuple(items) => {
                write!(f, "(")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match v {
                        Value::Str(s) => write!(f, "\"{}\"", s)?,
                        other => write!(f, "{}", other)?,
                    }
                }
                // a 1-tuple prints as `(x,)` to disambiguate from grouping
                if items.len() == 1 {
                    write!(f, ",")?;
                }
                write!(f, ")")
            }
            Value::Record(fields) => {
                // CANONICAL field order (sorted by name, decided 2026-08-24):
                // `==` always ignored order and `to_json` always sorted, so the
                // printer was the one place a record's CONSTRUCTION ROUTE showed
                // — a doc example would document one code path and fail on the
                // other (field review §1.5). Sorting prints the VALUE.
                //
                // Perf shape: `as_str` takes the interner lock, so resolve each
                // name ONCE (n locks, not n·log n·2), and an already-sorted
                // record — the common case once writers adopt the canon — skips
                // the index sort entirely.
                let names: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
                let mut order: Vec<usize> = (0..fields.len()).collect();
                if !names.windows(2).all(|w| w[0] <= w[1]) {
                    order.sort_unstable_by(|&a, &b| names[a].cmp(names[b]));
                }
                write!(f, "{{")?;
                for (i, &fi) in order.iter().enumerate() {
                    let (k, v) = &fields[fi];
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match v {
                        Value::Str(s) => write!(f, "{}: \"{}\"", k, s)?,
                        other => write!(f, "{}: {}", k, other)?,
                    }
                }
                write!(f, "}}")
            }
            // `{k => v}` arrow notation (sorted by key) — visually distinct from a
            // record's bare-identifier `{field: v}`; string keys and values are quoted.
            Value::Dict(map) => {
                write!(f, "{{")?;
                let q = |v: &Value| match v {
                    Value::Str(s) => format!("\"{}\"", s),
                    other => format!("{}", other),
                };
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{} => {}", q(&k.to_value()), q(v))?;
                }
                write!(f, "}}")
            }
            // Headers print in WIRE ORDER with names as they arrived — the same
            // arrow notation as a Dict, so the two read alike, but unsorted,
            // because the order and the casing are information here.
            Value::Headers(pairs) => {
                write!(f, "{{")?;
                for (i, (k, val)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "\"{}\" => \"{}\"", k, val)?;
                }
                write!(f, "}}")
            }
        }
    }
}

/// Render a value for **user-facing output** (`print`, string interpolation) — a
/// *fallible* mirror of `Display`. A `DataFrame` materializes its lazy plan here,
/// so a failed query surfaces as a real `HelixError` (and a non-zero exit) instead
/// of being swallowed into a placeholder string by `Display`. Recurses into
/// collections so a frame nested in an Array/Tuple/Record propagates too; every
/// other (leaf) value can't fail and delegates to `Display`.
pub fn display_value(v: &Value, line: usize, col: usize) -> Result<String, HelixError> {
    match v {
        Value::DataFrame(df) => crate::framefmt::frame_text(&***df, line, col),
        Value::Array(items) => {
            let mut s = String::from("[");
            for (i, it) in items.to_values().iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&display_elem(it, line, col)?);
            }
            s.push(']');
            Ok(s)
        }
        Value::Tuple(items) => {
            let mut s = String::from("(");
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&display_elem(it, line, col)?);
            }
            // a 1-tuple prints as `(x,)` to disambiguate from grouping
            if items.len() == 1 {
                s.push(',');
            }
            s.push(')');
            Ok(s)
        }
        Value::Record(fields) => {
            // Canonical sorted field order — the same rule (and the same perf
            // shape: one interner resolve per name) `Display` applies.
            let names: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
            let mut order: Vec<usize> = (0..fields.len()).collect();
            if !names.windows(2).all(|w| w[0] <= w[1]) {
                order.sort_unstable_by(|&a, &b| names[a].cmp(names[b]));
            }
            let mut s = String::from("{");
            for (i, &fi) in order.iter().enumerate() {
                let (k, val) = &fields[fi];
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(k.as_str());
                s.push_str(": ");
                s.push_str(&display_elem(val, line, col)?);
            }
            s.push('}');
            Ok(s)
        }
        // Scalars and other leaf values can't fail to render.
        other => Ok(other.to_string()),
    }
}

/// Append `v`'s display form straight into `buf` — the allocation-free counterpart
/// to [`display_value`] for the string-interpolation hot path (building many rows
/// like `"{id},{name},{score}\n"`). A scalar's `Display` writes directly into the
/// buffer with no throwaway `String`; collections and frames defer to
/// `display_value` (their rendering is itself fallible and less hot). The bytes are
/// identical to `buf.push_str(&display_value(v, …)?)`.
/// A type name with its indefinite article — "an Int", "a Float". Error text is read far
/// more often than it is written, and "a Int" is the kind of thing that makes a language
/// feel unfinished. Vowel-initial names are the common ones (`Int`, `Array`), so this is
/// not a rare case.
pub fn with_article(type_name: &str) -> String {
    // The tape's node is an implementation name a user never wrote — say what
    // it IS (dx-plan: route user-facing mentions through "a tracked value").
    if type_name == "Node" {
        return "a tracked value".to_string();
    }
    // The rule is about SOUND, not spelling. `U` is excluded deliberately: the only
    // U-initial type name is `Unit`, pronounced with a consonant glide — "a Unit", the
    // same as "a user". Every other vowel-initial name here (`Int`, `Array`) takes "an".
    let first = type_name.chars().next().unwrap_or(' ').to_ascii_uppercase();
    let article = if matches!(first, 'A' | 'E' | 'I' | 'O') { "an" } else { "a" };
    format!("{article} {type_name}")
}

pub fn write_value(buf: &mut String, v: &Value, line: usize, col: usize) -> Result<(), HelixError> {
    use std::fmt::Write as _;
    match v {
        Value::Array(_) | Value::Tuple(_) | Value::Record(_) | Value::DataFrame(_) => {
            buf.push_str(&display_value(v, line, col)?);
        }
        // The shapes that dominate interpolation, appended DIRECTLY. `write!(buf, "{}",
        // value)` costs two nested `fmt::Arguments` dispatches — one to reach
        // `Display for Value`, which then does `write!(f, "{}", inner)` for the scalar —
        // plus the `Formatter`'s width/fill/precision checks on each. These five arms
        // produce byte-for-byte what those `write!`s produce (see `Display for Value`),
        // so this is the same output on a shorter road.
        Value::Str(s) | Value::Dna(s) => buf.push_str(s),
        Value::Int(i) => push_i64(buf, *i),
        Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
        Value::Missing => buf.push_str("missing"),
        // Writing a scalar's `Display` into a `String` is infallible.
        other => {
            let _ = write!(buf, "{}", other);
        }
    }
    Ok(())
}

/// `"00", "01", … "99"` laid end to end — built by the compiler rather than typed out,
/// so it cannot be mistyped. Indexing it at `2 * n` gives the two digits of `n < 100`.
const DIGIT_PAIRS: [u8; 200] = {
    let mut t = [0u8; 200];
    let mut n = 0;
    while n < 100 {
        t[n * 2] = b'0' + (n / 10) as u8;
        t[n * 2 + 1] = b'0' + (n % 10) as u8;
        n += 1;
    }
    t
};

/// Append an `i64` in decimal — exactly the bytes `write!("{}", i)` writes.
/// `unsigned_abs` is what makes `i64::MIN` work: negating it would overflow, so the
/// sign is emitted first and the magnitude is accumulated as `u64`.
fn push_i64(buf: &mut String, i: i64) {
    if i < 0 {
        buf.push('-');
    }
    let mut m = i.unsigned_abs();
    // Digits fall out least-significant first, so they are staged in a fixed buffer
    // (a u64 is at most 20 digits) and copied out in one `push_str`. Two at a time:
    // a 19-digit value takes 10 divisions instead of 19, which is why this beats the
    // formatter on long integers as well as short ones.
    let mut digits = [0u8; 20];
    let mut n = digits.len();
    while m >= 100 {
        let p = (m % 100) as usize * 2;
        m /= 100;
        n -= 2;
        digits[n] = DIGIT_PAIRS[p];
        digits[n + 1] = DIGIT_PAIRS[p + 1];
    }
    if m < 10 {
        n -= 1;
        digits[n] = b'0' + m as u8;
    } else {
        let p = m as usize * 2;
        n -= 2;
        digits[n] = DIGIT_PAIRS[p];
        digits[n + 1] = DIGIT_PAIRS[p + 1];
    }
    match std::str::from_utf8(&digits[n..]) {
        Ok(s) => buf.push_str(s),
        // Unreachable — every byte staged above is `b'0'..=b'9'` — but the formatter
        // is right here and total, so there is no reason to make this a panic.
        Err(_) => {
            use std::fmt::Write as _;
            let _ = write!(buf, "{}", i);
        }
    }
}

/// Element rendering inside a collection: strings are quoted (matching `Display`),
/// everything else goes through `display_value` so nested frames stay fallible.
fn display_elem(v: &Value, line: usize, col: usize) -> Result<String, HelixError> {
    match v {
        Value::Str(s) => Ok(format!("\"{}\"", s)),
        other => display_value(other, line, col),
    }
}

/// A Dict's entries as record fields, for `{...dict, k: v}`.
///
/// A record field is a NAME, so only string keys can cross; an `Int`/`Bool`/`Dna` key
/// has no spelling as a field and is refused rather than skipped, because dropping part
/// of a payload silently is worse than not building it. Shared by the tree-walker and
/// the VM so the two cannot disagree about what a spread produced.
pub fn dict_as_record_fields(
    map: &std::collections::BTreeMap<DictKey, Value>,
) -> Result<Vec<(crate::symbol::Symbol, Value)>, String> {
    let mut out = Vec::with_capacity(map.len());
    for (k, val) in map {
        match k {
            DictKey::Str(s) => out.push((crate::symbol::Symbol::intern(s), val.clone())),
            other => {
                let shown = match other {
                    DictKey::Int(i) => i.to_string(),
                    DictKey::Bool(b) => b.to_string(),
                    DictKey::Dna(d) => d.to_string(),
                    DictKey::Str(_) => unreachable!("string keys handled above"),
                };
                return Err(format!(
                    "a record field must be a name, but this dict has the key `{shown}`"
                ));
            }
        }
    }
    Ok(out)
}

/// Is this a well-formed HTTP header field, safe to put on the wire?
///
/// `Err(message)` names what is wrong. Two separate rules, for two separate reasons:
///
/// * A NAME must be an RFC 9110 `token` — letters, digits, and ``!#$%&'*+-.^_`|~``.
///   A name with a space or a colon in it is not a header name, and a message built
///   from one is malformed in a way different intermediaries resolve differently,
///   which is the ground request smuggling grows in.
/// * A VALUE must not contain CR, LF, or NUL. This is HEADER INJECTION: the bytes
///   after a newline are read as another header, or as the start of a body, so a
///   caller-supplied string reaching a header without this check hands that caller
///   the rest of the message. Tab is allowed (it is legal whitespace in a value);
///   every other C0 control and DEL is not.
///
/// Shared by the client (`http_request`) and the server (`respond`) so neither can be
/// hardened while the other is not — a program is usually both.
pub fn validate_header(name: &str, value: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a header name cannot be empty".to_string());
    }
    // RFC 9110 §5.6.2 tchar.
    if let Some(bad) = name.chars().find(|c| {
        !(c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(*c))
    }) {
        let what = if bad == '\r' || bad == '\n' {
            format!("a newline in the header name `{}`", name.escape_debug())
        } else {
            format!("the character `{}` in the header name `{}`", bad.escape_debug(), name.escape_debug())
        };
        return Err(format!(
            "{what} — a header name may only contain letters, digits and !#$%&'*+-.^_`|~"
        ));
    }
    if let Some(bad) = value.chars().find(|c| {
        *c == '\r' || *c == '\n' || *c == '\0' || (c.is_control() && *c != '\t')
    }) {
        let what = match bad {
            '\r' | '\n' => "a newline",
            '\0' => "a NUL",
            _ => "a control character",
        };
        return Err(format!(
            "{what} in the value of header `{}` — this would inject additional headers \
             into the message. Strip or percent-encode the value before setting it.",
            name.escape_debug()
        ));
    }
    Ok(())
}

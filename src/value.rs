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
    /// Homogeneous `Int` (e.g. `range(...)`, an all-int literal, an int column).
    Ints(Vec<i64>),
    /// Homogeneous `Float`.
    Floats(Vec<f64>),
}

impl ArrayData {
    pub fn len(&self) -> usize {
        match self {
            ArrayData::Values(v) => v.len(),
            ArrayData::Ints(v) => v.len(),
            ArrayData::Floats(v) => v.len(),
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
        }
    }

}

/// An incremental, type-adaptive array builder. It accumulates packed `Int`/`Float`
/// while the pushed elements stay homogeneous, and promotes to a general
/// `Vec<Value>` on the first non-matching element. `finish()` yields the same array
/// as collecting a `Vec<Value>` and calling [`Value::array_sniff`] — but a numeric
/// comprehension result (`xs.map(...)`) never materializes the intermediate boxed
/// `Value`s, halving the transient memory of building a numeric column.
#[derive(Default)]
pub enum ColumnBuilder {
    #[default]
    Empty,
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Values(Vec<Value>),
}

impl ColumnBuilder {
    pub fn push(&mut self, v: Value) {
        match (std::mem::take(self), v) {
            (ColumnBuilder::Empty, Value::Int(i)) => *self = ColumnBuilder::Ints(vec![i]),
            (ColumnBuilder::Empty, Value::Float(f)) => *self = ColumnBuilder::Floats(vec![f]),
            (ColumnBuilder::Empty, other) => *self = ColumnBuilder::Values(vec![other]),
            (ColumnBuilder::Ints(mut v), Value::Int(i)) => {
                v.push(i);
                *self = ColumnBuilder::Ints(v);
            }
            (ColumnBuilder::Floats(mut v), Value::Float(f)) => {
                v.push(f);
                *self = ColumnBuilder::Floats(v);
            }
            (ColumnBuilder::Values(mut v), other) => {
                v.push(other);
                *self = ColumnBuilder::Values(v);
            }
            // Homogeneity broken: promote the packed buffer to boxed `Value`s.
            (ColumnBuilder::Ints(v), other) => {
                let mut vals: Vec<Value> = v.into_iter().map(Value::Int).collect();
                vals.push(other);
                *self = ColumnBuilder::Values(vals);
            }
            (ColumnBuilder::Floats(v), other) => {
                let mut vals: Vec<Value> = v.into_iter().map(Value::Float).collect();
                vals.push(other);
                *self = ColumnBuilder::Values(vals);
            }
        }
    }

    pub fn finish(self) -> Value {
        match self {
            ColumnBuilder::Empty => Value::array(Vec::new()),
            ColumnBuilder::Ints(v) => Value::int_array(v),
            ColumnBuilder::Floats(v) => Value::float_array(v),
            ColumnBuilder::Values(v) => Value::array(v),
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
            // Printing is the materialization point: execute the lazy plan.
            Value::DataFrame(df) => match df.collect_string() {
                Ok(s) => write!(f, "{}", s),
                Err(e) => write!(f, "<dataframe — query failed: {}>", e),
            },
            Value::GroupBy(g) => write!(f, "<grouped by {}>", g.keys.join(", ")),
            Value::Function(g) => write!(f, "<function/{}>", g.params.len()),
            Value::VmFunc { arity, .. } => write!(f, "<function/{}>", arity),
            Value::Closure(c) => write!(f, "<function/{}>", c.arity),
            Value::Missing => write!(f, "missing"),
            Value::Unit => write!(f, "()"),
            Value::PyObject(h) => write!(f, "{}", h.repr()),
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
                write!(f, "{{")?;
                for (i, (k, v)) in fields.iter().enumerate() {
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
        Value::DataFrame(df) => df.collect_string().map_err(|e| {
            HelixError::new(format!("could not render the DataFrame: {}", e), line, col)
        }),
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
            let mut s = String::from("{");
            for (i, (k, val)) in fields.iter().enumerate() {
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
pub fn write_value(buf: &mut String, v: &Value, line: usize, col: usize) -> Result<(), HelixError> {
    use std::fmt::Write as _;
    match v {
        Value::Array(_) | Value::Tuple(_) | Value::Record(_) | Value::DataFrame(_) => {
            buf.push_str(&display_value(v, line, col)?);
        }
        // Writing a scalar's `Display` into a `String` is infallible.
        other => {
            let _ = write!(buf, "{}", other);
        }
    }
    Ok(())
}

/// Element rendering inside a collection: strings are quoted (matching `Display`),
/// everything else goes through `display_value` so nested frames stay fallible.
fn display_elem(v: &Value, line: usize, col: usize) -> Result<String, HelixError> {
    match v {
        Value::Str(s) => Ok(format!("\"{}\"", s)),
        other => display_value(other, line, col),
    }
}

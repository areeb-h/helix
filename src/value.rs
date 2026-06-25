//! Runtime values.
//!
//! Collections are wrapped in `Rc` so that binding, passing, and returning are
//! cheap O(1) clones rather than deep copies. Because everything is immutable,
//! this sharing is always safe — a first taste of the "zero-copy where possible"
//! principle (Arrow-backed columns come in a later phase).

use std::fmt;
use std::rc::Rc;

use ndarray::ArrayD;

use crate::ast::Expr;
use crate::backend::Df;
use crate::error::HelixError;

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(Rc<String>),
    Bool(bool),
    Array(Rc<Vec<Value>>),
    /// A fixed-size, heterogeneous tuple: `(1, "a", true)`.
    Tuple(Rc<Vec<Value>>),
    /// An ordered record with identifier keys: `{name: "Ada", age: 41}`.
    Record(Rc<Vec<(String, Value)>>),
    /// A dense n-dimensional `f64` tensor (ndarray-backed). See ADR 0007.
    Tensor(Rc<ArrayD<f64>>),
    /// A columnar DataFrame, held behind the engine-agnostic backend seam (ADR
    /// 0012) as a **lazy** query plan. Verbs (`where`/`select`/`sort`/`group`)
    /// extend the plan; it materializes only at `print`/`count` — so a chain fuses
    /// into one multi-threaded pass and (with the default Polars backend's
    /// streaming engine) can run over data far larger than RAM.
    DataFrame(Df),
    /// The intermediate produced by `df.group(keys)`, consumed by an aggregation
    /// like `.mean(col)`. Boxed behind an `Rc` because it's a rare, transient value
    /// (never stored in arrays or hot loops); inlining its two-word payload would
    /// bloat *every* `Value` — and thus every VM stack push/pop/clone — for nothing.
    GroupBy(Rc<GroupByData>),
    /// A function value — from `fn name(p) = expr` or an anonymous `p => expr`.
    /// Used by the tree-walker (which evaluates `body` directly).
    Function {
        params: Rc<Vec<String>>,
        body: Rc<Expr>,
    },
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

/// The payload of a [`Value::GroupBy`] — the grouped frame plus its key columns,
/// held behind an `Rc` so the `Value` variant is one word wide.
pub struct GroupByData {
    pub handle: Df,
    pub keys: Rc<Vec<String>>,
}

// The interpreter copies `Value`s constantly (every VM stack op, every binding),
// so its size is a hot-path constant. Keep it small: scalars and Rc-wrapped
// collections fit in two words. A regression here (e.g. inlining a fat variant)
// would silently tax every clone.
const _: () = assert!(
    std::mem::size_of::<Value>() <= 24,
    "Value grew past 3 words — box the offending variant",
);

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::Str(_) => "String",
            Value::Bool(_) => "Bool",
            Value::Array(_) => "Array",
            Value::Tuple(_) => "Tuple",
            Value::Record(_) => "Record",
            Value::Tensor(_) => "Tensor",
            Value::DataFrame(_) => "DataFrame",
            Value::GroupBy(_) => "GroupBy",
            Value::Function { .. } => "Function",
            Value::VmFunc { .. } => "Function",
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
            Value::Function { params, .. } => write!(f, "Function(params={:?})", params),
            Value::VmFunc { arity, .. } => write!(f, "Function(arity={})", arity),
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
            Value::Function { params, .. } => write!(f, "<function/{}>", params.len()),
            Value::VmFunc { arity, .. } => write!(f, "<function/{}>", arity),
            Value::Missing => write!(f, "missing"),
            Value::Unit => write!(f, "()"),
            Value::PyObject(h) => write!(f, "{}", h.repr()),
            Value::Array(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
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
            for (i, it) in items.iter().enumerate() {
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
                s.push_str(k);
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

/// Element rendering inside a collection: strings are quoted (matching `Display`),
/// everything else goes through `display_value` so nested frames stay fallible.
fn display_elem(v: &Value, line: usize, col: usize) -> Result<String, HelixError> {
    match v {
        Value::Str(s) => Ok(format!("\"{}\"", s)),
        other => display_value(other, line, col),
    }
}

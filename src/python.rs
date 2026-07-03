//! CPython interop — the Helix → Python bridge (Phase 6, v1).
//!
//! `import python.math as m` / `let m = python.import("math")` embed CPython and
//! return an opaque handle (`Value::PyObject`). Method calls and attribute access
//! on a handle are forwarded to Python; results convert back **opaque-by-default**:
//! immutable scalars (int/float/bool/str/None) become native Helix values, but
//! lists/dicts/objects stay opaque `PyObject`s so identity and mutability survive
//! the boundary (the documented PyCall→PythonCall lesson). `to_array(pyobj)`
//! materializes a native copy on demand.
//!
//! All pyo3 contact lives in the `imp` submodule behind `#[cfg(feature = "python")]`.
//! `PyHandle` itself is always compiled — `value.rs`, the engines, and the `python`
//! global seed reference it unconditionally — but without the feature every bridge
//! call returns a friendly "rebuild with --features python" error, so the default
//! Helix binary never links libpython and stays self-contained.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

/// An opaque handle to a Python value, or the `python` namespace entry point.
///
/// Cloning the enclosing `Rc` shares one strong CPython reference; dropping the
/// last `Rc` drops the inner `Py<PyAny>`, which decrefs under the GIL (pyo3 handles
/// the attach). Helix's `Rc` is deterministic, so there is no PythonCall-style
/// tracing-GC leak where large Python objects linger behind small wrappers.
pub struct PyHandle {
    kind: Kind,
}

#[cfg(feature = "python")]
enum Kind {
    /// The `python` entry point: `python.import("...")`.
    Namespace,
    /// A live Python object (module, function, or value).
    Object(pyo3::Py<pyo3::PyAny>),
}

#[cfg(not(feature = "python"))]
enum Kind {
    /// The `python` entry point (the only handle that exists without the feature).
    Namespace,
}

impl PyHandle {
    /// The `python` global's value — a namespace marker. No interpreter needed.
    pub fn namespace() -> Self {
        PyHandle { kind: Kind::Namespace }
    }

    /// A short label for `Display`/`Debug` (never errors; no GIL for the namespace).
    pub fn repr(&self) -> String {
        match &self.kind {
            Kind::Namespace => "<python>".to_string(),
            #[cfg(feature = "python")]
            Kind::Object(obj) => imp::object_repr(obj),
        }
    }
}

/// `pyobj.method(args)` (and `python.import(...)`). Forwards to Python.
pub fn method(
    h: &Rc<PyHandle>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    #[cfg(feature = "python")]
    {
        imp::method(h, name, args, line, col)
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = (h, name, args);
        Err(unsupported(line, col))
    }
}

/// `pyobj.attr` (no parens). Forwards to Python `getattr`.
pub fn getattr(h: &Rc<PyHandle>, name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    #[cfg(feature = "python")]
    {
        imp::getattr(h, name, line, col)
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = (h, name);
        Err(unsupported(line, col))
    }
}

/// `to_array(pyobj)` — explicit, on-demand materialization of a Python iterable
/// into a native Helix `Array` (the visible escape hatch from opaque-by-default).
pub fn to_array(arg: Value, line: usize, col: usize) -> Result<Value, HelixError> {
    #[cfg(feature = "python")]
    {
        imp::to_array(arg, line, col)
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = arg;
        Err(unsupported(line, col))
    }
}

/// `to_dataframe(pyobj)` — bring a Python `polars.DataFrame` (or anything
/// pyo3-polars can read) into Helix as a native `DataFrame`, zero-copy via Arrow.
pub fn to_dataframe(arg: Value, line: usize, col: usize) -> Result<Value, HelixError> {
    #[cfg(feature = "python")]
    {
        imp::to_dataframe(arg, line, col)
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = arg;
        Err(unsupported(line, col))
    }
}

/// `to_tensor(pyobj)` — bring a Python NumPy `f64` array into Helix as a native
/// `Tensor` (an independent copy; Helix tensors are immutable).
pub fn to_tensor(arg: Value, line: usize, col: usize) -> Result<Value, HelixError> {
    #[cfg(feature = "python")]
    {
        imp::to_tensor(arg, line, col)
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = arg;
        Err(unsupported(line, col))
    }
}

/// `pyobj[key]` — forward to Python's `__getitem__` (numpy `a[i]`, dict `d["k"]`, …).
pub fn index(h: &Rc<PyHandle>, idx: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    #[cfg(feature = "python")]
    {
        imp::index(h, idx, line, col)
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = (h, idx);
        Err(unsupported(line, col))
    }
}

/// `pyobj[a:b:step]` — forward to Python `__getitem__` with a real `slice` object,
/// so numpy/list slicing (including open bounds and negative steps) works natively.
pub fn slice(
    h: &Rc<PyHandle>,
    start: Option<i64>,
    stop: Option<i64>,
    step: i64,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    #[cfg(feature = "python")]
    {
        imp::slice(h, start, stop, step, line, col)
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = (h, start, stop, step);
        Err(unsupported(line, col))
    }
}

/// A binary operator with at least one Python operand — forwards to Python's operator
/// protocol so `a + b`, `a * 2`, `a < b`, … work directly on Python objects.
pub fn binary(
    op: &crate::ast::BinOp,
    l: &Value,
    r: &Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    #[cfg(feature = "python")]
    {
        imp::binary(op, l, r, line, col)
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = (op, l, r);
        Err(unsupported(line, col))
    }
}

/// The error every bridge call returns when Helix was built without the feature.
#[cfg(not(feature = "python"))]
fn unsupported(line: usize, col: usize) -> HelixError {
    HelixError::new("Helix was built without Python support", line, col)
        .hint("rebuild with `cargo build --features python` (or `cargo run --features python`).")
}

#[cfg(feature = "python")]
mod imp {
    use super::{Kind, PyHandle, Value};
    use crate::ast::BinOp;
    use crate::error::HelixError;
    use numpy::ToPyArray;
    use pyo3::prelude::*;
    use pyo3::types::{PyBool, PyDict, PyFloat, PyList, PyString, PyTuple};
    use std::ffi::CString;
    use std::rc::Rc;
    use std::sync::OnceLock;

    /// A persistent global namespace for `python.exec` / `python.eval`, so
    /// `exec("import numpy as np")` then `eval("np.array(...)")` see each other.
    /// `Py<PyDict>` is GIL-independent, so it can live in a process-wide cell.
    fn globals(py: Python<'_>) -> Bound<'_, PyDict> {
        static GLOBALS: OnceLock<Py<PyDict>> = OnceLock::new();
        GLOBALS
            .get_or_init(|| Python::attach(|p| PyDict::new(p).unbind()))
            .bind(py)
            .clone()
    }

    /// `pyobj[key]` → Python `__getitem__`.
    pub fn index(
        h: &Rc<PyHandle>,
        idx: &Value,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match &h.kind {
            Kind::Object(obj) => Python::attach(|py| {
                let key = to_py(py, idx, line, col)?;
                let item = obj.bind(py).get_item(key).map_err(|e| py_err(py, e, line, col))?;
                from_py(py, &item, line, col)
            }),
            Kind::Namespace => Err(HelixError::new("can't index the `python` namespace", line, col)),
        }
    }

    /// `pyobj[a:b:step]` → Python `__getitem__(slice(a, b, step))`. Open bounds map
    /// to Python `None`, so numpy/list slice semantics (negatives, steps) are native.
    pub fn slice(
        h: &Rc<PyHandle>,
        start: Option<i64>,
        stop: Option<i64>,
        step: i64,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        let obj = match &h.kind {
            Kind::Object(o) => o,
            Kind::Namespace => return Err(HelixError::new("can't slice the `python` namespace", line, col)),
        };
        Python::attach(|py| {
            let bound_int = |o: Option<i64>| match o {
                Some(v) => v.into_pyobject(py).unwrap().into_any(),
                None => py.None().into_bound(py),
            };
            let slice_ty = py
                .import("builtins")
                .and_then(|b| b.getattr("slice"))
                .map_err(|e| py_err(py, e, line, col))?;
            let args = PyTuple::new(
                py,
                [bound_int(start), bound_int(stop), step.into_pyobject(py).unwrap().into_any()],
            )
            .map_err(|e| py_err(py, e, line, col))?;
            let sl = slice_ty.call1(&args).map_err(|e| py_err(py, e, line, col))?;
            let item = obj.bind(py).get_item(sl).map_err(|e| py_err(py, e, line, col))?;
            from_py(py, &item, line, col)
        })
    }

    /// A binary operator with a Python operand — dispatched through the `operator`
    /// module so every object's `__add__`/`__lt__`/… protocol is honored.
    pub fn binary(
        op: &BinOp,
        l: &Value,
        r: &Value,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        let fname = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "truediv",
            BinOp::FloorDiv => "floordiv",
            BinOp::Mod => "mod",
            BinOp::Pow => "pow",
            BinOp::Lt => "lt",
            BinOp::Gt => "gt",
            BinOp::Le => "le",
            BinOp::Ge => "ge",
            BinOp::Eq => "eq",
            BinOp::Ne => "ne",
            _ => {
                return Err(HelixError::new(
                    format!("operator `{}` is not supported on a Python object", op.symbol()),
                    line,
                    col,
                ))
            }
        };
        Python::attach(|py| {
            let opmod = py.import("operator").map_err(|e| py_err(py, e, line, col))?;
            let f = opmod.getattr(fname).map_err(|e| py_err(py, e, line, col))?;
            let pl = to_py(py, l, line, col)?;
            let pr = to_py(py, r, line, col)?;
            let res = f.call1((pl, pr)).map_err(|e| py_err(py, e, line, col))?;
            from_py(py, &res, line, col)
        })
    }

    /// Build an `Object` handle from a live Python value.
    fn object(obj: Py<PyAny>) -> Rc<PyHandle> {
        Rc::new(PyHandle { kind: Kind::Object(obj) })
    }

    /// Display of a Python handle: its actual `str(obj)` so printing a numpy array or
    /// dict shows the *value*, not an opaque tag (Python already elides huge reprs).
    /// Falls back to `<python TYPE>` only if `str()` itself raises.
    pub fn object_repr(obj: &Py<PyAny>) -> String {
        // Cap the rendered string so `print(huge_python_object)` can't build a
        // multi-gigabyte display string (numpy already elides, but a plain list/str
        // does not). 10k chars is far more than any readable line.
        const MAX_REPR: usize = 10_000;
        Python::attach(|py| {
            let bound = obj.bind(py);
            if let Ok(s) = bound.str() {
                let s = s.to_string();
                if s.len() > MAX_REPR {
                    let mut end = MAX_REPR;
                    while !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    return format!("{}… (+{} more chars)", &s[..end], s.len() - end);
                }
                return s;
            }
            let ty = bound.get_type().name().map(|n| n.to_string()).unwrap_or_else(|_| "object".to_string());
            format!("<python {ty}>")
        })
    }

    pub fn method(
        h: &Rc<PyHandle>,
        name: &str,
        args: &[Value],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match &h.kind {
            Kind::Namespace => namespace_method(name, args, line, col),
            Kind::Object(obj) => Python::attach(|py| {
                let bound = obj.bind(py);
                let attr = bound.getattr(name).map_err(|e| py_err(py, e, line, col))?;
                let pyargs = args_to_py(py, args, line, col)?;
                let tuple = PyTuple::new(py, pyargs).map_err(|e| py_err(py, e, line, col))?;
                let res = attr.call1(&tuple).map_err(|e| py_err(py, e, line, col))?;
                from_py(py, &res, line, col)
            }),
        }
    }

    /// `python.import("name")` (the only method on the namespace for now).
    fn namespace_method(
        name: &str,
        args: &[Value],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match name {
            "import" => {
                let module = expect_one_string(args, "python.import", line, col)?;
                Python::attach(|py| {
                    let m = py
                        .import(module.as_str())
                        .map_err(|e| py_err(py, e, line, col))?;
                    Ok(Value::PyObject(object(m.into_any().unbind())))
                })
            }
            // Run statements in the persistent namespace (`import numpy as np`, defs…).
            "exec" => {
                let code = expect_one_string(args, "python.exec", line, col)?;
                let cstr = CString::new(code).map_err(|_| {
                    HelixError::new("python code contains an interior NUL byte", line, col)
                })?;
                Python::attach(|py| {
                    let g = globals(py);
                    py.run(cstr.as_c_str(), Some(&g), None).map_err(|e| py_err(py, e, line, col))?;
                    Ok(Value::Unit)
                })
            }
            // Evaluate an expression in the persistent namespace and convert the result.
            // The full-syntax escape hatch: kwargs, slices, comprehensions all work here.
            "eval" => {
                let code = expect_one_string(args, "python.eval", line, col)?;
                let cstr = CString::new(code).map_err(|_| {
                    HelixError::new("python code contains an interior NUL byte", line, col)
                })?;
                Python::attach(|py| {
                    let g = globals(py);
                    let res = py
                        .eval(cstr.as_c_str(), Some(&g), None)
                        .map_err(|e| py_err(py, e, line, col))?;
                    from_py(py, &res, line, col)
                })
            }
            other => Err(HelixError::new(
                format!("`python` has no method `{other}`"),
                line,
                col,
            )
            .hint("use `python.import(\"name\")`, `python.exec(\"...\")`, or `python.eval(\"...\")`.")),
        }
    }

    pub fn getattr(
        h: &Rc<PyHandle>,
        name: &str,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match &h.kind {
            Kind::Object(obj) => Python::attach(|py| {
                let bound = obj.bind(py);
                let attr = bound.getattr(name).map_err(|e| py_err(py, e, line, col))?;
                from_py(py, &attr, line, col)
            }),
            Kind::Namespace => Err(HelixError::new(
                format!("`python` has no field `{name}`"),
                line,
                col,
            )
            .hint("did you mean `python.import(\"...\")`?")),
        }
    }

    pub fn to_array(arg: Value, line: usize, col: usize) -> Result<Value, HelixError> {
        match arg {
            Value::Array(_) => Ok(arg), // already native
            Value::PyObject(h) => match &h.kind {
                Kind::Object(obj) => Python::attach(|py| {
                    let bound = obj.bind(py);
                    let iter = bound.try_iter().map_err(|e| py_err(py, e, line, col))?;
                    let mut out = Vec::new();
                    for item in iter {
                        // Cap the pull so an unbounded Python iterable (a generator,
                        // `itertools.count()`, …) fails cleanly instead of OOMing/hanging.
                        // Matches the runtime's `MAX_ELEMENTS` collection cap.
                        const MAX_PULL: usize = 100_000_000;
                        if out.len() >= MAX_PULL {
                            return Err(HelixError::new(
                                format!(
                                    "`to_array` stopped at {MAX_PULL} elements — is the Python iterable unbounded?"
                                ),
                                line,
                                col,
                            )
                            .hint("slice or take a finite prefix on the Python side first."));
                        }
                        let item = item.map_err(|e| py_err(py, e, line, col))?;
                        out.push(from_py(py, &item, line, col)?);
                    }
                    Ok(Value::array(out))
                }),
                Kind::Namespace => Err(HelixError::new(
                    "`to_array` cannot convert the `python` namespace",
                    line,
                    col,
                )),
            },
            other => Err(HelixError::new(
                format!("`to_array` expects a Python object or array, got {}", other.type_name()),
                line,
                col,
            )),
        }
    }

    pub fn to_dataframe(arg: Value, line: usize, col: usize) -> Result<Value, HelixError> {
        match arg {
            Value::DataFrame(_) => Ok(arg), // already native
            Value::PyObject(h) => match &h.kind {
                Kind::Object(obj) => Python::attach(|py| {
                    let pydf: pyo3_polars::PyDataFrame =
                        obj.bind(py).extract().map_err(|e| py_err(py, e, line, col))?;
                    Ok(Value::dataframe(crate::backend::polars::from_polars_df(pydf.0)))
                }),
                Kind::Namespace => Err(HelixError::new(
                    "`to_dataframe` cannot convert the `python` namespace",
                    line,
                    col,
                )),
            },
            other => Err(HelixError::new(
                format!(
                    "`to_dataframe` expects a Python DataFrame or a DataFrame, got {}",
                    other.type_name()
                ),
                line,
                col,
            )),
        }
    }

    pub fn to_tensor(arg: Value, line: usize, col: usize) -> Result<Value, HelixError> {
        match arg {
            Value::Tensor(_) => Ok(arg), // already native
            Value::PyObject(h) => match &h.kind {
                Kind::Object(obj) => Python::attach(|py| {
                    // Require an f64 NumPy array (Helix tensors are f64). rust-numpy's
                    // extract error isn't a PyErr, so map it straight to a HelixError.
                    let arr: numpy::PyReadonlyArrayDyn<f64> =
                        obj.bind(py).extract().map_err(|e| {
                            HelixError::new(
                                format!("`to_tensor` expects a NumPy f64 array ({e})"),
                                line,
                                col,
                            )
                        })?;
                    Ok(Value::Tensor(Rc::new(arr.as_array().to_owned())))
                }),
                Kind::Namespace => Err(HelixError::new(
                    "`to_tensor` cannot convert the `python` namespace",
                    line,
                    col,
                )),
            },
            other => Err(HelixError::new(
                format!(
                    "`to_tensor` expects a NumPy array or a Tensor, got {}",
                    other.type_name()
                ),
                line,
                col,
            )),
        }
    }

    // --- conversions -------------------------------------------------------

    fn args_to_py<'py>(
        py: Python<'py>,
        args: &[Value],
        line: usize,
        col: usize,
    ) -> Result<Vec<Bound<'py, PyAny>>, HelixError> {
        args.iter().map(|v| to_py(py, v, line, col)).collect()
    }

    /// Helix → Python (implicit, lossless for what crosses *out*).
    fn to_py<'py>(
        py: Python<'py>,
        v: &Value,
        line: usize,
        col: usize,
    ) -> Result<Bound<'py, PyAny>, HelixError> {
        Ok(match v {
            Value::Int(i) => i
                .into_pyobject(py)
                .map_err(|e| py_err(py, e.into(), line, col))?
                .into_any(),
            Value::Float(x) => PyFloat::new(py, *x).into_any(),
            Value::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
            Value::Str(s) => PyString::new(py, s).into_any(),
            Value::Missing => py.None().into_bound(py),
            Value::Array(items) => {
                use crate::value::ArrayData;
                // Packed numeric arrays cross straight from the f64/i64 buffer — no
                // per-element `Value` boxing or recursive `to_py`.
                match &**items {
                    ArrayData::Floats(xs) => {
                        PyList::new(py, xs.iter().copied()).map_err(|e| py_err(py, e, line, col))?.into_any()
                    }
                    ArrayData::Ints(xs) => {
                        PyList::new(py, xs.iter().copied()).map_err(|e| py_err(py, e, line, col))?.into_any()
                    }
                    // A lazy range crosses as its i64 elements — identical to the `Ints` arm.
                    ArrayData::Range { .. } => PyList::new(py, items.to_ints().unwrap().iter().copied())
                        .map_err(|e| py_err(py, e, line, col))?
                        .into_any(),
                    ArrayData::Values(vs) => {
                        let mut elems = Vec::with_capacity(vs.len());
                        for it in vs.iter() {
                            elems.push(to_py(py, it, line, col)?);
                        }
                        PyList::new(py, elems).map_err(|e| py_err(py, e, line, col))?.into_any()
                    }
                }
            }
            // A Helix record becomes a Python dict (string keys) — so `json.dumps(rec)`,
            // and any positional-dict argument, work.
            Value::Record(fields) => {
                let d = PyDict::new(py);
                for (k, v) in fields.iter() {
                    d.set_item(k.as_str(), to_py(py, v, line, col)?)
                        .map_err(|e| py_err(py, e, line, col))?;
                }
                d.into_any()
            }
            Value::Tensor(t) => {
                // Copy into a NumPy array. Helix tensors are immutable and Rc-shared,
                // but NumPy arrays are mutable — so hand Python an independent copy
                // rather than aliasing a shared immutable buffer.
                (**t).to_pyarray(py).into_any()
            }
            Value::DataFrame(lf) => {
                // Materialize the lazy plan, then hand the Arrow buffers to Python
                // as a `polars.DataFrame` — zero-copy across the boundary (pyo3-polars
                // moves the Arrow data, doesn't re-serialize the values).
                let lazy = crate::backend::polars::as_lazyframe(lf, line, col)?;
                let df = lazy.collect().map_err(|e| {
                    HelixError::new(
                        format!("failed to materialize the DataFrame for Python: {e}"),
                        line,
                        col,
                    )
                })?;
                pyo3_polars::PyDataFrame(df)
                    .into_pyobject(py)
                    .map_err(|e| py_err(py, e, line, col))?
                    .into_any()
            }
            Value::PyObject(h) => match &h.kind {
                Kind::Object(o) => o.bind(py).clone().into_any(),
                Kind::Namespace => {
                    return Err(HelixError::new(
                        "can't pass the `python` namespace to Python",
                        line,
                        col,
                    ));
                }
            },
            other => {
                return Err(HelixError::new(
                    format!("can't pass a {} to Python yet", other.type_name()),
                    line,
                    col,
                ));
            }
        })
    }

    /// Python → Helix. Immutable scalars convert; everything else stays opaque.
    fn from_py(
        py: Python<'_>,
        obj: &Bound<'_, PyAny>,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        let _ = (line, col);
        if obj.is_none() {
            return Ok(Value::Missing);
        }
        // `bool` MUST be checked before `int` — Python `bool` subclasses `int`.
        if let Ok(b) = obj.cast::<PyBool>() {
            return Ok(Value::Bool(b.is_true()));
        }
        if let Ok(i) = obj.extract::<i64>() {
            return Ok(Value::Int(i));
        }
        if let Ok(f) = obj.extract::<f64>() {
            return Ok(Value::Float(f));
        }
        if let Ok(s) = obj.extract::<String>() {
            return Ok(Value::Str(Rc::new(s)));
        }
        // Containers / arbitrary objects stay opaque (preserve identity + mutability).
        let _ = py;
        Ok(Value::PyObject(object(obj.clone().unbind())))
    }

    // --- helpers -----------------------------------------------------------

    fn expect_one_string(
        args: &[Value],
        who: &str,
        line: usize,
        col: usize,
    ) -> Result<String, HelixError> {
        if args.len() != 1 {
            return Err(HelixError::new(
                format!("`{who}` takes exactly one argument, got {}", args.len()),
                line,
                col,
            ));
        }
        match &args[0] {
            Value::Str(s) => Ok(s.to_string()),
            other => Err(HelixError::new(
                format!("`{who}` expects a string, got {}", other.type_name()),
                line,
                col,
            )),
        }
    }

    /// Translate a Python exception into a Helix-native diagnostic at the call site.
    fn py_err(py: Python<'_>, e: PyErr, line: usize, col: usize) -> HelixError {
        let ty = e
            .get_type(py)
            .name()
            .map(|n| n.to_string())
            .unwrap_or_else(|_| "PythonError".to_string());
        let msg = e
            .value(py)
            .str()
            .map(|s| s.to_string())
            .unwrap_or_default();
        HelixError::new(format!("python error: {ty}: {msg}"), line, col)
    }
}

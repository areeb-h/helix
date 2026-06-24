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

/// The error every bridge call returns when Helix was built without the feature.
#[cfg(not(feature = "python"))]
fn unsupported(line: usize, col: usize) -> HelixError {
    HelixError::new("Helix was built without Python support", line, col)
        .hint("rebuild with `cargo build --features python` (or `cargo run --features python`).")
}

#[cfg(feature = "python")]
mod imp {
    use super::{Kind, PyHandle, Value};
    use crate::error::HelixError;
    use numpy::ToPyArray;
    use pyo3::prelude::*;
    use pyo3::types::{PyBool, PyFloat, PyList, PyString, PyTuple};
    use std::rc::Rc;

    /// Build an `Object` handle from a live Python value.
    fn object(obj: Py<PyAny>) -> Rc<PyHandle> {
        Rc::new(PyHandle { kind: Kind::Object(obj) })
    }

    /// Label for Display/Debug, e.g. `<python list>`, `<python module>` — makes
    /// clear the value is an opaque Python handle, not a native Helix value.
    pub fn object_repr(obj: &Py<PyAny>) -> String {
        Python::attach(|py| {
            let ty = obj
                .bind(py)
                .get_type()
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|_| "object".to_string());
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
            other => Err(HelixError::new(
                format!("`python` has no method `{other}`"),
                line,
                col,
            )
            .hint("use `python.import(\"name\")` to load a Python module.")),
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
                        let item = item.map_err(|e| py_err(py, e, line, col))?;
                        out.push(from_py(py, &item, line, col)?);
                    }
                    Ok(Value::Array(Rc::new(out)))
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
        use polars::prelude::IntoLazy;
        match arg {
            Value::DataFrame(_) => Ok(arg), // already native
            Value::PyObject(h) => match &h.kind {
                Kind::Object(obj) => Python::attach(|py| {
                    let pydf: pyo3_polars::PyDataFrame =
                        obj.bind(py).extract().map_err(|e| py_err(py, e, line, col))?;
                    Ok(Value::DataFrame(Rc::new(pydf.0.lazy())))
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
                let mut elems = Vec::with_capacity(items.len());
                for it in items.iter() {
                    elems.push(to_py(py, it, line, col)?);
                }
                PyList::new(py, elems)
                    .map_err(|e| py_err(py, e, line, col))?
                    .into_any()
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
                let df = (**lf).clone().collect().map_err(|e| {
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

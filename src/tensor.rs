//! Tensor engine — dense n-dimensional `f64` arrays backed by `ndarray`
//! (ADR 0007). The Helix surface (shape/reshape/matmul/broadcasting/reductions)
//! is backend-independent, so a GPU/autodiff backend can slot in behind it later.

use std::rc::Rc;

use ndarray::{Array2, ArrayD, Axis, Ix1, Ix2, IxDyn, Zip};

use crate::ast::BinOp;
use crate::error::{suggest, HelixError};
use crate::value::Value;

pub type Tensor = ArrayD<f64>;

// ---------- construction ----------

/// Infer the (rectangular) shape of a nested Helix value.
fn shape_of(v: &Value, line: usize, col: usize) -> Result<Vec<usize>, HelixError> {
    match v {
        Value::Int(_) | Value::Float(_) => Ok(vec![]),
        Value::Array(items) => {
            if items.is_empty() {
                return Ok(vec![0]);
            }
            let sub = shape_of(&items[0], line, col)?;
            for it in items.iter() {
                if shape_of(it, line, col)? != sub {
                    return Err(HelixError::new(
                        "tensor rows must all have the same shape (ragged array)",
                        line,
                        col,
                    )
                    .hint("every nested array at a given depth must have the same length."));
                }
            }
            let mut shape = vec![items.len()];
            shape.extend(sub);
            Ok(shape)
        }
        other => Err(HelixError::new(
            format!("cannot build a tensor from a value of type {}", other.type_name()),
            line,
            col,
        )
        .hint("tensors are built from numbers and (nested) arrays of numbers.")),
    }
}

fn flatten_into(v: &Value, out: &mut Vec<f64>, line: usize, col: usize) -> Result<(), HelixError> {
    match v {
        Value::Int(i) => out.push(*i as f64),
        Value::Float(f) => out.push(*f),
        Value::Array(items) => {
            for it in items.iter() {
                flatten_into(it, out, line, col)?;
            }
        }
        Value::Missing => {
            return Err(HelixError::new("tensors cannot contain `missing`", line, col)
                .hint("drop or impute missing values before building a tensor."))
        }
        other => {
            return Err(HelixError::new(
                format!("a tensor element must be a number, not a {}", other.type_name()),
                line,
                col,
            ))
        }
    }
    Ok(())
}

pub fn from_value(v: &Value, line: usize, col: usize) -> Result<Tensor, HelixError> {
    let shape = shape_of(v, line, col)?;
    let mut data = Vec::new();
    flatten_into(v, &mut data, line, col)?;
    ArrayD::from_shape_vec(IxDyn(&shape), data)
        .map_err(|e| HelixError::new(format!("could not build tensor: {}", e), line, col))
}

/// `t[i]` — index the first axis. On a 1-D tensor this yields a scalar `Float`;
/// on a higher-rank tensor it yields the sub-tensor (a row/plane). Negative
/// indices count from the end.
pub fn index_first(t: &Tensor, i: i64, line: usize, col: usize) -> Result<Value, HelixError> {
    if t.ndim() == 0 {
        return Err(HelixError::new(
            "cannot index a 0-D (scalar) tensor",
            line,
            col,
        ));
    }
    let n = t.shape()[0] as i64;
    let real = if i < 0 { n + i } else { i };
    if real < 0 || real >= n {
        return Err(HelixError::new(
            format!("index {} is out of bounds for a tensor axis of length {}", i, n),
            line,
            col,
        ));
    }
    let sub = t.clone().index_axis_move(Axis(0), real as usize);
    if sub.ndim() == 0 {
        Ok(Value::Float(*sub.first().unwrap()))
    } else {
        Ok(Value::Tensor(Rc::new(sub)))
    }
}

/// `t[a:b:step]` — slice the first axis using already-resolved indices.
pub fn slice_first(t: &Tensor, idxs: &[usize]) -> Value {
    Value::Tensor(Rc::new(t.select(Axis(0), idxs)))
}

pub fn zeros(shape: &[usize]) -> Tensor {
    ArrayD::zeros(IxDyn(shape))
}

pub fn ones(shape: &[usize]) -> Tensor {
    ArrayD::ones(IxDyn(shape))
}

pub fn eye(n: usize) -> Tensor {
    ndarray::Array2::<f64>::eye(n).into_dyn()
}

// ---------- broadcasting & elementwise ----------

/// NumPy broadcasting rule: align shapes from the right; each pair of dims must
/// be equal, or one of them 1 (which stretches).
pub fn broadcast_shape(a: &[usize], b: &[usize]) -> Option<Vec<usize>> {
    let n = a.len().max(b.len());
    let mut out = vec![0usize; n];
    for i in 0..n {
        let da = if i + a.len() < n { 1 } else { a[i + a.len() - n] };
        let db = if i + b.len() < n { 1 } else { b[i + b.len() - n] };
        out[i] = if da == db || db == 1 {
            da
        } else if da == 1 {
            db
        } else {
            return None;
        };
    }
    Some(out)
}

fn apply(op: &BinOp, x: f64, y: f64) -> f64 {
    match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => x / y,
        BinOp::Mod => x.rem_euclid(y),
        BinOp::Pow => x.powf(y),
        _ => x,
    }
}

pub fn elementwise(
    op: &BinOp,
    a: &Tensor,
    b: &Tensor,
    line: usize,
    col: usize,
) -> Result<Tensor, HelixError> {
    let shape = broadcast_shape(a.shape(), b.shape()).ok_or_else(|| {
        HelixError::new(
            format!(
                "cannot broadcast tensors of shape {:?} and {:?}",
                a.shape(),
                b.shape()
            ),
            line,
            col,
        )
        .hint("shapes must match, or a dimension of 1 stretches to fit (NumPy rules).")
    })?;
    let av = a.broadcast(IxDyn(&shape)).expect("broadcast verified");
    let bv = b.broadcast(IxDyn(&shape)).expect("broadcast verified");
    let mut out = ArrayD::zeros(IxDyn(&shape));
    Zip::from(&mut out)
        .and(&av)
        .and(&bv)
        .for_each(|o, &x, &y| *o = apply(op, x, y));
    Ok(out)
}

pub fn scalar_op(op: &BinOp, t: &Tensor, s: f64, tensor_left: bool) -> Tensor {
    if tensor_left {
        t.mapv(|x| apply(op, x, s))
    } else {
        t.mapv(|x| apply(op, s, x))
    }
}

// ---------- methods ----------

const TENSOR_METHODS: &[&str] = &[
    "shape", "ndim", "count", "sum", "mean", "min", "max", "flatten", "reshape", "transpose", "t",
    "matmul", "dot", "norm", "det", "inv", "solve",
];

// ---------- pure-Rust linear algebra (Gaussian elimination, no BLAS dep) ----------

/// Determinant via LU with partial pivoting. Returns 0.0 for singular matrices.
fn determinant(mut a: Array2<f64>) -> f64 {
    let n = a.nrows();
    let mut det = 1.0;
    for i in 0..n {
        let mut p = i;
        for r in (i + 1)..n {
            if a[[r, i]].abs() > a[[p, i]].abs() {
                p = r;
            }
        }
        if a[[p, i]] == 0.0 {
            return 0.0;
        }
        if p != i {
            for c in 0..n {
                a.swap([i, c], [p, c]);
            }
            det = -det;
        }
        det *= a[[i, i]];
        for r in (i + 1)..n {
            let f = a[[r, i]] / a[[i, i]];
            for c in i..n {
                let v = a[[i, c]];
                a[[r, c]] -= f * v;
            }
        }
    }
    det
}

/// Solve `A x = b` (b may have multiple columns) via Gauss–Jordan with partial
/// pivoting on the augmented matrix. Returns None if A is singular.
fn solve_system(a: &Array2<f64>, b: &Array2<f64>) -> Option<Array2<f64>> {
    let n = a.nrows();
    let m = b.ncols();
    // augmented [A | b]
    let mut aug = Array2::<f64>::zeros((n, n + m));
    for i in 0..n {
        for j in 0..n {
            aug[[i, j]] = a[[i, j]];
        }
        for j in 0..m {
            aug[[i, n + j]] = b[[i, j]];
        }
    }
    for i in 0..n {
        let mut p = i;
        for r in (i + 1)..n {
            if aug[[r, i]].abs() > aug[[p, i]].abs() {
                p = r;
            }
        }
        if aug[[p, i]].abs() < 1e-12 {
            return None; // singular
        }
        if p != i {
            for c in 0..(n + m) {
                aug.swap([i, c], [p, c]);
            }
        }
        let piv = aug[[i, i]];
        for c in 0..(n + m) {
            aug[[i, c]] /= piv;
        }
        for r in 0..n {
            if r != i {
                let f = aug[[r, i]];
                for c in 0..(n + m) {
                    let v = aug[[i, c]];
                    aug[[r, c]] -= f * v;
                }
            }
        }
    }
    let mut x = Array2::<f64>::zeros((n, m));
    for i in 0..n {
        for j in 0..m {
            x[[i, j]] = aug[[i, n + j]];
        }
    }
    Some(x)
}

fn as_2d_square(t: &Tensor, name: &str, line: usize, col: usize) -> Result<Array2<f64>, HelixError> {
    let a = t.clone().into_dimensionality::<Ix2>().map_err(|_| {
        HelixError::new(
            format!("`{}` needs a 2-D tensor, got shape {:?}", name, t.shape()),
            line,
            col,
        )
    })?;
    if a.nrows() != a.ncols() {
        return Err(HelixError::new(
            format!("`{}` needs a square matrix, got shape {:?}", name, a.shape()),
            line,
            col,
        ));
    }
    Ok(a)
}

fn as_usize_shape(v: &Value, line: usize, col: usize) -> Result<Vec<usize>, HelixError> {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|x| match x {
                Value::Int(i) if *i >= 0 => Ok(*i as usize),
                _ => Err(HelixError::new("shape entries must be non-negative integers", line, col)),
            })
            .collect(),
        _ => Err(HelixError::new("expected a shape array, e.g. `[2, 3]`", line, col)),
    }
}

/// Parse an optional axis index argument for a reduction (`sum()` vs `sum(0)`).
fn axis_arg(
    args: &[Value],
    ndim: usize,
    line: usize,
    col: usize,
) -> Result<Option<usize>, HelixError> {
    match args {
        [] => Ok(None),
        [Value::Int(k)] if *k >= 0 && (*k as usize) < ndim => Ok(Some(*k as usize)),
        [Value::Int(k)] => Err(HelixError::new(
            format!("axis {} is out of range for a {}-D tensor", k, ndim),
            line,
            col,
        )),
        _ => Err(HelixError::new(
            "expected an optional axis index, e.g. `sum(0)`",
            line,
            col,
        )),
    }
}

pub fn method(
    t: &Rc<Tensor>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let no_args = |n: &str| -> Result<(), HelixError> {
        if args.is_empty() {
            Ok(())
        } else {
            Err(HelixError::new(format!("`{}` takes no arguments", n), line, col))
        }
    };
    match name {
        "shape" => {
            no_args(name)?;
            let dims: Vec<Value> = t.shape().iter().map(|d| Value::Int(*d as i64)).collect();
            Ok(Value::Array(Rc::new(dims)))
        }
        "ndim" => {
            no_args(name)?;
            Ok(Value::Int(t.ndim() as i64))
        }
        "count" => {
            no_args(name)?;
            Ok(Value::Int(t.len() as i64))
        }
        "sum" => match axis_arg(args, t.ndim(), line, col)? {
            None => Ok(Value::Float(t.sum())),
            Some(k) => Ok(Value::Tensor(Rc::new(t.sum_axis(Axis(k))))),
        },
        "mean" => match axis_arg(args, t.ndim(), line, col)? {
            None => t.mean().map(Value::Float).ok_or_else(|| {
                HelixError::new("cannot take `mean` of an empty tensor", line, col)
            }),
            Some(k) => t
                .mean_axis(Axis(k))
                .map(|a| Value::Tensor(Rc::new(a)))
                .ok_or_else(|| HelixError::new("cannot take `mean` of an empty axis", line, col)),
        },
        "min" | "max" => {
            let is_min = name == "min";
            let init = if is_min { f64::INFINITY } else { f64::NEG_INFINITY };
            match axis_arg(args, t.ndim(), line, col)? {
                None => {
                    if t.is_empty() {
                        return Err(HelixError::new(
                            format!("cannot take `{}` of an empty tensor", name),
                            line,
                            col,
                        ));
                    }
                    let v = t.iter().fold(init, |acc, &x| if is_min { acc.min(x) } else { acc.max(x) });
                    Ok(Value::Float(v))
                }
                Some(k) => {
                    let r = t.fold_axis(Axis(k), init, |&acc, &x| {
                        if is_min { acc.min(x) } else { acc.max(x) }
                    });
                    Ok(Value::Tensor(Rc::new(r)))
                }
            }
        }
        "flatten" => {
            no_args(name)?;
            let flat = t
                .to_shape(IxDyn(&[t.len()]))
                .map_err(|e| HelixError::new(format!("could not flatten tensor: {}", e), line, col))?
                .to_owned();
            Ok(Value::Tensor(Rc::new(flat)))
        }
        "reshape" => {
            if args.len() != 1 {
                return Err(HelixError::new("`reshape` takes one shape array", line, col)
                    .hint("e.g. `t.reshape([3, 2])`."));
            }
            let shape = as_usize_shape(&args[0], line, col)?;
            let prod: usize = shape.iter().product();
            if prod != t.len() {
                return Err(HelixError::new(
                    format!(
                        "cannot reshape {} elements into shape {:?} ({} elements)",
                        t.len(),
                        shape,
                        prod
                    ),
                    line,
                    col,
                ));
            }
            let r = t
                .to_shape(IxDyn(&shape))
                .map_err(|e| HelixError::new(format!("could not reshape: {}", e), line, col))?
                .to_owned();
            Ok(Value::Tensor(Rc::new(r)))
        }
        "transpose" | "t" => {
            no_args(name)?;
            Ok(Value::Tensor(Rc::new(t.t().to_owned())))
        }
        "matmul" | "dot" => {
            if args.len() != 1 {
                return Err(HelixError::new(format!("`{}` takes one tensor", name), line, col));
            }
            let other = match &args[0] {
                Value::Tensor(o) => o,
                o => {
                    return Err(HelixError::new(
                        format!("`{}` needs a tensor, got a {}", name, o.type_name()),
                        line,
                        col,
                    ))
                }
            };
            let misaligned = |sa: &[usize], sb: &[usize]| {
                HelixError::new(
                    format!("shapes {:?} and {:?} are not aligned for `{}`", sa, sb, name),
                    line,
                    col,
                )
                .hint("vector·vector, matrix·matrix, and matrix·vector are supported.")
            };
            match (t.ndim(), other.ndim()) {
                // vector · vector -> scalar dot product
                (1, 1) => {
                    if t.len() != other.len() {
                        return Err(misaligned(t.shape(), other.shape()));
                    }
                    let s: f64 = t.iter().zip(other.iter()).map(|(&x, &y)| x * y).sum();
                    Ok(Value::Float(s))
                }
                // matrix · matrix
                (2, 2) => {
                    let a2 = (**t).clone().into_dimensionality::<Ix2>().unwrap();
                    let b2 = (**other).clone().into_dimensionality::<Ix2>().unwrap();
                    if a2.ncols() != b2.nrows() {
                        return Err(misaligned(a2.shape(), b2.shape()));
                    }
                    Ok(Value::Tensor(Rc::new(a2.dot(&b2).into_dyn())))
                }
                // matrix · vector -> vector
                (2, 1) => {
                    let a2 = (**t).clone().into_dimensionality::<Ix2>().unwrap();
                    let b1 = (**other).clone().into_dimensionality::<Ix1>().unwrap();
                    if a2.ncols() != b1.len() {
                        return Err(misaligned(a2.shape(), b1.shape()));
                    }
                    Ok(Value::Tensor(Rc::new(a2.dot(&b1).into_dyn())))
                }
                _ => Err(misaligned(t.shape(), other.shape())),
            }
        }
        "norm" => {
            no_args(name)?;
            Ok(Value::Float(t.mapv(|x| x * x).sum().sqrt()))
        }
        "det" => {
            no_args(name)?;
            Ok(Value::Float(determinant(as_2d_square(t, "det", line, col)?)))
        }
        "inv" => {
            no_args(name)?;
            let a = as_2d_square(t, "inv", line, col)?;
            let id = Array2::<f64>::eye(a.nrows());
            match solve_system(&a, &id) {
                Some(x) => Ok(Value::Tensor(Rc::new(x.into_dyn()))),
                None => Err(HelixError::new(
                    "matrix is singular (not invertible)",
                    line,
                    col,
                )
                .hint("its determinant is zero — check for dependent rows/columns.")),
            }
        }
        "solve" => {
            if args.len() != 1 {
                return Err(HelixError::new("`solve` takes one right-hand side", line, col)
                    .hint("solve `A x = b` with `a.solve(b)`."));
            }
            let a = as_2d_square(t, "solve", line, col)?;
            let b = match &args[0] {
                Value::Tensor(b) => b,
                o => {
                    return Err(HelixError::new(
                        format!("`solve` needs a tensor right-hand side, got a {}", o.type_name()),
                        line,
                        col,
                    ))
                }
            };
            let was_1d = b.ndim() == 1;
            let b2 = if was_1d {
                (**b).clone().into_dimensionality::<Ix1>().unwrap().insert_axis(Axis(1))
            } else {
                (**b).clone().into_dimensionality::<Ix2>().map_err(|_| {
                    HelixError::new(
                        format!("`solve` right-hand side must be 1-D or 2-D, got {:?}", b.shape()),
                        line,
                        col,
                    )
                })?
            };
            if b2.nrows() != a.nrows() {
                return Err(HelixError::new(
                    format!(
                        "`solve`: A is {}×{} but b has {} rows",
                        a.nrows(),
                        a.ncols(),
                        b2.nrows()
                    ),
                    line,
                    col,
                ));
            }
            match solve_system(&a, &b2) {
                Some(x) => {
                    let out = if was_1d {
                        x.column(0).to_owned().into_dyn()
                    } else {
                        x.into_dyn()
                    };
                    Ok(Value::Tensor(Rc::new(out)))
                }
                None => Err(HelixError::new("matrix is singular — no unique solution", line, col)),
            }
        }
        _ => {
            let mut err = HelixError::new(
                format!("a Tensor has no method `{}`", name),
                line,
                col,
            );
            if let Some(s) = suggest(name, TENSOR_METHODS) {
                err = err.hint(format!("did you mean `{}`?", s));
            } else {
                err = err.hint(format!("Tensor methods: {}", TENSOR_METHODS.join(", ")));
            }
            Err(err)
        }
    }
}

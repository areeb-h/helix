//! Tensor engine — dense n-dimensional `f64` arrays backed by `ndarray`
//! (ADR 0007). The Helix surface (shape/reshape/matmul/broadcasting/reductions)
//! is backend-independent, so a GPU/autodiff backend can slot in behind it later.

use std::rc::Rc;

use ndarray::{Array2, ArrayD, Axis, Ix1, Ix2, IxDyn, Zip};

use faer::linalg::solvers::Solve;

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
            let sub = shape_of(&items.get(0), line, col)?;
            for it in items.to_values().iter() {
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
            for it in items.to_values().iter() {
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
    // `broadcast_shape` above already proved these shapes broadcast; fall back to a
    // clean error rather than `expect` so a missed edge case can never panic.
    let bcast_err = || {
        HelixError::new(format!("cannot broadcast tensors of shape {:?} and {:?}", a.shape(), b.shape()), line, col)
    };
    let av = a.broadcast(IxDyn(&shape)).ok_or_else(bcast_err)?;
    let bv = b.broadcast(IxDyn(&shape)).ok_or_else(bcast_err)?;
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


// ---------- linear algebra (faer: SIMD blocked LU, pure-Rust, no BLAS dep) ----------

/// ndarray (row-major) → faer `Mat` (column-major). The element closure makes the
/// layout difference irrelevant.
fn nd_to_faer(a: &Array2<f64>) -> faer::Mat<f64> {
    faer::Mat::from_fn(a.nrows(), a.ncols(), |i, j| a[[i, j]])
}

/// faer matrix → ndarray `Array2`.
fn faer_to_nd(m: faer::MatRef<'_, f64>) -> Array2<f64> {
    Array2::from_shape_fn((m.nrows(), m.ncols()), |(i, j)| *m.get(i, j))
}

/// Determinant via faer's partial-pivoting LU (SIMD, blocked). Returns 0.0 for a
/// singular matrix (faer's LU yields a zero pivot product there).
fn determinant(a: Array2<f64>) -> f64 {
    if a.nrows() == 0 {
        return 1.0;
    }
    nd_to_faer(&a).as_ref().determinant()
}

/// Solve `A x = b` (b may have multiple columns) via faer's partial-pivoting LU.
/// Returns None if A is singular — detected by a near-zero pivot on the LU's `U`
/// diagonal, preserving the old `< 1e-12` threshold (faer's LU is not itself
/// rank-revealing, so we guard explicitly rather than return inf/NaN).
fn solve_system(a: &Array2<f64>, b: &Array2<f64>) -> Option<Array2<f64>> {
    let n = a.nrows();
    let lu = nd_to_faer(a).partial_piv_lu();
    let u = lu.U();
    for i in 0..n {
        if u.get(i, i).abs() < 1e-12 {
            return None; // singular
        }
    }
    let fb = nd_to_faer(b);
    let x = lu.solve(fb.as_ref());
    Some(faer_to_nd(x.as_ref()))
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
            .to_values()
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

/// Matrix product, parallelized over output row-blocks past a work threshold. Each
/// block is `A[rows] · B` via ndarray's `dot`, so every output element's inner sum is
/// computed exactly as in the single-threaded path — the result is bit-identical, only
/// spread across cores. Below the threshold (or few rows) it stays sequential.
fn parallel_matmul(a: &Array2<f64>, b: &Array2<f64>) -> Array2<f64> {
    let (m, k, n) = (a.nrows(), a.ncols(), b.ncols());
    // Below ~64M multiply-adds (e.g. 400×400×400), the rayon hand-off + the O(m·n)
    // re-stitch of the row blocks costs more than the parallel speedup — small matmul
    // is SIMD/BLAS turf, not thread turf — so stay single-threaded there. (At 200×200
    // ndarray's `dot` is the fast path; parallelism only pays at large sizes.)
    if m < 16 || (m as u128) * (k as u128) * (n as u128) < 64_000_000 {
        return a.dot(b);
    }
    parallel_matmul_blocks(a, b)
}

/// The parallel core (no threshold) — split out so a test can exercise it on a small
/// matrix and assert bit-identicality against `a.dot(b)`.
fn parallel_matmul_blocks(a: &Array2<f64>, b: &Array2<f64>) -> Array2<f64> {
    use rayon::prelude::*;
    let m = a.nrows();
    let chunk = (m / (rayon::current_num_threads().max(1) * 4)).max(1);
    let starts: Vec<usize> = (0..m).step_by(chunk).collect();
    let parts: Vec<Array2<f64>> = starts
        .par_iter()
        .map(|&s| {
            let e = (s + chunk).min(m);
            a.slice(ndarray::s![s..e, ..]).dot(b)
        })
        .collect();
    let views: Vec<_> = parts.iter().map(|p| p.view()).collect();
    ndarray::concatenate(Axis(0), &views).expect("row blocks share column count")
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
            Ok(Value::array(dims))
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
            // Checked product: a shape with absurd dimensions would otherwise overflow
            // `usize` (a debug-build panic / wrong validation) before the count check.
            let prod: usize = match shape.iter().try_fold(1usize, |acc, &d| acc.checked_mul(d)) {
                Some(p) => p,
                None => {
                    return Err(HelixError::new(
                        format!("cannot reshape into {shape:?}: the element count overflows"),
                        line,
                        col,
                    )
                    .hint("the requested shape is far too large."))
                }
            };
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
                // matrix · matrix. ndarray's portable `dot` beats a faer round-trip at
                // these sizes — the row-major↔column-major copy faer needs costs O(n²),
                // which only pays off against its O(n³) gemm for much larger matrices.
                // Past a work threshold, the output rows are computed in parallel by
                // chunk: each C row-block is `A[block] · B`, so every output element's
                // sum is computed exactly as before — the result is BIT-IDENTICAL to the
                // single-threaded dot, just spread across cores.
                (2, 2) => {
                    let a2 = (**t).clone().into_dimensionality::<Ix2>().unwrap();
                    let b2 = (**other).clone().into_dimensionality::<Ix2>().unwrap();
                    if a2.ncols() != b2.nrows() {
                        return Err(misaligned(a2.shape(), b2.shape()));
                    }
                    let out = parallel_matmul(&a2, &b2);
                    Ok(Value::Tensor(Rc::new(out.into_dyn())))
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
        // softmax along one axis (default: the last) — numerically stable (subtract the
        // per-slice max first). The standard neural-net output normalization: rows of a
        // (batch × classes) tensor become probability distributions.
        "softmax" => {
            if t.ndim() == 0 {
                return Err(HelixError::new("cannot take `softmax` of a 0-D (scalar) tensor", line, col));
            }
            let ax = axis_arg(args, t.ndim(), line, col)?.unwrap_or(t.ndim() - 1);
            let axis = Axis(ax);
            let maxes = t.fold_axis(axis, f64::NEG_INFINITY, |&a, &x| a.max(x)).insert_axis(axis);
            let maxes_b = maxes
                .broadcast(t.raw_dim())
                .ok_or_else(|| HelixError::new("softmax broadcast failed", line, col))?;
            let mut ex = ArrayD::<f64>::zeros(t.raw_dim());
            Zip::from(&mut ex).and(&**t).and(&maxes_b).for_each(|o, &x, &m| *o = (x - m).exp());
            let sums = ex.sum_axis(axis).insert_axis(axis);
            let sums_b = sums
                .broadcast(ex.raw_dim())
                .ok_or_else(|| HelixError::new("softmax broadcast failed", line, col))?;
            let mut out = ArrayD::<f64>::zeros(ex.raw_dim());
            Zip::from(&mut out).and(&ex).and(&sums_b).for_each(|o, &e, &s| *o = e / s);
            Ok(Value::Tensor(Rc::new(out)))
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
            let methods = crate::registry::methods_of(crate::registry::TENSOR_METHODS);
            if let Some(s) = suggest(name, &methods) {
                err = err.hint(format!("did you mean `{}`?", s));
            } else {
                err = err.hint(format!("Tensor methods: {}", methods.join(", ")));
            }
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_matmul_is_bit_identical_to_sequential() {
        // A matrix big enough to take the parallel path (150*90*120 > 1M).
        let a = Array2::from_shape_fn((150, 90), |(i, j)| ((i * 7 + j) as f64).sin());
        let b = Array2::from_shape_fn((90, 120), |(i, j)| ((i + j * 3) as f64).cos());
        let par = parallel_matmul_blocks(&a, &b); // force the parallel core
        let seq = a.dot(&b);
        assert_eq!(par, seq, "parallel matmul must equal sequential bit-for-bit");
    }
}

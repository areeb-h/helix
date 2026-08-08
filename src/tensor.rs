//! Tensor engine — dense n-dimensional `f64` arrays backed by `ndarray`
//! (ADR 0007). The Helix surface (shape/reshape/matmul/broadcasting/reductions)
//! is backend-independent, so a GPU/autodiff backend can slot in behind it later.

use std::rc::Rc;

use ndarray::{Array2, ArrayD, Axis, Ix1, Ix2, IxDyn, Zip};

use faer::linalg::solvers::Solve;

use crate::ast::BinOp;
use crate::error::{suggest, HelixError};
use crate::value::{ArrayData, Value};

pub type Tensor = ArrayD<f64>;

// ---------- construction ----------

/// Infer the (rectangular) shape of a nested Helix value.
fn shape_of(v: &Value, line: usize, col: usize) -> Result<Vec<usize>, HelixError> {
    match v {
        Value::Int(_) | Value::Float(_) => Ok(vec![]),
        Value::Array(items) => {
            // A PACKED numeric buffer is 1-D by construction and every element is a scalar, so
            // its shape is `[len]` and there is nothing to inspect. Without this, `to_values()`
            // below BOXES every element into a `Value` purely to re-derive `vec![]` from each
            // one and throw it away — and since a nested build calls this per row, a 1024×1024
            // tensor paid ~1M allocations before a single number was copied. No error is
            // skipped: a packed buffer cannot hold `missing` or a non-numeric element, which
            // are the only things the element walk rejects.
            if matches!(
                &**items,
                ArrayData::Ints(_) | ArrayData::Floats(_) | ArrayData::Range { .. }
            ) {
                return Ok(vec![items.len()]);
            }
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
        // A packed buffer copies straight through — `extend_from_slice` on `Floats` is the
        // memcpy this function should always have been, and the `Ints`/`Range` arms convert
        // in place. The general arm below is unchanged, and `to_values()` is free there
        // (`ArrayData::Values` borrows); it was the packed cases that boxed every element.
        Value::Array(items) => match &**items {
            ArrayData::Floats(v) => out.extend_from_slice(v),
            ArrayData::Ints(v) => out.extend(v.iter().map(|&n| n as f64)),
            // `range_at`'s element formula verbatim. The `i128` widening matches it exactly
            // rather than relying on the range invariant a caller could later weaken.
            ArrayData::Range { start, step, len } => out.extend(
                (0..*len).map(|i| (*start as i128 + *step as i128 * i as i128) as i64 as f64),
            ),
            _ => {
                for it in items.to_values().iter() {
                    flatten_into(it, out, line, col)?;
                }
            }
        },
        Value::Missing => {
            return Err(HelixError::new("tensors cannot contain `missing`", line, col)
                .hint("drop or impute missing values before building a tensor."))
        }
        other => {
            return Err(HelixError::new(
                format!("a tensor element must be a number, not {}", crate::value::with_article(other.type_name())),
                line,
                col,
            ))
        }
    }
    Ok(())
}

pub fn from_value(v: &Value, line: usize, col: usize) -> Result<Tensor, HelixError> {
    let shape = shape_of(v, line, col)?;
    // The shape is already known, so the buffer can be sized once instead of doubling its way
    // to 8 MB. `checked_mul` because a bogus shape must not wrap into a small capacity; on
    // overflow we simply do not preallocate and `from_shape_vec` reports the real error.
    let count = shape.iter().try_fold(1usize, |a, &d| a.checked_mul(d)).unwrap_or(0);
    let mut data = Vec::with_capacity(count);
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

/// Zero-copy matmul via faer (the kernix trick): row-major `A [m,k]` viewed as a
/// column-major `(k,m)` slice *is* `Aᵀ` with no transpose; likewise `B [k,n]` viewed
/// `(n,k)` is `Bᵀ`. Then `Cᵀ = Bᵀ·Aᵀ` is an `[n,m]` column-major result, and a column-
/// major `[n,m]` buffer is bit-for-bit a row-major `[m,n]` buffer — so faer writes
/// straight into our output. Zero copies in or out; only the GEMM touches memory.
fn faer_matmul(a: &Array2<f64>, b: &Array2<f64>) -> Array2<f64> {
    use faer::linalg::matmul::matmul;
    use faer::mat::{MatMut, MatRef};
    use faer::{Accum, Par};
    let (m, k, n) = (a.nrows(), a.ncols(), b.ncols());
    // Standard (row-major) contiguous slices; `as_standard_layout` copies only if a
    // view is non-contiguous (transposed/strided), else borrows.
    let a_std = a.as_standard_layout();
    let b_std = b.as_standard_layout();
    let a_slice = a_std.as_slice().expect("standard layout is contiguous");
    let b_slice = b_std.as_slice().expect("standard layout is contiguous");
    let a_t = MatRef::from_column_major_slice(a_slice, k, m); // Aᵀ
    let b_t = MatRef::from_column_major_slice(b_slice, n, k); // Bᵀ
    let mut out = vec![0.0f64; m * n];
    {
        let dst = MatMut::from_column_major_slice_mut(&mut out, n, m); // Cᵀ [n,m]
        let par = if (m as u128) * (k as u128) * (n as u128) >= 64_000_000 {
            Par::rayon(0)
        } else {
            Par::Seq
        };
        matmul(dst, Accum::Replace, b_t, a_t, 1.0, par); // Cᵀ = Bᵀ·Aᵀ
    }
    // The column-major Cᵀ [n,m] buffer is bit-identical to row-major C [m,n].
    Array2::from_shape_vec((m, n), out).expect("m*n length")
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
                        format!("`{}` needs a tensor, got {}", name, crate::value::with_article(o.type_name())),
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
                // matrix · matrix via faer's SIMD-blocked GEMM, zero-copy in both
                // directions (see `faer_matmul`): 1.2× at 200², 4–6× at 512²–1024² vs
                // ndarray's `dot`. faer parallelizes internally and is deterministic —
                // `Par::Seq` and `Par::rayon` give bit-identical results — so a given
                // binary is reproducible regardless of core count. (Last-bit values differ
                // from the old ndarray `dot` by ~1e-13: a different, equally-valid
                // accumulation order. faer dispatches on CPU SIMD width at run time, so
                // results can differ across *machines* — as autovectorized `dot` already
                // could across build targets.)
                (2, 2) => {
                    let a2 = (**t).clone().into_dimensionality::<Ix2>().unwrap();
                    let b2 = (**other).clone().into_dimensionality::<Ix2>().unwrap();
                    if a2.ncols() != b2.nrows() {
                        return Err(misaligned(a2.shape(), b2.shape()));
                    }
                    let out = faer_matmul(&a2, &b2);
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
                        format!("`solve` needs a tensor right-hand side, got {}", crate::value::with_article(o.type_name())),
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

    /// `shape_of` short-circuits a PACKED buffer to `[len]` without walking its elements, and
    /// `flatten_into` copies one straight through. That is sound only because a packed buffer
    /// holds numbers by construction — so the element walk it skips could never have rejected
    /// anything. The cases that would expose a mistake are ragged nesting where one row is
    /// packed and another is not (in BOTH orientations, since only the non-first row is
    /// compared against the first), and the `Ints`/`Range` → `f64` conversions.
    ///
    /// Before this, `to_values()` boxed every element into a `Value` twice — once per row to
    /// re-derive `vec![]` and discard it, once to copy — so a 1024×1024 build paid ~2M
    /// allocations. It cost 0.12s of k8's wall time, more than the GEMM.
    #[test]
    fn tensor_construction_takes_the_packed_path_without_weakening_its_checks() {
        use crate::value::Value;
        let f = |v: &Value| from_value(v, 1, 1);
        let ints = |v: Vec<i64>| Value::Array(std::rc::Rc::new(ArrayData::Ints(v)));
        let floats = |v: Vec<f64>| Value::Array(std::rc::Rc::new(ArrayData::Floats(v)));
        let nested = |v: Vec<Value>| Value::Array(std::rc::Rc::new(ArrayData::Values(v)));

        // Packed sources convert, and `Ints` widen to `f64`.
        assert_eq!(f(&floats(vec![1.0, 2.5])).unwrap().shape(), &[2]);
        let t = f(&ints(vec![-3, 4])).unwrap();
        assert_eq!(t.iter().copied().collect::<Vec<f64>>(), vec![-3.0, 4.0]);
        // A lazy range must produce `range_at`'s elements, negative step included.
        let rng = |start, step, len| Value::Array(std::rc::Rc::new(ArrayData::Range { start, step, len }));
        assert_eq!(
            f(&rng(10, -2, 5)).unwrap().iter().copied().collect::<Vec<f64>>(),
            vec![10.0, 8.0, 6.0, 4.0, 2.0]
        );
        assert_eq!(f(&rng(0, 1, 0)).unwrap().shape(), &[0]);
        // An empty packed buffer keeps the `[0]` shape the element walk used to produce.
        assert_eq!(f(&ints(vec![])).unwrap().shape(), &[0]);

        // Nested rows: shape and element order (a row/column mix-up would show here).
        let m = f(&nested(vec![ints(vec![1, 2]), ints(vec![3, 4])])).unwrap();
        assert_eq!(m.shape(), &[2, 2]);
        assert_eq!(m.iter().copied().collect::<Vec<f64>>(), vec![1.0, 2.0, 3.0, 4.0]);

        // RAGGED must still be caught — including when one row is packed and the other is
        // not, in both orders, since only non-first rows are compared against the first.
        for bad in [
            nested(vec![floats(vec![1.0, 2.0]), floats(vec![3.0])]),
            nested(vec![floats(vec![1.0, 2.0]), nested(vec![floats(vec![3.0])])]),
            nested(vec![nested(vec![floats(vec![3.0])]), floats(vec![1.0, 2.0])]),
        ] {
            assert!(f(&bad).is_err(), "ragged input was accepted");
        }
        // And a non-numeric element inside a general array is still rejected.
        assert!(f(&nested(vec![Value::Float(1.0), Value::Missing])).is_err());
        let s = Value::Str(std::rc::Rc::new(String::from("x")));
        assert!(f(&nested(vec![Value::Float(1.0), s])).is_err());
    }

    #[test]
    fn faer_matmul_is_correct_and_deterministic() {
        // Correctness vs a hand-rolled triple loop (the ground truth), and exactness of
        // the zero-copy row-major↔column-major reshape (no transpose bug). A non-square
        // case so a row/col mix-up would show.
        let (m, k, n) = (12usize, 9usize, 7usize);
        let a = Array2::from_shape_fn((m, k), |(i, j)| ((i * 3 + j) as f64).sin());
        let b = Array2::from_shape_fn((k, n), |(i, j)| ((i + j * 2) as f64).cos());
        let got = faer_matmul(&a, &b);
        assert_eq!(got.dim(), (m, n));
        for i in 0..m {
            for j in 0..n {
                let want: f64 = (0..k).map(|p| a[[i, p]] * b[[p, j]]).sum();
                assert!(
                    (got[[i, j]] - want).abs() < 1e-12,
                    "({i},{j}): {} vs {want}",
                    got[[i, j]]
                );
            }
        }
        // Determinism: faer's internal parallelism must not change the result — a binary
        // is reproducible regardless of core count (the property Helix relies on).
        use faer::linalg::matmul::matmul;
        use faer::mat::{MatMut, MatRef};
        use faer::{Accum, Par};
        let a = Array2::from_shape_fn((600, 600), |(i, j)| ((i * 3 + j) as f64).sin());
        let b = Array2::from_shape_fn((600, 600), |(i, j)| ((i + j * 5) as f64).cos());
        let run = |par| {
            let (m, k, n) = (600usize, 600usize, 600usize);
            let at = MatRef::from_column_major_slice(a.as_slice().unwrap(), k, m);
            let bt = MatRef::from_column_major_slice(b.as_slice().unwrap(), n, k);
            let mut out = vec![0.0f64; m * n];
            {
                let dst = MatMut::from_column_major_slice_mut(&mut out, n, m);
                matmul(dst, Accum::Replace, bt, at, 1.0, par);
            }
            out
        };
        assert_eq!(
            run(Par::Seq),
            run(Par::rayon(0)),
            "faer matmul must be deterministic across Par"
        );
    }
}

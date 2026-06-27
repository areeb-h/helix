//! Reverse-mode automatic differentiation over scalars and dense `f64` tensors.
//!
//! A `Node` is a value in a computation graph: its forward `value` plus the
//! information needed to push a gradient back to the inputs that produced it (a
//! `backward` closure and the parent nodes). Building an expression out of the
//! arithmetic / matmul / activation primitives below records the graph; `backward`
//! walks it once in reverse-topological order, accumulating each node's gradient.
//!
//! The surface is deliberately the *explicit-graph* API, not a `grad(f, x)` that
//! takes a function: a Helix lambda compiles to bytecode under the VM and to an AST
//! closure under the tree-walker, so a builtin can't invoke one uniformly. Instead
//! the user wraps a leaf with `variable(x)`, writes the forward pass inline (the ops
//! intercept on `Value::Node` in the engine-shared `eval_binary`/`call_method`/
//! `call_builtin`, so both engines behave identically), and asks for
//! `gradient(loss, x)`. Reductions to a scalar loss are required before `backward`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use ndarray::{ArrayD, Axis, Ix1, Ix2, IxDyn, Zip};

use crate::ast::BinOp;
use crate::error::HelixError;
use crate::tensor::broadcast_shape;
use crate::value::Value;

pub type Arr = ArrayD<f64>;

/// A node's local backward rule: given this node's accumulated gradient, the
/// contribution to each parent (same order as `parents`), reduced to its shape.
type Backprop = Box<dyn Fn(&Arr) -> Vec<Arr>>;

/// A node in the autodiff graph. Cheap to clone (it is always held behind `Rc`).
pub struct Node {
    value: Arr,
    grad: RefCell<Arr>,
    parents: Vec<Rc<Node>>,
    backward: Backprop,
}

fn make(value: Arr, parents: Vec<Rc<Node>>, backward: Backprop) -> Rc<Node> {
    let grad = RefCell::new(ArrayD::zeros(value.raw_dim()));
    Rc::new(Node { value, grad, parents, backward })
}

/// A graph leaf (an input / parameter): no parents, no incoming gradient rule.
pub fn leaf(value: Arr) -> Rc<Node> {
    make(value, vec![], Box::new(|_| vec![]))
}

// ---------- broadcasting helpers ----------

/// Elementwise binary op with NumPy broadcasting; infallible (forward already
/// proved the shapes broadcast). Falls back to `a` clone on an impossible shape.
fn ew(a: &Arr, b: &Arr, f: impl Fn(f64, f64) -> f64) -> Arr {
    match broadcast_shape(a.shape(), b.shape()) {
        Some(shape) => {
            let (av, bv) = (a.broadcast(IxDyn(&shape)), b.broadcast(IxDyn(&shape)));
            match (av, bv) {
                (Some(av), Some(bv)) => {
                    let mut out = ArrayD::zeros(IxDyn(&shape));
                    Zip::from(&mut out).and(&av).and(&bv).for_each(|o, &x, &y| *o = f(x, y));
                    out
                }
                _ => a.clone(),
            }
        }
        None => a.clone(),
    }
}

/// Sum a gradient back down to `target` shape — the adjoint of broadcasting: any
/// axis that was stretched in the forward pass is summed out in the backward pass.
fn unbroadcast(g: &Arr, target: &[usize]) -> Arr {
    let mut g = g.clone();
    while g.ndim() > target.len() {
        g = g.sum_axis(Axis(0));
    }
    for (i, &t) in target.iter().enumerate() {
        if t == 1 && g.shape()[i] != 1 {
            g = g.sum_axis(Axis(i)).insert_axis(Axis(i));
        }
    }
    g
}

fn scalar(x: f64) -> Arr {
    ArrayD::from_elem(IxDyn(&[]), x)
}

// ---------- elementwise primitives ----------

fn add(a: &Rc<Node>, b: &Rc<Node>) -> Rc<Node> {
    let value = ew(&a.value, &b.value, |x, y| x + y);
    let (sa, sb) = (a.value.shape().to_vec(), b.value.shape().to_vec());
    make(value, vec![a.clone(), b.clone()], Box::new(move |g| {
        vec![unbroadcast(g, &sa), unbroadcast(g, &sb)]
    }))
}

fn sub(a: &Rc<Node>, b: &Rc<Node>) -> Rc<Node> {
    let value = ew(&a.value, &b.value, |x, y| x - y);
    let (sa, sb) = (a.value.shape().to_vec(), b.value.shape().to_vec());
    make(value, vec![a.clone(), b.clone()], Box::new(move |g| {
        vec![unbroadcast(g, &sa), unbroadcast(&g.mapv(|x| -x), &sb)]
    }))
}

fn mul(a: &Rc<Node>, b: &Rc<Node>) -> Rc<Node> {
    let value = ew(&a.value, &b.value, |x, y| x * y);
    let (av, bv) = (a.value.clone(), b.value.clone());
    let (sa, sb) = (a.value.shape().to_vec(), b.value.shape().to_vec());
    make(value, vec![a.clone(), b.clone()], Box::new(move |g| {
        vec![unbroadcast(&ew(g, &bv, |x, y| x * y), &sa), unbroadcast(&ew(g, &av, |x, y| x * y), &sb)]
    }))
}

fn div(a: &Rc<Node>, b: &Rc<Node>) -> Rc<Node> {
    let value = ew(&a.value, &b.value, |x, y| x / y);
    let (av, bv) = (a.value.clone(), b.value.clone());
    let (sa, sb) = (a.value.shape().to_vec(), b.value.shape().to_vec());
    make(value, vec![a.clone(), b.clone()], Box::new(move |g| {
        // d/da = g/b ;  d/db = -g*a/b^2
        let ga = ew(g, &bv, |x, y| x / y);
        let gb = ew(&ew(g, &av, |x, y| x * y), &bv, |x, y| -x / (y * y));
        vec![unbroadcast(&ga, &sa), unbroadcast(&gb, &sb)]
    }))
}

fn pow_scalar(a: &Rc<Node>, n: f64) -> Rc<Node> {
    let value = a.value.mapv(|x| x.powf(n));
    let av = a.value.clone();
    make(value, vec![a.clone()], Box::new(move |g| {
        // d/dx x^n = n*x^(n-1)
        let local = av.mapv(|x| n * x.powf(n - 1.0));
        vec![ew(g, &local, |x, y| x * y)]
    }))
}

fn unary(a: &Rc<Node>, fwd: fn(f64) -> f64, deriv: fn(f64) -> f64) -> Rc<Node> {
    let value = a.value.mapv(fwd);
    let av = a.value.clone();
    make(value, vec![a.clone()], Box::new(move |g| {
        let local = av.mapv(deriv);
        vec![ew(g, &local, |x, y| x * y)]
    }))
}

// ---------- reductions ----------

fn sum(a: &Rc<Node>) -> Rc<Node> {
    let value = scalar(a.value.sum());
    let shape = a.value.raw_dim();
    make(value, vec![a.clone()], Box::new(move |g| {
        let s = *g.first().unwrap_or(&0.0);
        vec![ArrayD::from_elem(shape.clone(), s)]
    }))
}

fn mean(a: &Rc<Node>) -> Rc<Node> {
    let n = a.value.len().max(1) as f64;
    let value = scalar(a.value.sum() / n);
    let shape = a.value.raw_dim();
    make(value, vec![a.clone()], Box::new(move |g| {
        let s = *g.first().unwrap_or(&0.0) / n;
        vec![ArrayD::from_elem(shape.clone(), s)]
    }))
}

// ---------- matmul ----------

fn matmul(a: &Rc<Node>, b: &Rc<Node>, line: usize, col: usize) -> Result<Rc<Node>, HelixError> {
    let mis = || {
        HelixError::new(
            format!("shapes {:?} and {:?} are not aligned for `matmul`", a.value.shape(), b.value.shape()),
            line,
            col,
        )
    };
    match (a.value.ndim(), b.value.ndim()) {
        // matrix · matrix
        (2, 2) => {
            let am = a.value.clone().into_dimensionality::<Ix2>().unwrap();
            let bm = b.value.clone().into_dimensionality::<Ix2>().unwrap();
            if am.ncols() != bm.nrows() {
                return Err(mis());
            }
            let value = am.dot(&bm).into_dyn();
            let (am2, bm2) = (am.clone(), bm.clone());
            Ok(make(value, vec![a.clone(), b.clone()], Box::new(move |g| {
                let g2 = g.clone().into_dimensionality::<Ix2>().unwrap();
                let da = g2.dot(&bm2.t()).into_dyn();
                let db = am2.t().dot(&g2).into_dyn();
                vec![da, db]
            })))
        }
        // matrix · vector → vector
        (2, 1) => {
            let am = a.value.clone().into_dimensionality::<Ix2>().unwrap();
            let bv = b.value.clone().into_dimensionality::<Ix1>().unwrap();
            if am.ncols() != bv.len() {
                return Err(mis());
            }
            let value = am.dot(&bv).into_dyn();
            let (am2, bv2) = (am.clone(), bv.clone());
            Ok(make(value, vec![a.clone(), b.clone()], Box::new(move |g| {
                let g1 = g.clone().into_dimensionality::<Ix1>().unwrap();
                // dA = outer(g, x) ; dx = A^T g
                let da = ndarray::Array2::from_shape_fn((am2.nrows(), am2.ncols()), |(i, j)| g1[i] * bv2[j]);
                let db = am2.t().dot(&g1);
                vec![da.into_dyn(), db.into_dyn()]
            })))
        }
        // vector · vector → scalar
        (1, 1) => {
            let av = a.value.clone().into_dimensionality::<Ix1>().unwrap();
            let bv = b.value.clone().into_dimensionality::<Ix1>().unwrap();
            if av.len() != bv.len() {
                return Err(mis());
            }
            let value = scalar(av.dot(&bv));
            let (av2, bv2) = (av.clone(), bv.clone());
            Ok(make(value, vec![a.clone(), b.clone()], Box::new(move |g| {
                let s = *g.first().unwrap_or(&0.0);
                vec![(&bv2 * s).into_dyn(), (&av2 * s).into_dyn()]
            })))
        }
        _ => Err(mis()),
    }
}

// ---------- backward pass ----------

fn run_backward(root: &Rc<Node>) {
    let mut topo: Vec<Rc<Node>> = Vec::new();
    let mut seen: HashSet<*const Node> = HashSet::new();
    // Iterative post-order DFS (avoids native stack overflow on a deep graph).
    let mut stack: Vec<(Rc<Node>, usize)> = vec![(root.clone(), 0)];
    while let Some((node, i)) = stack.pop() {
        if i < node.parents.len() {
            stack.push((node.clone(), i + 1));
            let p = node.parents[i].clone();
            if !seen.contains(&(Rc::as_ptr(&p))) {
                stack.push((p, 0));
            }
        } else if seen.insert(Rc::as_ptr(&node)) {
            topo.push(node);
        }
    }
    for n in &topo {
        *n.grad.borrow_mut() = ArrayD::zeros(n.value.raw_dim());
    }
    *root.grad.borrow_mut() = ArrayD::ones(root.value.raw_dim());
    for n in topo.iter().rev() {
        let g = n.grad.borrow().clone();
        let contribs = (n.backward)(&g);
        for (parent, c) in n.parents.iter().zip(contribs) {
            let mut pg = parent.grad.borrow_mut();
            *pg = &*pg + &c;
        }
    }
}

// ---------- Value glue ----------

/// A plain numeric `Value` as an `Arr` (scalars become 0-D). `None` for non-numerics.
fn to_arr(v: &Value) -> Option<Arr> {
    match v {
        Value::Int(i) => Some(scalar(*i as f64)),
        Value::Float(f) => Some(scalar(*f)),
        Value::Tensor(t) => Some((**t).clone()),
        _ => None,
    }
}

/// Lift a `Value` to a graph node: a `Node` passes through, a constant becomes a leaf.
fn to_node(v: &Value) -> Option<Rc<Node>> {
    match v {
        Value::Node(n) => Some(n.clone()),
        _ => to_arr(v).map(leaf),
    }
}

/// Public `to_node`: used by `call_method` to pull a plain numeric *receiver* into the
/// graph when one of its arguments is tracked (e.g. `X.matmul(w)` with `X` constant).
pub fn lift(v: &Value) -> Option<Rc<Node>> {
    to_node(v)
}

/// An `Arr` back to a Helix value: 0-D → `Float`, otherwise `Tensor`.
fn arr_to_value(a: Arr) -> Value {
    if a.ndim() == 0 {
        Value::Float(*a.first().unwrap_or(&0.0))
    } else {
        Value::Tensor(Rc::new(a))
    }
}

/// The forward value carried by a node (for `value_of` and display).
pub fn node_value(n: &Rc<Node>) -> Value {
    arr_to_value(n.value.clone())
}

/// Arithmetic on a graph: at least one operand is a `Node`.
pub fn binary(op: &BinOp, l: &Value, r: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let bad = |v: &Value| {
        HelixError::new(
            format!("a tracked value can't be combined with a {}", v.type_name()),
            line,
            col,
        )
        .hint("differentiable ops are + - * / ** and matmul, over numbers and tensors.")
    };
    let a = to_node(l).ok_or_else(|| bad(l))?;
    let b = to_node(r).ok_or_else(|| bad(r))?;
    use BinOp::*;
    let out = match op {
        Add => add(&a, &b),
        Sub => sub(&a, &b),
        Mul => mul(&a, &b),
        Div => div(&a, &b),
        Pow => {
            // Only a constant scalar exponent is differentiable here.
            let n = b.value.first().copied();
            match (b.value.ndim() == 0, n) {
                (true, Some(n)) => pow_scalar(&a, n),
                _ => {
                    return Err(HelixError::new(
                        "a tracked value can only be raised to a constant scalar power",
                        line,
                        col,
                    ))
                }
            }
        }
        _ => {
            return Err(HelixError::new(
                format!("`{}` is not differentiable", op.symbol()),
                line,
                col,
            )
            .hint("use + - * / ** , matmul, or an activation (relu/sigmoid/tanh/exp/ln)."))
        }
    };
    Ok(Value::Node(out))
}

/// A differentiable unary builtin (activations + a few math fns) on a node.
pub fn unary_builtin(name: &str, v: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let a = match v {
        Value::Node(n) => n.clone(),
        _ => return Err(HelixError::new("expected a tracked value", line, col)),
    };
    let out = match name {
        "relu" => unary(&a, |x| x.max(0.0), |x| if x > 0.0 { 1.0 } else { 0.0 }),
        "sigmoid" => unary(&a, |x| 1.0 / (1.0 + (-x).exp()), |x| {
            let s = 1.0 / (1.0 + (-x).exp());
            s * (1.0 - s)
        }),
        "tanh" => unary(&a, |x| x.tanh(), |x| 1.0 - x.tanh().powi(2)),
        "exp" => unary(&a, |x| x.exp(), |x| x.exp()),
        "ln" => unary(&a, |x| x.ln(), |x| 1.0 / x),
        "sqrt" => unary(&a, |x| x.sqrt(), |x| 0.5 / x.sqrt()),
        _ => {
            return Err(HelixError::new(
                format!("`{name}` is not differentiable on a tracked value"),
                line,
                col,
            ))
        }
    };
    Ok(Value::Node(out))
}

/// A method call on a node (`matmul`/`dot`/`sum`/`mean`/`t`/`transpose`).
pub fn method(n: &Rc<Node>, name: &str, args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
        "matmul" | "dot" => {
            if args.len() != 1 {
                return Err(HelixError::new(format!("`{name}` takes one argument"), line, col));
            }
            let other = to_node(&args[0]).ok_or_else(|| {
                HelixError::new(format!("`{name}` needs a tensor or tracked value"), line, col)
            })?;
            Ok(Value::Node(matmul(n, &other, line, col)?))
        }
        "sum" => {
            no_method_args(name, args, line, col)?;
            Ok(Value::Node(sum(n)))
        }
        "mean" => {
            no_method_args(name, args, line, col)?;
            Ok(Value::Node(mean(n)))
        }
        "t" | "transpose" => {
            no_method_args(name, args, line, col)?;
            let value = n.value.t().to_owned();
            let out = make(value, vec![n.clone()], Box::new(|g| vec![g.t().to_owned()]));
            Ok(Value::Node(out))
        }
        "relu" | "sigmoid" | "tanh" | "exp" | "ln" | "sqrt" => {
            no_method_args(name, args, line, col)?;
            unary_builtin(name, &Value::Node(n.clone()), line, col)
        }
        _ => Err(HelixError::new(
            format!("a tracked value has no differentiable method `{name}`"),
            line,
            col,
        )
        .hint("methods: matmul/dot, sum, mean, t/transpose, relu, sigmoid, tanh, exp, ln, sqrt.")),
    }
}

fn no_method_args(name: &str, args: &[Value], line: usize, col: usize) -> Result<(), HelixError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(HelixError::new(format!("`{name}` takes no arguments on a tracked value"), line, col))
    }
}

/// `variable(x)` — wrap a number or tensor as a tracked graph leaf.
pub fn variable(v: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    match to_arr(v) {
        Some(a) => Ok(Value::Node(leaf(a))),
        None => Err(HelixError::new(
            format!("`variable` needs a number or tensor, got a {}", v.type_name()),
            line,
            col,
        )),
    }
}

/// `value_of(node)` — the forward value of a tracked value (or pass plain values through).
pub fn value_of(v: &Value) -> Value {
    match v {
        Value::Node(n) => node_value(n),
        other => other.clone(),
    }
}

/// `gradient(loss, x)` — reverse-mode gradient of the scalar `loss` w.r.t. the leaf
/// `x` (or each leaf in an array `x`). Runs one backward pass per call.
pub fn gradient(loss: &Value, x: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let root = match loss {
        Value::Node(n) => n.clone(),
        _ => {
            return Err(HelixError::new(
                "`gradient` needs a tracked loss built from `variable(...)`",
                line,
                col,
            )
            .hint("the first argument must be a scalar produced by differentiable ops."))
        }
    };
    if root.value.ndim() != 0 && root.value.len() != 1 {
        return Err(HelixError::new(
            format!("`gradient` needs a scalar loss, got shape {:?}", root.value.shape()),
            line,
            col,
        )
        .hint("reduce to a scalar with `.sum()` or `.mean()` first."));
    }
    run_backward(&root);
    grad_of(x, line, col)
}

/// Read the (already-computed) gradient out of a leaf node or an array of leaves.
fn grad_of(x: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    match x {
        Value::Node(n) => Ok(arr_to_value(n.grad.borrow().clone())),
        Value::Array(items) => {
            let out: Result<Vec<Value>, HelixError> =
                items.to_values().iter().map(|e| grad_of(e, line, col)).collect();
            Ok(Value::array(out?))
        }
        other => Err(HelixError::new(
            format!("`gradient` needs a tracked variable (or array of them), got a {}", other.type_name()),
            line,
            col,
        )),
    }
}

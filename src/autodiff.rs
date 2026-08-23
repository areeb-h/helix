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

use std::cell::{Cell, RefCell};
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
    /// The backward pass that last touched this node (see `EPOCH`). `grad` is only
    /// meaningful for readers from that same pass; a leaf that did not feed the most
    /// recent loss keeps a stale accumulation here, and `grad_of` must answer 0 for
    /// it, not replay whatever an earlier tape left behind.
    epoch: Cell<u64>,
}

impl Node {
    /// Length of the forward value's leading axis — 0 for a tracked scalar, which
    /// has no axis to index. A slicing caller needs it to resolve bounds exactly as
    /// it does for a plain tensor, without reaching into the tape.
    pub fn axis0_len(&self) -> usize {
        self.value.shape().first().copied().unwrap_or(0)
    }
}

/// Rank of a tracked value's forward payload — 0 for a scalar. Lets an error
/// constructor outside this module say `Tensor` for a tracked tensor without
/// reaching into the node.
pub fn rank_of(n: &Rc<Node>) -> usize {
    n.value.ndim()
}

thread_local! {
    /// Monotonic id of the most recent `run_backward`. 0 means "no pass yet", which
    /// no node ever carries after a pass — so a fresh leaf (epoch 0) always reads as
    /// off-tape until a backward pass actually visits it.
    static EPOCH: Cell<u64> = const { Cell::new(0) };
}

fn make(value: Arr, parents: Vec<Rc<Node>>, backward: Backprop) -> Rc<Node> {
    let grad = RefCell::new(ArrayD::zeros(value.raw_dim()));
    Rc::new(Node { value, grad, parents, backward, epoch: Cell::new(0) })
}

/// A graph leaf (an input / parameter): no parents, no incoming gradient rule.
pub fn leaf(value: Arr) -> Rc<Node> {
    make(value, vec![], Box::new(|_| vec![]))
}

// ---------- broadcasting helpers ----------

/// Elementwise binary op with NumPy broadcasting; infallible because `binary()`
/// refuses non-broadcastable shapes before any node is built (backward-pass calls
/// only combine shapes the forward already proved). The clone fallback is defensive
/// depth only — it must never be the path that answers for a user's shape mistake.
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
        // d/dx x^n = n*x^(n-1); x^0 is the constant 1, so its derivative is 0
        // everywhere — without the guard, n=0 at x=0 computes 0 * inf = NaN.
        let local = av.mapv(|x| if n == 0.0 { 0.0 } else { n * x.powf(n - 1.0) });
        vec![ew(g, &local, |x, y| x * y)]
    }))
}

/// A two-parent elementwise node: `fwd` computes the value, `dl`/`dr` the local
/// derivatives with respect to each side at `(x, y)`. Broadcasting follows the
/// same `ew`/`unbroadcast` adjoint pair the arithmetic primitives use.
fn binary_ew(
    a: &Rc<Node>,
    b: &Rc<Node>,
    fwd: fn(f64, f64) -> f64,
    dl: fn(f64, f64) -> f64,
    dr: fn(f64, f64) -> f64,
) -> Rc<Node> {
    let value = ew(&a.value, &b.value, fwd);
    let (av, bv) = (a.value.clone(), b.value.clone());
    let (sa, sb) = (a.value.shape().to_vec(), b.value.shape().to_vec());
    make(value, vec![a.clone(), b.clone()], Box::new(move |g| {
        let ga = ew(g, &ew(&av, &bv, dl), |x, d| x * d);
        let gb = ew(g, &ew(&av, &bv, dr), |x, d| x * d);
        vec![unbroadcast(&ga, &sa), unbroadcast(&gb, &sb)]
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

// ---------- the scalar ↔ tensor bridge ----------
//
// Two adjoint primitives, and everything at the boundary is built from them:
// `stack` joins N nodes of one shape into a node with a new leading axis, and
// `select` pulls one slice back out. Stacking's adjoint is slicing (parent `k`
// receives the k-th slice of the incoming gradient) and slicing's adjoint is
// scattering (the gradient lands in a zero block at slice `k`) — so a value can
// be assembled out of tracked scalars, computed on with BLAS, and taken apart
// again without ever leaving the tape.

/// Stack `parents` — all of identical shape `S` — into one node of shape `[N, …S]`.
///
/// The caller guarantees a non-empty list of equal shapes; `tensor_node` below is
/// the only caller and checks both, reporting the same ragged error the plain
/// build reports. Equal shapes are what make the backward total: contribution `k`
/// is exactly `parents[k]`'s shape, and gradient accumulation adds same-shape
/// arrays only (a mismatch there is the abort ADR 0024 forbids).
fn stack(parents: Vec<Rc<Node>>) -> Rc<Node> {
    debug_assert!(!parents.is_empty(), "stack: `build` only reaches here with elements");
    let mut shape = vec![parents.len()];
    shape.extend_from_slice(parents[0].value.shape());
    let mut value = ArrayD::zeros(IxDyn(&shape));
    for (k, p) in parents.iter().enumerate() {
        value.index_axis_mut(Axis(0), k).assign(&p.value);
    }
    let n = parents.len();
    make(
        value,
        parents,
        Box::new(move |g| (0..n).map(|k| g.index_axis(Axis(0), k).to_owned()).collect()),
    )
}

/// One slice off the leading axis (`t[k]`), as a node. `k` is already in bounds.
///
/// A 1-D receiver yields a 0-D node — which reads back as a `Float`, exactly what
/// `tensor::index_first` hands back for the same index on a plain tensor.
fn select(n: &Rc<Node>, k: usize) -> Rc<Node> {
    let value = n.value.index_axis(Axis(0), k).to_owned();
    let full = n.value.raw_dim();
    make(
        value,
        vec![n.clone()],
        Box::new(move |g| {
            let mut out = ArrayD::zeros(full.clone());
            out.index_axis_mut(Axis(0), k).assign(g);
            vec![out]
        }),
    )
}

/// Does this value carry a tracked node anywhere inside it?
///
/// The gate on the whole bridge: when this is false — the overwhelmingly common
/// case — tensor construction takes the plain path unchanged, which is a shape
/// walk and a memcpy. Nothing here costs a program that is not differentiating.
pub fn contains_tracked(v: &Value) -> bool {
    use crate::value::ArrayData;
    match v {
        Value::Node(_) => true,
        Value::Array(items) => match &**items {
            // Only a boxed element list can hold one.
            ArrayData::Values(vs) => vs.iter().any(contains_tracked),
            // `Ints`/`Floats`/`Range` are numbers by construction. `Enumerate` and
            // `Zip` are NOT — they can wrap a boxed buffer — but every element they
            // yield is a TUPLE, which no tensor build accepts, tracked or plain. So
            // they answer `false` here and are refused by the build either way,
            // with the plain build's wording. Walking their inners would change no
            // outcome; saying they are "packed" would be wrong.
            _ => false,
        },
        _ => false,
    }
}

/// Build a tracked tensor out of a value containing tracked scalars — the bridge
/// `tensor([w1, w2])` crosses. Mixed plain and tracked elements are fine: a plain
/// number becomes a constant leaf, so gradient flows to the tracked ones only.
///
/// This accepts EXACTLY what the plain build accepts — numbers and nested arrays of
/// numbers — with one addition: a tracked scalar counts as a number. That one
/// sentence is the whole rule, and keeping it that short is deliberate. The plain
/// build refuses a tensor as an element (`tensor([tensor(…), …])`, hint: "nested
/// arrays of numbers"), so this refuses a tracked tensor as an element too; letting
/// one through would mean the legality of an expression depended on whether a
/// variable happened to be inside it. Stacking whole tensors as rows is a real
/// widening for a later release, and it belongs to both builds or neither.
///
/// Every refusal is the plain build's refusal in its words (`tensor::*_err`), so the
/// same mistake reads the same whether or not a variable is in the array.
pub fn tensor_node(v: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    Ok(Value::Node(build(v, line, col)?))
}

fn build(v: &Value, line: usize, col: usize) -> Result<Rc<Node>, HelixError> {
    // A wholly plain subtree is ONE constant block, not one leaf per number: it
    // contributes no gradient, so its internal structure is never needed again, and
    // the plain build already turns it into a dense buffer in a single pass (a
    // packed row memcpies). This is what keeps `tensor([[w, …], [1.0, 2.0, …]])`
    // from boxing a thousand-element row into a thousand nodes, and it hands the
    // plain build's own errors back for anything inside it that is not a number.
    if !contains_tracked(v) {
        return Ok(leaf(crate::tensor::from_value(v, line, col)?));
    }
    // Past here the value holds a tracked one, so it is a node or an array with a
    // node somewhere inside — every plain shape, `missing` and the empty array
    // included, was answered above by the plain build and its wording.
    match v {
        // A tracked SCALAR is a number and passes straight through. A tracked
        // TENSOR is not an element, exactly as a plain tensor is not, and
        // `not_tensor_value_err` gives both the one sentence.
        Value::Node(n) if n.value.ndim() == 0 => Ok(n.clone()),
        Value::Array(items) => {
            // Shapes are compared AS the elements are built, never afterwards: the
            // plain shape walk interleaves the two and stops at the first offending
            // element, so `tensor([[1.0], [2.0, 3.0], "x"])` reports the ragged row
            // rather than the string. Building every element first and comparing at
            // the end would let a bad type anywhere outrank a ragged row anywhere,
            // and the same program would change its error the moment a variable
            // appeared in it. A tracked array is never empty — an empty one holds
            // no node — so the first element always exists to set the shape.
            let mut parts: Vec<Rc<Node>> = Vec::with_capacity(items.len());
            let mut head: Option<Vec<usize>> = None;
            for it in items.to_values().iter() {
                let part = build(it, line, col)?;
                match &head {
                    None => head = Some(part.value.shape().to_vec()),
                    Some(h) if part.value.shape() != h.as_slice() => {
                        return Err(crate::tensor::ragged_err(line, col))
                    }
                    Some(_) => {}
                }
                parts.push(part);
            }
            Ok(stack(parts))
        }
        // `contains_tracked` admits nothing else. Answering with the plain build's
        // refusal rather than an `unreachable!` keeps the function total — the rule
        // ADR 0024 states, and the one the v0.2.6 poison-cell abort broke.
        other => Err(crate::tensor::not_tensor_value_err(other, line, col)),
    }
}

/// `t[i]` on a tracked tensor — differentiable element access, the bridge's other
/// direction. Bounds and wording follow `tensor::index_first` exactly, because to a
/// reader this IS that operation; the only difference is that the result stays on
/// the tape.
pub fn index(n: &Rc<Node>, i: i64, line: usize, col: usize) -> Result<Value, HelixError> {
    if n.value.ndim() == 0 {
        return Err(crate::tensor::index_scalar_err(line, col));
    }
    let len = n.value.shape()[0] as i64;
    let real = if i < 0 { len + i } else { i };
    if real < 0 || real >= len {
        return Err(crate::tensor::index_bounds_err(i, len, line, col));
    }
    Ok(Value::Node(select(n, real as usize)))
}

/// `t[a:b:step]` on a tracked tensor — the slice twin of `index`, over already
/// resolved indices (the caller resolves them exactly as it does for a plain
/// tensor, so the two agree on every edge including a reversing step).
///
/// Gathering rows is a sum over the rows it names, so its adjoint SCATTERS and
/// ACCUMULATES: a row named twice by a slice must receive both contributions.
pub fn slice(n: &Rc<Node>, idxs: &[usize], line: usize, col: usize) -> Result<Value, HelixError> {
    if n.value.ndim() == 0 {
        return Err(HelixError::new("cannot slice a 0-D (scalar) tensor", line, col));
    }
    let picks: Vec<usize> = idxs.to_vec();
    let mut shape = vec![picks.len()];
    shape.extend_from_slice(&n.value.shape()[1..]);
    let mut value = ArrayD::zeros(IxDyn(&shape));
    for (k, &src) in picks.iter().enumerate() {
        value
            .index_axis_mut(Axis(0), k)
            .assign(&n.value.index_axis(Axis(0), src));
    }
    let full = n.value.raw_dim();
    let out = make(
        value,
        vec![n.clone()],
        Box::new(move |g| {
            let mut back = ArrayD::zeros(full.clone());
            for (k, &src) in picks.iter().enumerate() {
                let mut row = back.index_axis_mut(Axis(0), src);
                row += &g.index_axis(Axis(0), k);
            }
            vec![back]
        }),
    );
    Ok(Value::Node(out))
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
    let epoch = EPOCH.with(|e| {
        e.set(e.get() + 1);
        e.get()
    });
    for n in &topo {
        *n.grad.borrow_mut() = ArrayD::zeros(n.value.raw_dim());
        n.epoch.set(epoch);
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
            format!("a tracked value can't be combined with {}", crate::value::with_article(v.type_name())),
            line,
            col,
        )
        .hint("differentiable ops are + - * / ** and matmul, over numbers and tensors.")
    };
    let a = to_node(l).ok_or_else(|| bad(l))?;
    let b = to_node(r).ok_or_else(|| bad(r))?;
    // The tracked path must refuse exactly where the plain path refuses. Without
    // this guard `ew`'s defensive fallback fabricates a forward value (the LHS,
    // unchanged) and the backward accumulation panics on the shape mismatch — the
    // stabilization sweep's silent-wrong-value + uncatchable-abort pair. Scoped to
    // the elementwise ops so a non-differentiable op still reports THAT first.
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div)
        && broadcast_shape(a.value.shape(), b.value.shape()).is_none()
    {
        return Err(HelixError::new(
            format!(
                "cannot broadcast tensors of shape {:?} and {:?}",
                a.value.shape(),
                b.value.shape()
            ),
            line,
            col,
        )
        .hint("shapes must match, or a dimension of 1 stretches to fit (NumPy rules)."));
    }
    use BinOp::*;
    let out = match op {
        Add => add(&a, &b),
        Sub => sub(&a, &b),
        Mul => mul(&a, &b),
        Div => div(&a, &b),
        Pow => {
            // A constant scalar exponent keeps `pow_scalar`'s wider domain
            // (negative bases, integer powers). A TRACKED exponent gets the
            // full two-parent node: d/da = b·a^(b-1), d/db = a^b·ln a — the
            // d/db term needs ln a, so it demands a strictly positive base
            // rather than freezing the exponent (the stabilization sweep's
            // silent-0.0 repro, now a feature instead of a refusal).
            let exp_tracked = matches!(r, Value::Node(_));
            let n = b.value.first().copied();
            if !exp_tracked && b.value.ndim() == 0 {
                match n {
                    Some(n) => pow_scalar(&a, n),
                    None => {
                        return Err(HelixError::new(
                            "a tracked value can only be raised to a constant scalar power",
                            line,
                            col,
                        ))
                    }
                }
            } else if exp_tracked {
                if a.value.iter().any(|&x| x <= 0.0) {
                    return Err(HelixError::new(
                        "a tracked exponent needs a strictly positive base \
                         (d/db of a**b is a**b·ln a)",
                        line,
                        col,
                    )
                    .hint(
                        "if the exponent is really a constant here, read it off the tape \
                         with `value_of(...)`.",
                    ));
                }
                binary_ew(
                    &a,
                    &b,
                    f64::powf,
                    |x, y| y * x.powf(y - 1.0),
                    |x, y| x.powf(y) * x.ln(),
                )
            } else {
                return Err(HelixError::new(
                    "a tracked value can only be raised to a constant scalar power",
                    line,
                    col,
                ));
            }
        }
        _ => {
            return Err(HelixError::new(
                format!("`{}` is not differentiable", op.symbol()),
                line,
                col,
            )
            .hint("use + - * / ** , matmul, or an activation (relu/sigmoid/tanh/exp/ln/sin/cos/abs)."))
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
        "sin" => unary(&a, |x| x.sin(), |x| x.cos()),
        "cos" => unary(&a, |x| x.cos(), |x| -x.sin()),
        // The subgradient convention: 0 at the kink, ±1 elsewhere — the same choice
        // `relu` above already makes at its own kink, so the two stay consistent.
        // (The nn field report was rebuilding `abs` as `sqrt(x² + ε)` to get a
        // gradient at all, and the ε choice was its own trap.)
        "abs" => unary(
            &a,
            |x| x.abs(),
            |x| {
                if x > 0.0 {
                    1.0
                } else if x < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            },
        ),
        "tan" => unary(&a, |x| x.tan(), |x| {
            let c = x.cos();
            1.0 / (c * c)
        }),
        "asin" => unary(&a, |x| x.asin(), |x| 1.0 / (1.0 - x * x).sqrt()),
        "acos" => unary(&a, |x| x.acos(), |x| -1.0 / (1.0 - x * x).sqrt()),
        "atan" => unary(&a, |x| x.atan(), |x| 1.0 / (1.0 + x * x)),
        "sinh" => unary(&a, |x| x.sinh(), |x| x.cosh()),
        "cosh" => unary(&a, |x| x.cosh(), |x| x.sinh()),
        "log2" => unary(&a, |x| x.log2(), |x| 1.0 / (x * std::f64::consts::LN_2)),
        "log10" => unary(&a, |x| x.log10(), |x| 1.0 / (x * std::f64::consts::LN_10)),
        "cbrt" => unary(&a, |x| x.cbrt(), |x| {
            let c = x.cbrt();
            1.0 / (3.0 * c * c)
        }),
        "degrees" => unary(&a, |x| x.to_degrees(), |_| 180.0 / std::f64::consts::PI),
        "radians" => unary(&a, |x| x.to_radians(), |_| std::f64::consts::PI / 180.0),
        // d/dx erf(x) = 2/sqrt(pi) * e^(-x^2); d/dx normal_cdf(x) = normal_pdf(x);
        // d/dx normal_pdf(x) = -x * normal_pdf(x). All exact, all elementary.
        "erf" => unary(&a, crate::stats::erf, |x| {
            2.0 / std::f64::consts::PI.sqrt() * (-x * x).exp()
        }),
        "normal_cdf" => unary(&a, crate::stats::normal_cdf, crate::stats::normal_pdf),
        "normal_pdf" => unary(&a, crate::stats::normal_pdf, |x| {
            -x * crate::stats::normal_pdf(x)
        }),
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

/// The uniform refusal for an op whose derivative is zero or undefined almost
/// everywhere (`floor`, `round`, `sign`, …): the error names the real problem —
/// the OPERATION — never the value's type, and says what to do instead.
pub fn not_differentiable(name: &str, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("`{name}` is not differentiable on a tracked value"), line, col)
        .hint(
            "its derivative is zero (or undefined) almost everywhere, so the tape refuses \
             rather than silently zeroing your gradient; `value_of(x)` deliberately leaves \
             the tape if the plain number is what you want.",
        )
}

/// Named two-argument differentiable builtins — the binary twin of
/// `unary_builtin`. Ties route the gradient to the FIRST argument, the same
/// convention `relu`'s kink already sets (relu'(0) = 0), so `max(a, b)` and the
/// field idiom `a + relu(b - a)` agree everywhere, tie included.
pub fn binary_builtin(
    name: &str,
    l: &Value,
    r: &Value,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let bad = |v: &Value| {
        HelixError::new(
            format!(
                "a tracked value can't be combined with {}",
                crate::value::with_article(v.type_name())
            ),
            line,
            col,
        )
        .hint(
            "differentiable ops are + - * / ** , matmul, and max/min/clamp/hypot, over \
             numbers and tensors.",
        )
    };
    let a = to_node(l).ok_or_else(|| bad(l))?;
    let b = to_node(r).ok_or_else(|| bad(r))?;
    // The same guard `binary()` carries: refuse exactly where the plain path
    // refuses, before `ew`'s defensive fallback can fabricate a forward value.
    if broadcast_shape(a.value.shape(), b.value.shape()).is_none() {
        return Err(HelixError::new(
            format!(
                "cannot broadcast tensors of shape {:?} and {:?}",
                a.value.shape(),
                b.value.shape()
            ),
            line,
            col,
        )
        .hint("shapes must match, or a dimension of 1 stretches to fit (NumPy rules)."));
    }
    let out = match name {
        "max" => binary_ew(
            &a,
            &b,
            f64::max,
            |x, y| if x >= y { 1.0 } else { 0.0 },
            |x, y| if x >= y { 0.0 } else { 1.0 },
        ),
        "min" => binary_ew(
            &a,
            &b,
            f64::min,
            |x, y| if x <= y { 1.0 } else { 0.0 },
            |x, y| if x <= y { 0.0 } else { 1.0 },
        ),
        // d/dx hypot = x/hypot, d/dy = y/hypot; the origin's subgradient is 0.
        "hypot" => binary_ew(
            &a,
            &b,
            f64::hypot,
            |x, y| {
                let h = x.hypot(y);
                if h == 0.0 { 0.0 } else { x / h }
            },
            |x, y| {
                let h = x.hypot(y);
                if h == 0.0 { 0.0 } else { y / h }
            },
        ),
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

/// Does this free builtin ACCEPT A TRACKED VALUE and extend the tape? The
/// list `helix describe` reports — kept honest by a unit test that actually
/// differentiates every flagged name. (`variable`/`gradient`/`value_of` are the
/// tape's own tooling; `to_array` reads a value OFF the tape — their describe
/// notes say so, and none of them is an "op" in this sense.)
pub fn differentiable_builtin(name: &str) -> bool {
    matches!(
        name,
        "relu" | "sigmoid" | "tanh" | "exp" | "ln" | "sqrt" | "sin" | "cos" | "abs" | "tan"
            | "asin" | "acos" | "atan" | "sinh" | "cosh" | "log2" | "log10" | "cbrt"
            | "degrees" | "radians" | "erf" | "normal_cdf" | "normal_pdf" | "max" | "min"
            | "clamp" | "hypot"
    )
}

/// The forward value's rank — the tracked-fold element gate uses it to admit
/// exactly the tracked SCALARS the plain folds' number rule admits.
pub fn node_ndim(n: &Rc<Node>) -> usize {
    n.value.ndim()
}

/// The tape's own method names. `ufcs_fallback_applies` consults this so a
/// FAILED method call on a tracked value may retry as the free builtin
/// (`v.to_array()` → `to_array(v)`, `v.tan()` → `tan(v)`), while a name the
/// tape owns keeps the tape's error — a method that owns its name always wins.
pub fn is_tape_method(name: &str) -> bool {
    matches!(
        name,
        "matmul"
            | "dot"
            | "sum"
            | "mean"
            | "t"
            | "transpose"
            | "relu"
            | "sigmoid"
            | "tanh"
            | "exp"
            | "ln"
            | "sqrt"
            | "sin"
            | "cos"
            | "abs"
            | "max"
            | "min"
            | "shape"
            | "count"
            | "ndim"
    )
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
        "relu" | "sigmoid" | "tanh" | "exp" | "ln" | "sqrt" | "sin" | "cos" | "abs" => {
            no_method_args(name, args, line, col)?;
            unary_builtin(name, &Value::Node(n.clone()), line, col)
        }
        // The reductions a PLAIN tensor answers, on the tape (the sweep found
        // `variable(tensor).max()` stolen by the UFCS fallback into the 2-arg
        // scalar builtin, with an arity error misdescribing the program).
        // 0 args = the reduction — gradient 1 to the FIRST extreme element in
        // logical order, ties-to-first like the scalar pair; 1 arg = the
        // elementwise binary twin, exactly `max(v, other)`.
        "max" | "min" => {
            if args.len() == 1 {
                return binary_builtin(name, &Value::Node(n.clone()), &args[0], line, col);
            }
            no_method_args(name, args, line, col)?;
            if n.value.is_empty() {
                return Err(HelixError::new(
                    format!("cannot take the `{name}` of an empty tensor"),
                    line,
                    col,
                ));
            }
            let want_max = name == "max";
            let mut best: Option<(ndarray::IxDyn, f64)> = None;
            for (idx, &x) in n.value.indexed_iter() {
                let better = match &best {
                    None => true,
                    Some((_, b)) => {
                        if want_max {
                            x > *b
                        } else {
                            x < *b
                        }
                    }
                };
                if better {
                    best = Some((idx.clone(), x));
                }
            }
            let Some((at, x)) = best else {
                return Err(HelixError::new(
                    format!("cannot take the `{name}` of an empty tensor"),
                    line,
                    col,
                ));
            };
            let value = scalar(x);
            let shape = n.value.raw_dim();
            let out = make(
                value,
                vec![n.clone()],
                Box::new(move |g| {
                    let s = *g.first().unwrap_or(&0.0);
                    let mut grad = ArrayD::zeros(shape.clone());
                    grad[&at] = s;
                    vec![grad]
                }),
            );
            Ok(Value::Node(out))
        }
        // Metadata, not mathematics: how big a value is does not depend on the
        // tape, and answering "no differentiable method `shape`" would mean
        // `tensor([w1, w2]).shape()` failed where `tensor([1.0, 2.0]).shape()`
        // works — the same expression made illegal by a variable being inside it.
        // These read straight off the forward value, exactly as the plain twins do.
        "shape" => {
            no_method_args(name, args, line, col)?;
            Ok(Value::array(
                n.value.shape().iter().map(|&d| Value::Int(d as i64)).collect(),
            ))
        }
        "count" => {
            no_method_args(name, args, line, col)?;
            Ok(Value::Int(n.value.len() as i64))
        }
        "ndim" => {
            no_method_args(name, args, line, col)?;
            Ok(Value::Int(n.value.ndim() as i64))
        }
        _ => Err(HelixError::new(
            format!("a tracked value has no differentiable method `{name}`"),
            line,
            col,
        )
        .hint(
            "methods: matmul/dot, sum, mean, t/transpose, relu, sigmoid, tanh, exp, ln, \
             sqrt, sin, cos, abs; shape/count/ndim read the value. Any free builtin also \
             chains — `v.tan()` means `tan(v)`.",
        )),
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
            format!("`variable` needs a number or tensor, got {}", crate::value::with_article(v.type_name())),
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
///
/// A leaf the most recent backward pass never reached has gradient ZERO with respect
/// to that loss — its `grad` cell may still hold an accumulation from an earlier
/// tape (`run_backward` zeroes only nodes reachable from the root), and answering
/// with it is the training-loop footgun the stabilization sweep pinned:
/// `gradient(x*x, y)` reporting y's gradient from a previous loss.
fn grad_of(x: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    let current = EPOCH.with(|e| e.get());
    match x {
        Value::Node(n) if n.epoch.get() != current => {
            Ok(arr_to_value(ArrayD::zeros(n.value.raw_dim())))
        }
        Value::Node(n) => Ok(arr_to_value(n.grad.borrow().clone())),
        Value::Array(items) => {
            let out: Result<Vec<Value>, HelixError> =
                items.to_values().iter().map(|e| grad_of(e, line, col)).collect();
            Ok(Value::array(out?))
        }
        other => Err(HelixError::new(
            format!("`gradient` needs a tracked variable (or array of them), got {}", crate::value::with_article(other.type_name())),
            line,
            col,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    /// Every name `differentiable_builtin` flags REALLY differentiates: build a
    /// leaf, apply the op, and demand a graph node back — the describe flag can
    /// never claim an op the tape refuses.
    #[test]
    fn the_differentiable_flag_is_honest() {
        let leaf = |x: f64| variable(&Value::Float(x), 0, 0).expect("leaf");
        for name in [
            "relu", "sigmoid", "tanh", "exp", "ln", "sqrt", "sin", "cos", "abs", "tan", "asin",
            "acos", "atan", "sinh", "cosh", "log2", "log10", "cbrt", "degrees", "radians", "erf",
            "normal_cdf", "normal_pdf",
        ] {
            assert!(differentiable_builtin(name), "{name} missing from the flag");
            let out = unary_builtin(name, &leaf(0.5), 0, 0);
            assert!(matches!(out, Ok(Value::Node(_))), "{name} refused a tracked value");
        }
        for name in ["max", "min", "hypot"] {
            assert!(differentiable_builtin(name), "{name} missing from the flag");
            let out = binary_builtin(name, &leaf(0.5), &Value::Float(1.0), 0, 0);
            assert!(matches!(out, Ok(Value::Node(_))), "{name} refused a tracked value");
        }
        assert!(differentiable_builtin("clamp"), "clamp is min(max(x, lo), hi) on the tape");
        assert!(!differentiable_builtin("floor"), "floor must stay refused (zero derivative)");
    }
}

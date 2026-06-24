//! Bytecode compiler — Stage 1 of the performance roadmap.
//!
//! Lowers the AST into a flat, stack-based instruction stream (think Wasm/JVM)
//! that `vm.rs` executes in a tight dispatch loop. Two structural wins over the
//! tree-walker:
//!
//!   * **Variables become slot indices**, resolved here at compile time — no
//!     per-access hashing of a `String` in a `FxHashMap`, no insert/remove
//!     churn on every function call.
//!   * **The AST is walked once**, not re-traversed on every execution.
//!
//! It is deliberately *partial*: anything it doesn't yet know how to compile
//! (arrays, methods/comprehensions, records, tensors, DataFrames, lambdas, …)
//! makes [`compile`] return [`Unsupported`], and the caller transparently falls
//! back to the tree-walker. So the VM accelerates the scalar / control-flow /
//! recursion core — exactly where interpreter overhead dominates — while every
//! program keeps running. Opcodes are added over time to widen its reach.
//!
//! Semantics are kept identical to the tree-walker by reusing its value type and
//! its arithmetic/boolean helpers (`interp::eval_binary`, `eval_unary`, …), so
//! the VM can never silently disagree with the reference interpreter.

use std::collections::HashSet;

use crate::ast::{BinOp, Expr, InterpPart, Stmt, UnOp};
use crate::interp::BUILTIN_FNS;
use crate::value::Value;

/// Builtins with side effects (output) or non-reproducible results (IO). A
/// function that reaches any of these — directly or through another function —
/// is impure and must never be memoized.
const IMPURE_BUILTINS: &[&str] = &[
    "print",
    "read_csv",
    "read_parquet",
    "read_fasta",
    "write_parquet",
];

/// Sentinel: this program uses a construct the compiler doesn't support yet, so
/// the caller should run it on the tree-walker instead.
#[derive(Debug)]
pub struct Unsupported;

/// Which comprehension a `CompInit` begins (drives the collect behaviour and the
/// "no such method" error wording).
#[derive(Debug, Clone, Copy)]
pub enum CompKind {
    Map,
    Filter,
    Reduce,
    Any,
    All,
}

impl CompKind {
    pub fn method_name(self) -> &'static str {
        match self {
            CompKind::Map => "map",
            CompKind::Filter => "filter",
            CompKind::Reduce => "reduce",
            CompKind::Any => "any",
            CompKind::All => "all",
        }
    }
}

type R<T> = Result<T, Unsupported>;

/// A single stack-machine instruction. Operand and local references are
/// pre-resolved `u32` indices. Each instruction has a parallel source position
/// in [`Chunk::pos`] for runtime error reporting.
#[derive(Debug, Clone)]
pub enum Op {
    /// Push a literal from the chunk's constant pool.
    Const(u32),
    /// Push a copy of a frame-local slot (function param or `let` binding).
    LoadLocal(u32),
    /// Pop and store into a frame-local slot.
    StoreLocal(u32),
    /// Push a copy of a global (top-level binding, or a seeded constant).
    LoadGlobal(u32),
    /// Pop and store into a global slot.
    StoreGlobal(u32),
    /// Pop one, push the unary result.
    Unary(UnOp),
    /// Pop two (b then a), push `a op b`. Never used for And/Or/Coalesce — those
    /// short-circuit via the dedicated ops below.
    Binary(BinOp),
    /// Superinstruction: push `locals[a] op locals[b]` (fuses `LoadLocal a;
    /// LoadLocal b; Binary op`).
    LoadLocalBinary(u32, u32, BinOp),
    /// Superinstruction: push `locals[a] op consts[k]` (fuses `LoadLocal a;
    /// Const k; Binary op`).
    LoadLocalConstBinary(u32, u32, BinOp),
    /// Superinstruction: pop `v`, push `v op consts[k]` (fuses `Const k; Binary op`).
    ConstBinary(u32, BinOp),
    /// Unconditional jump to an instruction index.
    Jump(u32),
    /// Pop a boolean condition; jump if it is `false`. Used only for `if`, so it
    /// owns the "`if` condition is `missing`" / non-boolean error wording.
    JumpIfFalse(u32),
    /// Three-valued `and` short-circuit. Peeks the left value: if it's a
    /// determined `false`, replaces it with `Bool(false)` and jumps to the end;
    /// otherwise leaves it and falls through to the right operand.
    AndCheck(u32),
    /// Combine the two `and` operands left on the stack into the final value.
    AndCombine,
    /// Three-valued `or` short-circuit (mirror of [`Op::AndCheck`]).
    OrCheck(u32),
    OrCombine,
    /// `a ?? b`: if the left value is `missing`, drop it and fall through to `b`;
    /// otherwise keep it and jump to the end.
    CoalesceCheck(u32),
    /// Call user function `funcs[idx]` with `nargs` args from the stack top.
    CallFn { idx: u32, nargs: u32 },
    /// Push a first-class function value referencing `funcs[idx]` (from a lambda
    /// or a bare function-name used as a value).
    MakeFunc { idx: u32, arity: u32 },
    /// Call a function *value*: the stack holds `[func, arg0..argN-1]` (the value
    /// was loaded before the args). Reads the callee chunk from the value; errors
    /// (using the call-site `name`) if it isn't a function or the arity is wrong.
    CallValue { nargs: u32, name: std::rc::Rc<String> },
    /// Call builtin `builtins[idx]` with `nargs` args from the stack top.
    CallBuiltin { idx: u32, nargs: u32 },
    /// Pop `n` values and build an array from them (in push order).
    MakeArray(u32),
    /// Pop an index then a receiver; push `recv[index]`.
    Index,
    /// Build an interpolated string. The template's `Expr` holes consume the
    /// values pushed for them (in order); `Lit` parts are inlined.
    Interp(std::rc::Rc<Vec<InterpPart>>),
    /// Pop `n` values and build a tuple from them (in push order).
    MakeTuple(u32),
    /// Pop `names.len()` values and pair them with these field names → a record.
    MakeRecord(std::rc::Rc<Vec<String>>),
    /// Pop a receiver; push `recv.<name>` (record field access).
    GetField(std::rc::Rc<String>),
    /// Slice a receiver. The bitmask says which of start/stop/step were supplied
    /// (bit 0/1/2); those bound values were pushed after the receiver, in order.
    Slice(u8),
    /// Pop a tuple/array and store its elements into the given global slots
    /// (`a, b = pair`).
    Destructure(std::rc::Rc<Vec<u32>>),
    /// Pop a tuple/array element and store its parts into the given *local* slots
    /// — a comprehension's multi-binder pattern (`xs.map((a, b) => ...)`). Raises
    /// the same "cannot destructure …"/"lambda expects N values …" errors as the
    /// tree-walker's `eval_with_pattern` when the element isn't a tuple/array of
    /// the right arity.
    DestructureBind(std::rc::Rc<Vec<u32>>),
    /// Pop `nargs` evaluated args and a receiver; dispatch the value-method
    /// `name` at runtime by receiver type.
    Method(std::rc::Rc<String>, u32),
    /// Pop a DataFrame receiver and apply a column-verb (`where`/`filter`/`select`/
    /// `sort`/`group`) whose `args` are *unevaluated* column/predicate ASTs. Bare
    /// names resolve against the frame's columns first, then `locals` (by slot),
    /// then globals. Emitted only when the type checker proved the receiver is a
    /// DataFrame, so the args are genuinely columns, not values.
    DfColumnVerb {
        name: std::rc::Rc<String>,
        args: std::rc::Rc<Vec<Expr>>,
        locals: std::rc::Rc<Vec<(String, u32)>>,
    },
    /// Pop a GroupBy receiver and apply an aggregation over one *unevaluated*
    /// column (`mean`/`sum`/`min`/`max`/`count`/`std`). Emitted only when the type
    /// checker proved the receiver is a GroupBy.
    GroupByAgg {
        name: std::rc::Rc<String>,
        args: std::rc::Rc<Vec<Expr>>,
    },
    /// Begin a comprehension over the popped receiver. If it's an array, push an
    /// iterator; if `missing`, jump to the given target (the result is `missing`);
    /// otherwise raise "no such method".
    CompInit(CompKind, u32),
    /// Advance the current iterator: if elements remain, bind the next one to the
    /// given local slot and fall through; otherwise jump to the given target.
    CompNext(u32, u32),
    /// `map`: pop the body result and append it to the iterator's builder.
    CompMapPush,
    /// `filter`/`where`: pop a boolean; if true, append the current element.
    CompFilterPush,
    /// Finish a `map`/`filter`: pop the iterator and push its built array.
    CompEnd,
    /// Finish a `reduce`: pop the iterator (its result, the accumulator, is loaded
    /// separately).
    CompEndDiscard,
    /// `any`/`all` per-element test. Pops a boolean: short-circuits to the target
    /// on a determining result (`true` for `any`, `false` for `all`); a `missing`
    /// sets the seen-missing slot; a non-boolean errors. Fields: (is_all, sm_slot,
    /// short_target).
    CompBoolTest(bool, u32, u32),
    /// Begin a comprehension over a fused `range`: pop `[start, end]`, validate as
    /// integers and cap-check (identically to a materialized `range`), then push a
    /// lazy range iterator — no array is allocated. `CompNext` drives it like any
    /// other iterator.
    CompInitRange,
    /// Fast path for a JIT-eligible `range(start,end).reduce(init, (acc,x)=>body)`.
    /// At this point `[start, end]` are on the stack (top is `end`) and `acc` (the
    /// local at `acc_slot`) already holds `init`. If a native loop for `loop_idx`
    /// exists AND `start`/`end`/`init` are all `Int` within the 100M cap, the VM
    /// pops `[start,end]`, runs the native loop, writes the result into `acc_slot`,
    /// and jumps to `after` (the trailing `LoadLocal(acc)`), skipping the bytecode
    /// loop. Otherwise it falls through to the identical `CompInitRange` loop — so
    /// every non-`Int`, over-cap, or no-JIT case takes the oracle-matched path.
    TryJitReduce { loop_idx: u32, acc_slot: u32, after: u32 },
    /// Raise a runtime error with the given message and hint. Used where the
    /// program is statically known to be an error but the error should still fire
    /// at the point of execution (e.g. reassigning an immutable global, after its
    /// value expression — and any side effects — have run).
    Raise(std::rc::Rc<String>, std::rc::Rc<String>),
    /// Discard the top of the stack (an expression-statement's value).
    Pop,
    /// Return the top of the stack from the current function frame.
    Return,
}

/// A JIT-eligible `reduce` loop body the compiler asked the JIT to compile. The
/// JIT lowers each to a native `extern "C" fn(i64 start, i64 end, i64 init)->i64`;
/// the index into [`Program::reduce_loops`] is the `loop_idx` in [`Op::TryJitReduce`].
#[derive(Debug, Clone)]
pub struct ReduceLoop {
    /// The accumulator binder name (lambda param 0).
    pub pa: String,
    /// The element/counter binder name (lambda param 1).
    pub pb: String,
    /// The reduce body expression, evaluated over `{pa, pb}` as `i64`.
    pub body: Expr,
}

/// One compiled code unit: a function body or the top-level `main`.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub code: Vec<Op>,
    pub consts: Vec<Value>,
    pub pos: Vec<(usize, usize)>,
    pub n_params: u32,
    /// Total local slots to reserve in a frame (params + every `let` binding).
    pub n_locals: u32,
}

/// A fully compiled program ready for the VM. `funcs[0]` is `main`; `funcs[1..]`
/// are user functions, indexed by [`Op::CallFn`].
#[derive(Debug)]
pub struct Program {
    pub funcs: Vec<Chunk>,
    pub func_names: Vec<String>,
    pub builtins: Vec<String>,
    /// Initial value for every global slot: real values for the pre-seeded
    /// constants (`pi`, `e`, `inf`), `Unit` for user globals (written before
    /// use). Its length *is* the global count.
    pub global_init: Vec<Value>,
    /// Per-function flag (aligned with `funcs`): may this function be memoized?
    /// True only for pure, mutable-global-free functions with overlapping
    /// recursion. The VM additionally gates on all-`Int` arguments at call time.
    pub memoizable: Vec<bool>,
    /// JIT-eligible `reduce` loop bodies, indexed by [`Op::TryJitReduce::loop_idx`].
    /// Handed to [`crate::jit::build`] so the native loop and the bytecode agree on
    /// which sites are eligible — a single source of truth, no two-pass coupling.
    pub reduce_loops: Vec<ReduceLoop>,
    /// Global slot names, aligned with `global_init`. Lets the VM resolve a bare
    /// name to a global's runtime value inside a DataFrame predicate
    /// (`df.where(age > threshold)`) — the column-verb `resolve_var`.
    pub global_names: std::rc::Rc<Vec<String>>,
}

/// How an identifier resolves at compile time.
enum NameRef {
    Local(u32),
    Global(u32),
    /// A user function (callable, but not usable as a first-class value yet).
    Func(u32),
}

/// Per-function code being built.
struct Builder {
    code: Vec<Op>,
    consts: Vec<Value>,
    pos: Vec<(usize, usize)>,
    /// Lexical scopes of locals; each entry maps a name to its slot.
    scopes: Vec<Vec<(String, u32)>>,
    next_slot: u32,
    max_slot: u32,
}

impl Builder {
    fn new() -> Self {
        Builder {
            code: Vec::new(),
            consts: Vec::new(),
            pos: Vec::new(),
            scopes: vec![Vec::new()],
            next_slot: 0,
            max_slot: 0,
        }
    }

    fn emit(&mut self, op: Op, line: usize, col: usize) -> usize {
        let at = self.code.len();
        self.code.push(op);
        self.pos.push((line, col));
        at
    }

    fn add_const(&mut self, v: Value) -> u32 {
        let idx = self.consts.len() as u32;
        self.consts.push(v);
        idx
    }

    fn resolve_local(&self, name: &str) -> Option<u32> {
        // Inner scopes shadow outer ones.
        for scope in self.scopes.iter().rev() {
            for (n, slot) in scope.iter().rev() {
                if n == name {
                    return Some(*slot);
                }
            }
        }
        None
    }

    fn declare_local(&mut self, name: &str) -> u32 {
        let slot = self.next_slot;
        self.next_slot += 1;
        if self.next_slot > self.max_slot {
            self.max_slot = self.next_slot;
        }
        self.scopes.last_mut().unwrap().push((name.to_string(), slot));
        slot
    }

    /// Every local name currently in scope paired with its slot, so a DataFrame
    /// predicate can resolve a bare name to a local variable (matching the
    /// tree-walker's env). Outer scopes first; the VM's `resolve_var` takes the
    /// last (innermost) match on a duplicate name.
    fn in_scope_locals(&self) -> Vec<(String, u32)> {
        let mut out: Vec<(String, u32)> = Vec::new();
        for scope in &self.scopes {
            for (n, slot) in scope {
                out.push((n.clone(), *slot));
            }
        }
        out
    }
}

/// Compiler-global state shared across `main` and every function.
pub struct Compiler {
    globals: Vec<String>,
    global_mut: Vec<bool>,
    global_init: Vec<Value>,
    func_names: Vec<String>,
    /// Arity of each entry in `func_names`/`funcs` (param count), for building
    /// first-class function values (`MakeFunc`). Aligned with `func_names`.
    func_arity: Vec<u32>,
    funcs: Vec<Option<Chunk>>,
    builtins: Vec<String>,
    /// Accumulated JIT reduce-loop requests (see [`Program::reduce_loops`]).
    reduce_loops: Vec<ReduceLoop>,
    /// Inferred receiver types from the type checker (see [`crate::types::TypeMap`]),
    /// used to route receiver-polymorphic methods. `None` when compiling without a
    /// prior type-check (tests/fuzzers) — then such methods fall back as before.
    types: Option<crate::types::TypeMap>,
}

/// Compile a whole program to bytecode, or return [`Unsupported`] so the caller
/// falls back to the tree-walker. Never partially compiles: it's all-or-nothing
/// per program, which keeps the VM and the fallback path cleanly separated.
/// Compile, optionally using the type checker's inferred receiver types to route
/// receiver-polymorphic methods (DataFrame/Tensor column-verbs). Pass `None` to
/// compile without a prior type-check (tests/fuzzers) — then such methods fall
/// back as before.
pub fn compile_with_types(program: &[Stmt], types: Option<crate::types::TypeMap>) -> R<Program> {
    let mut c = Compiler {
        // Seed the math constants as immutable globals so scalar programs that
        // use `pi`/`e`/`inf` still compile (the tree-walker predefines them).
        globals: vec!["pi".into(), "e".into(), "inf".into()],
        global_mut: vec![false, false, false],
        global_init: vec![
            Value::Float(std::f64::consts::PI),
            Value::Float(std::f64::consts::E),
            Value::Float(f64::INFINITY),
        ],
        func_names: vec!["<main>".into()],
        func_arity: vec![0], // main takes no params
        funcs: vec![None], // slot 0 reserved for main
        builtins: Vec::new(),
        reduce_loops: Vec::new(),
        types,
    };

    let mut main = Builder::new();
    for stmt in program {
        c.compile_stmt(&mut main, stmt)?;
    }

    let main_chunk = Chunk {
        code: main.code,
        consts: main.consts,
        pos: main.pos,
        n_params: 0,
        n_locals: main.max_slot,
    };
    c.funcs[0] = Some(main_chunk);

    let funcs: Vec<Chunk> = c.funcs.into_iter().map(|f| f.unwrap()).collect();
    let memo_set = memoizable_fns(program);
    let memoizable: Vec<bool> = c
        .func_names
        .iter()
        .map(|n| memo_set.contains(n.as_str()))
        .collect();
    Ok(Program {
        funcs,
        func_names: c.func_names,
        builtins: c.builtins,
        global_init: c.global_init,
        memoizable,
        reduce_loops: c.reduce_loops,
        global_names: std::rc::Rc::new(c.globals),
    })
}

/// The names of functions that are **safe and worthwhile to memoize**:
///   * **pure** — never reaches `print`/`read_*`/`write_*` (transitively), so a
///     cache hit can't skip a side effect;
///   * **reads no mutable global** — so the result is a function of its arguments
///     alone (immutable globals like `pi` are fine; they never change);
///   * **overlapping recursion** — at least two self-calls in the body, the
///     signature of exponential redundancy (linear recursion stays on the JIT,
///     where one fast native call per step beats a cached bytecode step).
///
/// This is the static half of the automatic "under the hood" cache; the VM gates
/// on all-`Int` arguments at runtime (float keys are excluded — NaN/precision).
pub fn memoizable_fns(program: &[Stmt]) -> HashSet<String> {
    let funcs: Vec<(&str, &[(String, Option<crate::ast::TypeAnn>)], &Expr)> = program
        .iter()
        .filter_map(|s| match s {
            Stmt::Func { name, params, body, .. } => Some((name.as_str(), params.as_slice(), body)),
            _ => None,
        })
        .collect();

    // Names of mutable top-level bindings.
    let mut mutable: HashSet<&str> = HashSet::new();
    for s in program {
        match s {
            Stmt::Assign { name, mutable: true, .. } => {
                mutable.insert(name.as_str());
            }
            Stmt::Destructure { names, mutable: true, .. } => {
                for n in names {
                    mutable.insert(n.as_str());
                }
            }
            _ => {}
        }
    }

    // Purity fixpoint: impure if it reaches an impure builtin, an impure user fn,
    // or *any method call* — methods are assumed potentially side-effecting
    // (fail-closed), so the analysis stays sound even as the VM widens to compile
    // method calls rather than relying on the compiler rejecting them today.
    let mut impure: HashSet<&str> = HashSet::new();
    loop {
        let mut changed = false;
        for &(name, _, body) in &funcs {
            if !impure.contains(name)
                && (has_method(body)
                    || any_call(body, &|n| IMPURE_BUILTINS.contains(&n) || impure.contains(n)))
            {
                impure.insert(name);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Reads-a-mutable-global, **transitively** (its own fixpoint, mirroring
    // purity): a function that reaches a mutable global through a callee is not a
    // function of its arguments alone, so it must not be memoized.
    let mut reads_mut: HashSet<&str> = HashSet::new();
    for &(name, params, body) in &funcs {
        let bound: HashSet<&str> = params.iter().map(|(p, _)| p.as_str()).collect();
        if reads_mutable(body, &bound, &mutable) {
            reads_mut.insert(name);
        }
    }
    loop {
        let snapshot = reads_mut.clone();
        let mut changed = false;
        for &(name, _, body) in &funcs {
            if !snapshot.contains(name) && any_call(body, &|n| snapshot.contains(n)) {
                reads_mut.insert(name);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut result = HashSet::new();
    for &(name, _, body) in &funcs {
        if !impure.contains(name)
            && !reads_mut.contains(name)
            && count_self_calls(body, name) >= 2
        {
            result.insert(name.to_string());
        }
    }
    result
}

/// True if any method call appears anywhere in the tree.
fn has_method(e: &Expr) -> bool {
    matches!(e, Expr::Method { .. }) || children(e).into_iter().any(has_method)
}

/// True if any free-function call in the tree satisfies `pred` on its name.
fn any_call(e: &Expr, pred: &dyn Fn(&str) -> bool) -> bool {
    match e {
        Expr::Call { name, args, .. } => {
            pred(name) || args.iter().any(|a| any_call(a, pred))
        }
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing
        | Expr::Ident { .. } => false,
        Expr::Interp(parts) => parts
            .iter()
            .any(|p| matches!(p, InterpPart::Expr(e) if any_call(e, pred))),
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().any(|x| any_call(x, pred)),
        Expr::Record(fs) => fs.iter().any(|(_, v)| any_call(v, pred)),
        Expr::Field { recv, .. } => any_call(recv, pred),
        Expr::Unary { expr, .. } => any_call(expr, pred),
        Expr::Binary { left, right, .. } => any_call(left, pred) || any_call(right, pred),
        Expr::Method { recv, args, .. } => {
            any_call(recv, pred) || args.iter().any(|a| any_call(a, pred))
        }
        Expr::Index { recv, index, .. } => any_call(recv, pred) || any_call(index, pred),
        Expr::Slice { recv, start, stop, step, .. } => {
            any_call(recv, pred)
                || [start, stop, step]
                    .iter()
                    .any(|o| o.as_ref().is_some_and(|x| any_call(x, pred)))
        }
        Expr::Lambda { body, .. } => any_call(body, pred),
        Expr::Let { bindings, body } => {
            bindings.iter().any(|(_, v)| any_call(v, pred)) || any_call(body, pred)
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            any_call(cond, pred) || any_call(then_branch, pred) || any_call(else_branch, pred)
        }
    }
}

/// Count direct recursive calls to `name` in the body.
fn count_self_calls(e: &Expr, name: &str) -> usize {
    let mut n = 0;
    count_self_calls_into(e, name, &mut n);
    n
}

fn count_self_calls_into(e: &Expr, name: &str, n: &mut usize) {
    if let Expr::Call { name: callee, .. } = e {
        if callee == name {
            *n += 1;
        }
    }
    for child in children(e) {
        count_self_calls_into(child, name, n);
    }
}

/// True if the expression reads a mutable global that isn't shadowed by a
/// parameter or `let` binding (which would make it not a pure function of args).
fn reads_mutable(e: &Expr, bound: &HashSet<&str>, mutable: &HashSet<&str>) -> bool {
    match e {
        Expr::Ident { name, .. } => {
            mutable.contains(name.as_str()) && !bound.contains(name.as_str())
        }
        Expr::Lambda { params, body } => {
            let mut b = bound.clone();
            for p in params {
                b.insert(p.as_str());
            }
            reads_mutable(body, &b, mutable)
        }
        Expr::Let { bindings, body } => {
            let mut b = bound.clone();
            for (n, v) in bindings {
                if reads_mutable(v, &b, mutable) {
                    return true;
                }
                b.insert(n.as_str());
            }
            reads_mutable(body, &b, mutable)
        }
        _ => children(e).into_iter().any(|c| reads_mutable(c, bound, mutable)),
    }
}

/// The immediate child expressions of a node (for generic traversal). `Lambda`
/// and `Let` are handled by their callers because they introduce bindings.
fn children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing
        | Expr::Ident { .. } => vec![],
        Expr::Interp(parts) => parts
            .iter()
            .filter_map(|p| match p {
                InterpPart::Expr(e) => Some(&**e),
                _ => None,
            })
            .collect(),
        Expr::Array(xs) | Expr::Tuple(xs) => xs.iter().collect(),
        Expr::Record(fs) => fs.iter().map(|(_, v)| v).collect(),
        Expr::Field { recv, .. } => vec![recv],
        Expr::Unary { expr, .. } => vec![expr],
        Expr::Binary { left, right, .. } => vec![left, right],
        Expr::Call { args, .. } => args.iter().collect(),
        Expr::Method { recv, args, .. } => {
            let mut v = vec![&**recv];
            v.extend(args.iter());
            v
        }
        Expr::Index { recv, index, .. } => vec![recv, index],
        Expr::Slice { recv, start, stop, step, .. } => {
            let mut v = vec![&**recv];
            for o in [start, stop, step].into_iter().flatten() {
                v.push(o);
            }
            v
        }
        Expr::Lambda { body, .. } => vec![body],
        Expr::Let { bindings, body } => {
            let mut v: Vec<&Expr> = bindings.iter().map(|(_, e)| e).collect();
            v.push(body);
            v
        }
        Expr::If { cond, then_branch, else_branch, .. } => vec![cond, then_branch, else_branch],
    }
}

/// If `e` is a `range(...)` call with 1 or 2 arguments, return its `(start, end)`
/// where a 1-argument range has `None` start (i.e. 0). Enables range fusion.
fn as_range_call(e: &Expr) -> Option<(Option<&Expr>, &Expr)> {
    if let Expr::Call { name, args, .. } = e {
        if name == "range" {
            return match args.len() {
                1 => Some((None, &args[0])),
                2 => Some((Some(&args[0]), &args[1])),
                _ => None,
            };
        }
    }
    None
}

impl Compiler {
    /// The type checker's inferred type for a method receiver expression, if a
    /// type-check ran. Keyed by the receiver's node address (stable: the AST is
    /// not cloned between `types::check` and here).
    fn recv_type(&self, recv: &Expr) -> Option<&crate::types::Type> {
        self.types.as_ref().and_then(|m| m.get(&(recv as *const Expr)))
    }

    /// Compile `recv` (for its side effects — the tree-walker evaluates the receiver
    /// before validating a malformed method call), then emit a runtime [`Op::Raise`].
    /// Keeps `compile` total for malformed comprehensions/reductions that the
    /// (deliberately permissive) type checker leaves to a runtime error.
    fn raise_after_recv(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        msg: String,
        hint: String,
        line: usize,
        col: usize,
    ) -> R<()> {
        self.compile_expr(b, recv)?;
        b.emit(Op::Raise(std::rc::Rc::new(msg), std::rc::Rc::new(hint)), line, col);
        Ok(())
    }

    fn builtin_idx(&mut self, name: &str) -> u32 {
        if let Some(i) = self.builtins.iter().position(|b| b == name) {
            return i as u32;
        }
        let i = self.builtins.len() as u32;
        self.builtins.push(name.to_string());
        i
    }

    fn resolve(&self, b: &Builder, name: &str) -> Option<NameRef> {
        if let Some(slot) = b.resolve_local(name) {
            return Some(NameRef::Local(slot));
        }
        if let Some(i) = self.globals.iter().position(|g| g == name) {
            return Some(NameRef::Global(i as u32));
        }
        if let Some(i) = self.func_names.iter().position(|f| f == name) {
            return Some(NameRef::Func(i as u32));
        }
        None
    }

    fn compile_stmt(&mut self, b: &mut Builder, stmt: &Stmt) -> R<()> {
        match stmt {
            Stmt::Assign { name, mutable, value, line, col } => {
                self.compile_expr(b, value)?;
                // Top-level assignments are globals, matching the tree-walker's
                // `assign`: `mut x = …` always (re)declares as mutable; a plain
                // `x = …` reassigns a mutable global, but reassigning an *immutable*
                // global is an error raised at this point (the value above has
                // already been evaluated, so its side effects still happen).
                if let Some(i) = self.globals.iter().position(|g| g == name) {
                    if *mutable {
                        self.global_mut[i] = true; // `mut x = …` re-declares as mutable
                        b.emit(Op::StoreGlobal(i as u32), *line, *col);
                    } else if self.global_mut[i] {
                        b.emit(Op::StoreGlobal(i as u32), *line, *col);
                    } else {
                        b.emit(
                            Op::Raise(
                                std::rc::Rc::new(format!(
                                    "`{}` is immutable and cannot be reassigned",
                                    name
                                )),
                                std::rc::Rc::new(format!(
                                    "declare it as mutable up front with `mut {} = ...` if it needs to change.",
                                    name
                                )),
                            ),
                            *line,
                            *col,
                        );
                    }
                } else {
                    let i = self.globals.len() as u32;
                    self.globals.push(name.clone());
                    self.global_mut.push(*mutable);
                    self.global_init.push(Value::Unit);
                    b.emit(Op::StoreGlobal(i), *line, *col);
                }
                Ok(())
            }
            Stmt::Func { name, params, body, .. } => self.compile_func(name, params, body),
            Stmt::Expr(e) => {
                self.compile_expr(b, e)?;
                b.emit(Op::Pop, 0, 0);
                Ok(())
            }
            Stmt::Destructure { names, mutable, value, line, col } => {
                self.compile_expr(b, value)?;
                // Same mutability rule as `Assign`: `mut a, b = …` (re)declares each
                // as mutable; a plain destructure reassigning an *immutable* global
                // is an error. (The tree-walker checks arity first, then mutability;
                // an arity mismatch *and* an immutable target is a rare error-on-error
                // edge where the message may differ — both still reject.)
                if !*mutable {
                    for name in names {
                        if let Some(i) = self.globals.iter().position(|g| g == name) {
                            if !self.global_mut[i] {
                                b.emit(
                                    Op::Raise(
                                        std::rc::Rc::new(format!(
                                            "`{}` is immutable and cannot be reassigned",
                                            name
                                        )),
                                        std::rc::Rc::new(format!(
                                            "declare it as mutable up front with `mut {} = ...` if it needs to change.",
                                            name
                                        )),
                                    ),
                                    *line,
                                    *col,
                                );
                                return Ok(());
                            }
                        }
                    }
                }
                let mut slots: Vec<u32> = Vec::with_capacity(names.len());
                for name in names {
                    if let Some(i) = self.globals.iter().position(|g| g == name) {
                        if *mutable {
                            self.global_mut[i] = true; // `mut …` re-declares as mutable
                        }
                        slots.push(i as u32);
                    } else {
                        let i = self.globals.len() as u32;
                        self.globals.push(name.clone());
                        self.global_mut.push(*mutable);
                        self.global_init.push(Value::Unit);
                        slots.push(i);
                    }
                }
                b.emit(Op::Destructure(std::rc::Rc::new(slots)), *line, *col);
                Ok(())
            }
        }
    }

    fn compile_func(
        &mut self,
        name: &str,
        params: &[(String, Option<crate::ast::TypeAnn>)],
        body: &Expr,
    ) -> R<()> {
        // Reserve the function's index *before* compiling its body so recursive
        // self-calls resolve.
        let idx = self.funcs.len();
        self.func_names.push(name.to_string());
        self.func_arity.push(params.len() as u32);
        self.funcs.push(None);

        let mut fb = Builder::new();
        for (pname, _) in params {
            fb.declare_local(pname);
        }
        self.compile_expr(&mut fb, body)?;
        fb.emit(Op::Return, 0, 0);

        let chunk = Chunk {
            code: fb.code,
            consts: fb.consts,
            pos: fb.pos,
            n_params: params.len() as u32,
            n_locals: fb.max_slot,
        };
        self.funcs[idx] = Some(chunk);
        Ok(())
    }

    /// Compile an anonymous lambda body into its own chunk (like [`Self::compile_func`]
    /// but nameless) and return its function-table index. Free variables resolve to
    /// globals during body compilation — matching the tree-walker, which has no
    /// captured environment (the type checker rejects local capture).
    fn compile_lambda(&mut self, params: &[String], body: &Expr) -> R<u32> {
        let idx = self.funcs.len() as u32;
        self.func_names.push("<lambda>".to_string());
        self.func_arity.push(params.len() as u32);
        self.funcs.push(None);

        let mut fb = Builder::new();
        for p in params {
            fb.declare_local(p);
        }
        self.compile_expr(&mut fb, body)?;
        fb.emit(Op::Return, 0, 0);

        let chunk = Chunk {
            code: fb.code,
            consts: fb.consts,
            pos: fb.pos,
            n_params: params.len() as u32,
            n_locals: fb.max_slot,
        };
        self.funcs[idx as usize] = Some(chunk);
        Ok(idx)
    }

    fn compile_expr(&mut self, b: &mut Builder, e: &Expr) -> R<()> {
        match e {
            Expr::Int(i) => {
                let k = b.add_const(Value::Int(*i));
                b.emit(Op::Const(k), 0, 0);
            }
            Expr::Float(f) => {
                let k = b.add_const(Value::Float(*f));
                b.emit(Op::Const(k), 0, 0);
            }
            Expr::Str(s) => {
                let k = b.add_const(Value::Str(std::rc::Rc::new(s.clone())));
                b.emit(Op::Const(k), 0, 0);
            }
            Expr::Bool(v) => {
                let k = b.add_const(Value::Bool(*v));
                b.emit(Op::Const(k), 0, 0);
            }
            Expr::Missing => {
                let k = b.add_const(Value::Missing);
                b.emit(Op::Const(k), 0, 0);
            }
            Expr::Ident { name, line, col } => match self.resolve(b, name) {
                Some(NameRef::Local(slot)) => {
                    b.emit(Op::LoadLocal(slot), *line, *col);
                }
                Some(NameRef::Global(i)) => {
                    b.emit(Op::LoadGlobal(i), *line, *col);
                }
                // A bare function name used as a value → a first-class function value.
                Some(NameRef::Func(idx)) => {
                    let arity = self.func_arity[idx as usize];
                    b.emit(Op::MakeFunc { idx, arity }, *line, *col);
                }
                // An undefined name. The type checker rejects this before compile, so
                // it is unreachable in the normal pipeline; emit a runtime error
                // (rather than `Unsupported`) so `compile` is total.
                None => {
                    b.emit(
                        Op::Raise(
                            std::rc::Rc::new(format!("`{}` is not defined", name)),
                            std::rc::Rc::new(format!("assign it first, e.g. `{} = ...`.", name)),
                        ),
                        *line,
                        *col,
                    );
                }
            },
            Expr::Unary { op, expr, line, col } => {
                self.compile_expr(b, expr)?;
                b.emit(Op::Unary(op.clone()), *line, *col);
            }
            Expr::Binary { op, left, right, line, col } => match op {
                BinOp::And => {
                    self.compile_expr(b, left)?;
                    let check = b.emit(Op::AndCheck(0), *line, *col);
                    self.compile_expr(b, right)?;
                    b.emit(Op::AndCombine, *line, *col);
                    let end = b.code.len() as u32;
                    b.code[check] = Op::AndCheck(end);
                }
                BinOp::Or => {
                    self.compile_expr(b, left)?;
                    let check = b.emit(Op::OrCheck(0), *line, *col);
                    self.compile_expr(b, right)?;
                    b.emit(Op::OrCombine, *line, *col);
                    let end = b.code.len() as u32;
                    b.code[check] = Op::OrCheck(end);
                }
                BinOp::Coalesce => {
                    self.compile_expr(b, left)?;
                    let check = b.emit(Op::CoalesceCheck(0), *line, *col);
                    self.compile_expr(b, right)?;
                    let end = b.code.len() as u32;
                    b.code[check] = Op::CoalesceCheck(end);
                }
                _ => {
                    // Superinstruction fusion. We collapse `<load left>; <load right>;
                    // Binary` into one op ONLY when each operand is a syntactically
                    // simple value (a literal or a bare identifier). Such operands
                    // compile to exactly one push with NO internal jump targets, so
                    // truncating + replacing them can never corrupt control flow.
                    //
                    // The danger we are avoiding: a complex operand (e.g. an `if`/`and`
                    // whose last emitted op is a literal else-branch) ends in a Const
                    // that is *also* a jump-target landing point. Blindly peepholing it
                    // would delete a live branch destination. Gating on AST simplicity
                    // sidesteps that entirely — simple operands have no inbound jumps.
                    fn simple(e: &Expr) -> bool {
                        matches!(
                            e,
                            Expr::Int(_)
                                | Expr::Float(_)
                                | Expr::Str(_)
                                | Expr::Bool(_)
                                | Expr::Ident { .. }
                        )
                    }
                    let left_simple = simple(left);
                    let right_simple = simple(right);
                    self.compile_expr(b, left)?;
                    let after_left = b.code.len();
                    self.compile_expr(b, right)?;
                    // Only the ops emitted for `right` (>= after_left) and the single
                    // op emitted for `left` (when simple) are candidates. We additionally
                    // confirm the emitted shape is LoadLocal/Const — a bare ident that
                    // resolved to a global/builtin would not be LoadLocal, so we skip it.
                    let n = b.code.len();
                    let right_one = n == after_left + 1;
                    let last = if n >= 1 { Some(b.code[n - 1].clone()) } else { None };
                    let prev = if n >= 2 { Some(b.code[n - 2].clone()) } else { None };
                    match (prev, last) {
                        (Some(Op::LoadLocal(a)), Some(Op::LoadLocal(c)))
                            if left_simple && right_simple && right_one =>
                        {
                            b.code.truncate(n - 2);
                            b.pos.truncate(n - 2);
                            b.emit(Op::LoadLocalBinary(a, c, op.clone()), *line, *col);
                        }
                        (Some(Op::LoadLocal(a)), Some(Op::Const(k)))
                            if left_simple && right_simple && right_one =>
                        {
                            b.code.truncate(n - 2);
                            b.pos.truncate(n - 2);
                            b.emit(Op::LoadLocalConstBinary(a, k, op.clone()), *line, *col);
                        }
                        (_, Some(Op::Const(k))) if right_simple && right_one => {
                            b.code.truncate(n - 1);
                            b.pos.truncate(n - 1);
                            b.emit(Op::ConstBinary(k, op.clone()), *line, *col);
                        }
                        _ => {
                            b.emit(Op::Binary(op.clone()), *line, *col);
                        }
                    }
                }
            },
            Expr::If { cond, then_branch, else_branch, line, col } => {
                self.compile_expr(b, cond)?;
                let jif = b.emit(Op::JumpIfFalse(0), *line, *col);
                self.compile_expr(b, then_branch)?;
                let jend = b.emit(Op::Jump(0), *line, *col);
                let else_at = b.code.len() as u32;
                b.code[jif] = Op::JumpIfFalse(else_at);
                self.compile_expr(b, else_branch)?;
                let end = b.code.len() as u32;
                b.code[jend] = Op::Jump(end);
            }
            Expr::Let { bindings, body } => {
                b.scopes.push(Vec::new());
                let saved_next = b.next_slot;
                for (name, expr) in bindings {
                    self.compile_expr(b, expr)?;
                    let slot = b.declare_local(name);
                    b.emit(Op::StoreLocal(slot), 0, 0);
                }
                self.compile_expr(b, body)?;
                b.scopes.pop();
                b.next_slot = saved_next;
            }
            Expr::Call { name, args, line, col } => {
                // Builtins win over user names, matching the tree-walker.
                if BUILTIN_FNS.contains(&name.as_str()) {
                    for a in args {
                        self.compile_expr(b, a)?;
                    }
                    let idx = self.builtin_idx(name);
                    b.emit(Op::CallBuiltin { idx, nargs: args.len() as u32 }, *line, *col);
                } else {
                    match self.resolve(b, name) {
                        Some(NameRef::Func(idx)) => {
                            for a in args {
                                self.compile_expr(b, a)?;
                            }
                            b.emit(Op::CallFn { idx, nargs: args.len() as u32 }, *line, *col);
                        }
                        // Calling a value-bound function: load the value, then the
                        // args, and dispatch on the value's chunk at runtime.
                        Some(NameRef::Global(i)) => {
                            b.emit(Op::LoadGlobal(i), *line, *col);
                            for a in args {
                                self.compile_expr(b, a)?;
                            }
                            b.emit(
                                Op::CallValue {
                                    nargs: args.len() as u32,
                                    name: std::rc::Rc::new(name.clone()),
                                },
                                *line,
                                *col,
                            );
                        }
                        Some(NameRef::Local(slot)) => {
                            b.emit(Op::LoadLocal(slot), *line, *col);
                            for a in args {
                                self.compile_expr(b, a)?;
                            }
                            b.emit(
                                Op::CallValue {
                                    nargs: args.len() as u32,
                                    name: std::rc::Rc::new(name.clone()),
                                },
                                *line,
                                *col,
                            );
                        }
                        // Unknown name. The type checker rejects this before compile
                        // (so it's unreachable normally); evaluate the args for their
                        // side effects, then raise — keeping `compile` total.
                        None => {
                            for a in args {
                                self.compile_expr(b, a)?;
                            }
                            b.emit(
                                Op::Raise(
                                    std::rc::Rc::new(format!("`{}` is not a known function", name)),
                                    std::rc::Rc::new(
                                        "only functions and the built-ins `print`/`dna`/`range` can be called.".to_string(),
                                    ),
                                ),
                                *line,
                                *col,
                            );
                        }
                    }
                }
            }
            Expr::Array(items) => {
                for item in items {
                    self.compile_expr(b, item)?;
                }
                b.emit(Op::MakeArray(items.len() as u32), 0, 0);
            }
            Expr::Index { recv, index, line, col } => {
                self.compile_expr(b, recv)?;
                self.compile_expr(b, index)?;
                b.emit(Op::Index, *line, *col);
            }
            Expr::Interp(parts) => {
                for part in parts {
                    if let InterpPart::Expr(e) = part {
                        self.compile_expr(b, e)?;
                    }
                }
                b.emit(Op::Interp(std::rc::Rc::new(parts.clone())), 0, 0);
            }
            Expr::Tuple(items) => {
                for item in items {
                    self.compile_expr(b, item)?;
                }
                b.emit(Op::MakeTuple(items.len() as u32), 0, 0);
            }
            Expr::Record(fields) => {
                for (_, v) in fields {
                    self.compile_expr(b, v)?;
                }
                let names: Vec<String> = fields.iter().map(|(k, _)| k.clone()).collect();
                b.emit(Op::MakeRecord(std::rc::Rc::new(names)), 0, 0);
            }
            Expr::Field { recv, name, line, col } => {
                self.compile_expr(b, recv)?;
                b.emit(Op::GetField(std::rc::Rc::new(name.clone())), *line, *col);
            }
            Expr::Method { recv, name, args, line, col } => {
                use crate::types::Type;
                let n = name.as_str();

                // 1. Type-directed column verbs. When the type checker proved the
                // receiver is a DataFrame/GroupBy, the args are *columns/predicates*
                // (not values), so route to the unevaluated-AST ops. This is the
                // only correct disambiguation — `where`/`sort`/`min` mean different
                // things per receiver type, and column args can't compile as values.
                if matches!(self.recv_type(recv), Some(Type::DataFrame))
                    && matches!(n, "where" | "filter" | "select" | "sort" | "group")
                {
                    self.compile_expr(b, recv)?;
                    let locals = std::rc::Rc::new(b.in_scope_locals());
                    b.emit(
                        Op::DfColumnVerb {
                            name: std::rc::Rc::new(name.clone()),
                            args: std::rc::Rc::new(args.to_vec()),
                            locals,
                        },
                        *line,
                        *col,
                    );
                    return Ok(());
                }
                if matches!(self.recv_type(recv), Some(Type::GroupBy))
                    && matches!(n, "mean" | "sum" | "min" | "max" | "count" | "std")
                {
                    self.compile_expr(b, recv)?;
                    b.emit(
                        Op::GroupByAgg {
                            name: std::rc::Rc::new(name.clone()),
                            args: std::rc::Rc::new(args.to_vec()),
                        },
                        *line,
                        *col,
                    );
                    return Ok(());
                }

                // 2. Comprehensions compile to inline bytecode loops (no closures).
                // For an Array receiver, `where`/`filter` are comprehensions (the
                // DataFrame case was handled above).
                if matches!(n, "map" | "filter" | "where" | "reduce") {
                    return self.compile_comprehension(b, recv, name, args, *line, *col);
                }
                if matches!(n, "any" | "all") {
                    return self.compile_any_all(b, recv, name, args, *line, *col);
                }

                // 3. `select`/`group` are DataFrame-only column verbs. A
                // statically-known DataFrame was routed in step 1; reaching here
                // means the receiver type is *not* a known DataFrame (most likely
                // `Unknown` — a DataFrame from a dynamic source). Emit the column-verb
                // op, which validates the receiver at runtime (a real DataFrame
                // works; anything else raises). The type checker already rejects
                // `array.select(...)`, so a wrong concrete type can't reach here.
                if matches!(n, "select" | "group") {
                    self.compile_expr(b, recv)?;
                    let locals = std::rc::Rc::new(b.in_scope_locals());
                    b.emit(
                        Op::DfColumnVerb {
                            name: std::rc::Rc::new(name.clone()),
                            args: std::rc::Rc::new(args.to_vec()),
                            locals,
                        },
                        *line,
                        *col,
                    );
                    return Ok(());
                }

                // 4. Everything else is a value-method with evaluated args —
                // including ambiguous aggregations on a non-GroupBy receiver
                // (`tensor.min(axis)`, `arr.sort()`). A GroupBy receiver was routed
                // in step 2; an Unknown one that is actually a GroupBy at runtime
                // with a bare-column argument would surface as "<col> is not defined"
                // (the column can't be a value) — an accepted edge for a receiver the
                // checker couldn't pin down.
                self.compile_expr(b, recv)?;
                for a in args {
                    self.compile_expr(b, a)?;
                }
                b.emit(Op::Method(std::rc::Rc::new(name.clone()), args.len() as u32), *line, *col);
            }
            Expr::Slice { recv, start, stop, step, line, col } => {
                self.compile_expr(b, recv)?;
                let mut mask = 0u8;
                if let Some(s) = start {
                    self.compile_expr(b, s)?;
                    mask |= 1;
                }
                if let Some(s) = stop {
                    self.compile_expr(b, s)?;
                    mask |= 2;
                }
                if let Some(s) = step {
                    self.compile_expr(b, s)?;
                    mask |= 4;
                }
                b.emit(Op::Slice(mask), *line, *col);
            }
            Expr::Lambda { params, body, .. } => {
                // A standalone lambda → a first-class function value. Its body is
                // compiled into its own chunk; free variables resolve to globals.
                let idx = self.compile_lambda(params, body)?;
                b.emit(Op::MakeFunc { idx, arity: params.len() as u32 }, 0, 0);
            }
            // NOTE: every `Expr` variant is now handled — `compile_expr` no longer
            // has a catch-all. The remaining whole-program fallbacks live in
            // `compile_stmt` (immutable reassignment) and the `Method` verb-with-args
            // path (DataFrame/Tensor column verbs).
        }
        Ok(())
    }

    /// Compile a `map`/`filter`/`where`/`reduce` comprehension into an inline
    /// loop. The element binder lives in a fresh local slot, and the body is
    /// compiled inline (so outer variables are reached directly — no closures).
    /// Cases the loop form doesn't cover (multi-parameter binders, malformed
    /// `reduce`) return `Unsupported` and fall back to the tree-walker.
    /// Declare a comprehension element binder pattern. For a single binder
    /// (`it`/`x`) `CompNext` writes straight into its slot. For a multi-binder
    /// pattern (`(a, b)`) `CompNext` writes the element into a hidden slot, which
    /// the returned `DestructureBind` op then splits into the named param slots
    /// each iteration (mirroring the tree-walker's `eval_with_pattern`).
    fn declare_binder_pattern(b: &mut Builder, params: &[String]) -> (u32, Option<std::rc::Rc<Vec<u32>>>) {
        if params.len() == 1 {
            (b.declare_local(&params[0]), None)
        } else {
            let elem = b.declare_local("$elem");
            let slots: Vec<u32> = params.iter().map(|p| b.declare_local(p)).collect();
            (elem, Some(std::rc::Rc::new(slots)))
        }
    }

    fn compile_comprehension(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> R<()> {
        if name == "reduce" {
            return self.compile_reduce(b, recv, args, line, col);
        }
        if args.len() != 1 {
            let example = if name == "map" { "(it * 2)" } else { "(it > 0)" };
            return self.raise_after_recv(
                b,
                recv,
                format!("`{}` takes exactly one expression", name),
                format!("e.g. `xs.{}{}`.", name, example),
                line,
                col,
            );
        }
        let (params, body) = crate::interp::comprehension_params(&args[0]);
        if params.is_empty() {
            return self.raise_after_recv(
                b,
                recv,
                format!("`{}`'s function needs at least one parameter", name),
                "e.g. `xs.map(it * 2)` or `xs.map((a, b) => ...)`.".to_string(),
                line,
                col,
            );
        }
        let kind = if name == "map" { CompKind::Map } else { CompKind::Filter };

        self.compile_expr(b, recv)?;
        let init_at = b.emit(Op::CompInit(kind, 0), line, col);

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;
        let (binder, destruct) = Self::declare_binder_pattern(b, &params);

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(binder, 0), line, col);
        if let Some(slots) = &destruct {
            b.emit(Op::LoadLocal(binder), line, col);
            b.emit(Op::DestructureBind(slots.clone()), line, col);
        }
        self.compile_expr(b, body)?;
        b.emit(
            if matches!(kind, CompKind::Map) { Op::CompMapPush } else { Op::CompFilterPush },
            line,
            col,
        );
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(binder, end_at);
        b.emit(Op::CompEnd, line, col);
        let jump_done = b.emit(Op::Jump(0), line, col);

        // missing-source landing: push `missing` as the whole result
        let missing_at = b.code.len() as u32;
        b.code[init_at] = Op::CompInit(kind, missing_at);
        let mk = b.add_const(Value::Missing);
        b.emit(Op::Const(mk), line, col);

        let done_at = b.code.len() as u32;
        b.code[jump_done] = Op::Jump(done_at);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }

    /// Compile `any`/`all` into a short-circuiting loop with a hidden
    /// "seen-missing" slot: `missing` in the undetermined position makes the
    /// answer `missing` (ADR-0001 three-valued logic), exactly like the interpreter.
    fn compile_any_all(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> R<()> {
        if args.len() != 1 {
            return self.raise_after_recv(
                b,
                recv,
                format!("`{}` takes exactly one expression", name),
                format!("e.g. `xs.{}(it > 0)`.", name),
                line,
                col,
            );
        }
        let (params, body) = crate::interp::comprehension_params(&args[0]);
        if params.is_empty() {
            return self.raise_after_recv(
                b,
                recv,
                format!("`{}`'s function needs at least one parameter", name),
                "e.g. `xs.any(it > 0)` or `xs.all((a, b) => a < b)`.".to_string(),
                line,
                col,
            );
        }
        let is_all = name == "all";
        let kind = if is_all { CompKind::All } else { CompKind::Any };

        self.compile_expr(b, recv)?;
        let init_at = b.emit(Op::CompInit(kind, 0), line, col);

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;
        let (binder, destruct) = Self::declare_binder_pattern(b, &params);
        // hidden seen-missing flag (the name can't collide with user identifiers)
        let fk = b.add_const(Value::Bool(false));
        b.emit(Op::Const(fk), line, col);
        let sm = b.declare_local("$sm");
        b.emit(Op::StoreLocal(sm), line, col);

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(binder, 0), line, col);
        if let Some(slots) = &destruct {
            b.emit(Op::LoadLocal(binder), line, col);
            b.emit(Op::DestructureBind(slots.clone()), line, col);
        }
        self.compile_expr(b, body)?;
        let test_at = b.emit(Op::CompBoolTest(is_all, sm, 0), line, col);
        b.emit(Op::Jump(loop_start), line, col);

        // exhausted without short-circuiting: `missing` if any element was missing,
        // else the default (`all` → true, `any` → false).
        let exhausted = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(binder, exhausted);
        b.emit(Op::CompEndDiscard, line, col);
        b.emit(Op::LoadLocal(sm), line, col);
        let jif = b.emit(Op::JumpIfFalse(0), line, col);
        let mk = b.add_const(Value::Missing);
        b.emit(Op::Const(mk), line, col);
        let jdone1 = b.emit(Op::Jump(0), line, col);
        let notmiss = b.code.len() as u32;
        b.code[jif] = Op::JumpIfFalse(notmiss);
        let dk = b.add_const(Value::Bool(is_all));
        b.emit(Op::Const(dk), line, col);
        let jdone2 = b.emit(Op::Jump(0), line, col);

        // short-circuit landing: `any` → true, `all` → false
        let short = b.code.len() as u32;
        b.code[test_at] = Op::CompBoolTest(is_all, sm, short);
        b.emit(Op::CompEndDiscard, line, col);
        let sk = b.add_const(Value::Bool(!is_all));
        b.emit(Op::Const(sk), line, col);
        let jdone3 = b.emit(Op::Jump(0), line, col);

        // missing source
        let missing_at = b.code.len() as u32;
        b.code[init_at] = Op::CompInit(kind, missing_at);
        let mk2 = b.add_const(Value::Missing);
        b.emit(Op::Const(mk2), line, col);

        let done = b.code.len() as u32;
        b.code[jdone1] = Op::Jump(done);
        b.code[jdone2] = Op::Jump(done);
        b.code[jdone3] = Op::Jump(done);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }

    /// `range(a, b).reduce(init, (acc, x) => body)` as a counting loop. No input
    /// array is built; `x` (the second binder) is the loop counter.
    #[allow(clippy::too_many_arguments)]
    fn compile_reduce_range(
        &mut self,
        b: &mut Builder,
        start: Option<&Expr>,
        end: &Expr,
        init: &Expr,
        pa: &str,
        pb: &str,
        body: &Expr,
        line: usize,
        col: usize,
    ) -> R<()> {
        // Push [start, end] for CompInitRange, then drive it with the same loop
        // as an array reduce — one dispatch per element, zero array allocated.
        match start {
            None => {
                let c0 = b.add_const(Value::Int(0));
                b.emit(Op::Const(c0), line, col);
            }
            Some(e) => self.compile_expr(b, e)?,
        }
        self.compile_expr(b, end)?;

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;

        // CRITICAL: compile `init` while the scope is still empty — the binders
        // (`pa`/`pb`) must NOT be visible to it. `reduce(x, (acc, x) => ...)`
        // evaluates `init` in the *outer* environment, so an `init` that mentions a
        // binder name must resolve to the outer binding, not the (unbound) loop
        // slot. Its value stays on the stack until it is stored into `acc` below.
        //
        // If the body is a pure `i64` expression over `{acc, x}`, register a native
        // loop for it and emit a runtime-guarded fast path. The guard (in the VM)
        // takes the native path only when start/end/init are all `Int` within the
        // cap; otherwise it falls through to the identical bytecode loop — so float
        // accumulators, over-cap ranges, and non-x86/`HELIX_NOJIT` builds all run
        // the oracle-matched path.
        let eligible = crate::jit::reduce_loop_eligible(body, pa, pb);
        let acc;
        let x;
        let guard;
        if eligible {
            self.compile_expr(b, init)?; // stack: [start, end, init]
            acc = b.declare_local(pa);
            x = b.declare_local(pb);
            b.emit(Op::StoreLocal(acc), line, col); // stack: [start, end]; acc=init
            let loop_idx = self.reduce_loops.len() as u32;
            self.reduce_loops.push(ReduceLoop {
                pa: pa.to_string(),
                pb: pb.to_string(),
                body: body.clone(),
            });
            // `after` is patched once the trailing LoadLocal position is known.
            let at = b.emit(Op::TryJitReduce { loop_idx, acc_slot: acc, after: 0 }, line, col);
            b.emit(Op::CompInitRange, line, col); // consumes [start, end] on fall-through
            guard = Some((at, loop_idx));
        } else {
            b.emit(Op::CompInitRange, line, col); // consumes [start, end]
            self.compile_expr(b, init)?; // outer scope (binders not declared yet)
            acc = b.declare_local(pa);
            x = b.declare_local(pb);
            b.emit(Op::StoreLocal(acc), line, col);
            guard = None;
        }

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(x, 0), line, col);
        self.compile_expr(b, body)?;
        b.emit(Op::StoreLocal(acc), line, col);
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(x, end_at);
        b.emit(Op::CompEndDiscard, line, col);
        let after_at = b.code.len() as u32;
        if let Some((at, loop_idx)) = guard {
            b.code[at] = Op::TryJitReduce { loop_idx, acc_slot: acc, after: after_at };
        }
        b.emit(Op::LoadLocal(acc), line, col);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }

    fn compile_reduce(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> R<()> {
        if args.len() != 2 {
            return self.raise_after_recv(
                b,
                recv,
                "`reduce` takes a starting value and an accumulator function".to_string(),
                "e.g. `xs.reduce(0, (acc, x) => acc + x)` to sum.".to_string(),
                line,
                col,
            );
        }
        let (pa, pb, body) = match &args[1] {
            Expr::Lambda { params, body, .. } if params.len() == 2 => {
                (params[0].clone(), params[1].clone(), body.as_ref())
            }
            // Match the tree-walker's two precise messages (wrong arity vs not a
            // function), evaluating the receiver first for side-effect parity.
            Expr::Lambda { params, .. } => {
                return self.raise_after_recv(
                    b,
                    recv,
                    format!(
                        "`reduce`'s function needs exactly two parameters, but got {}",
                        params.len()
                    ),
                    "the first is the running accumulator, e.g. `(acc, x) => acc + x`.".to_string(),
                    line,
                    col,
                );
            }
            _ => {
                return self.raise_after_recv(
                    b,
                    recv,
                    "`reduce` needs an explicit accumulator function".to_string(),
                    "name both binders: `xs.reduce(0, (acc, x) => acc + x)`.".to_string(),
                    line,
                    col,
                );
            }
        };

        // Range fusion: `range(...).reduce(...)` becomes a counting loop with no
        // array materialized at all — the element binder *is* the counter.
        if let Some((start, end)) = as_range_call(recv) {
            return self.compile_reduce_range(b, start, end, &args[0], &pa, &pb, body, line, col);
        }

        self.compile_expr(b, recv)?;
        let init_at = b.emit(Op::CompInit(CompKind::Reduce, 0), line, col);

        b.scopes.push(Vec::new());
        let saved_next = b.next_slot;

        // Compile `init` while the scope is empty so the binders (`pa`/`pb`) are not
        // visible to it — `reduce` evaluates its initial accumulator in the *outer*
        // environment (see the note in `compile_reduce_range`).
        self.compile_expr(b, &args[0])?; // initial accumulator (outer scope)
        let acc = b.declare_local(&pa);
        let x = b.declare_local(&pb);
        b.emit(Op::StoreLocal(acc), line, col);

        let loop_start = b.code.len() as u32;
        let next_at = b.emit(Op::CompNext(x, 0), line, col);
        self.compile_expr(b, body)?;
        b.emit(Op::StoreLocal(acc), line, col);
        b.emit(Op::Jump(loop_start), line, col);

        let end_at = b.code.len() as u32;
        b.code[next_at] = Op::CompNext(x, end_at);
        b.emit(Op::CompEndDiscard, line, col);
        b.emit(Op::LoadLocal(acc), line, col);
        let jump_done = b.emit(Op::Jump(0), line, col);

        let missing_at = b.code.len() as u32;
        b.code[init_at] = Op::CompInit(CompKind::Reduce, missing_at);
        let mk = b.add_const(Value::Missing);
        b.emit(Op::Const(mk), line, col);

        let done_at = b.code.len() as u32;
        b.code[jump_done] = Op::Jump(done_at);

        b.scopes.pop();
        b.next_slot = saved_next;
        Ok(())
    }
}

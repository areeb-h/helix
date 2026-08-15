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
//! It is *total*: every type-checked program lowers to bytecode (arrays, methods,
//! comprehensions, records, tensors, DataFrames, lambdas, …), so the VM is the sole
//! automatic engine — there is no silent tree-walker fallback. A user error in an
//! otherwise type-checked program compiles to an `Op::Raise` that reports it at
//! runtime, never to [`Unsupported`]; that sentinel remains only as a defensive
//! backstop, surfaced by the runner as an internal error if it were ever returned.
//! (The tree-walker still runs under `HELIX_NOVM` for A/B checks and for `try`,
//! whose error recovery the VM does not yet implement.)
//!
//! Semantics are kept identical to the tree-walker by reusing its value type and
//! its arithmetic/boolean helpers (`interp::eval_binary`, `eval_unary`, …), so
//! the VM can never silently disagree with the reference interpreter.

use std::collections::HashSet;

use crate::ast::{BinOp, Expr, InterpPart, Stmt, UnOp};
use crate::value::Value;

/// Defensive sentinel for a compilation that could not be lowered. The compiler is
/// total for any type-checked program, so this is never constructed in practice; if
/// it ever were, the runner surfaces it as an internal error rather than silently
/// falling back to the tree-walker. Retained so the lowering helpers keep a fallible
/// signature, leaving room to reintroduce a guarded fallback should one be needed.
#[derive(Debug)]
pub struct Unsupported;

/// Which comprehension a `CompInit` begins (drives the collect behaviour and the
/// "no such method" error wording).
#[derive(Debug, Clone, Copy)]
pub enum CompKind {
    Map,
    Filter,
    /// `where` — semantically identical to [`CompKind::Filter`] everywhere;
    /// a separate variant only so runtime errors quote the method the user
    /// actually wrote (the walker threads the surface name the same way).
    Where,
    Reduce,
    Any,
    All,
    /// `position` — the first index whose predicate result is exactly `Bool(want)`.
    Position,
}

impl CompKind {
    pub fn method_name(self) -> &'static str {
        match self {
            CompKind::Map => "map",
            CompKind::Filter => "filter",
            CompKind::Where => "where",
            CompKind::Reduce => "reduce",
            CompKind::Any => "any",
            CompKind::All => "all",
            CompKind::Position => "position",
        }
    }
}

type R<T> = Result<T, Unsupported>;


mod ops;
pub use ops::*;

/// How an identifier resolves at compile time.
enum NameRef {
    Local(u32),
    /// An upvalue (index into the current function's captured environment) — a
    /// variable closed over from an enclosing scope.
    Upvalue(u32),
    Global(u32),
    /// A user function (callable, but not usable as a first-class value yet).
    Func(u32),
}

/// Per-function code being built.
struct Builder {
    code: Vec<Op>,
    consts: Vec<Value>,
    pos: Vec<(u32, u32)>,
    /// Lexical scopes of locals; each entry maps a name to its slot.
    scopes: Vec<Vec<(String, u32)>>,
    next_slot: u32,
    max_slot: u32,
    /// This function's upvalues — names closed over from an enclosing scope, each
    /// paired with how the *enclosing* frame provides it. Populated lazily by
    /// `resolve_upvalue`; its order is the closure's upvalue layout.
    upvalues: Vec<(String, CaptureSrc)>,
    /// The capturable environment of the *enclosing* function (its locals + its
    /// upvalues, in enclosing-frame terms). Empty for `main` and top-level `fn`s.
    enclosing: Vec<(String, CaptureSrc)>,
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
            upvalues: Vec::new(),
            enclosing: Vec::new(),
        }
    }

    /// Resolve `name` to an upvalue index — either one already captured, or a fresh
    /// capture from the enclosing environment. `None` if it isn't capturable here.
    fn resolve_upvalue(&mut self, name: &str) -> Option<u32> {
        if let Some(i) = self.upvalues.iter().position(|(n, _)| n == name) {
            return Some(i as u32);
        }
        let src = self.enclosing.iter().find(|(n, _)| n == name).map(|(_, s)| *s)?;
        let idx = self.upvalues.len() as u32;
        self.upvalues.push((name.to_string(), src));
        Some(idx)
    }

    /// The capturable environment this function exposes to a nested lambda: each
    /// in-scope local as a `Local(slot)` source, plus every variable reachable from
    /// *this* function's enclosing scope, **eagerly captured into this function's own
    /// upvalues** so the nested lambda can chain-capture it (transitive closure: a
    /// grandchild can close over a grandparent's local). The eager registration adds
    /// at most the enclosing names as this function's upvalues — only when it
    /// actually contains a nested lambda (the only caller).
    fn capturable_env(&mut self) -> Vec<(String, CaptureSrc)> {
        let mut out: Vec<(String, CaptureSrc)> = Vec::new();
        for scope in &self.scopes {
            for (n, slot) in scope {
                // Innermost/latest binding wins on a duplicate name, exactly like
                // `resolve_local`'s reverse scan. `resolve_upvalue` matches with a
                // forward `.find()`, so a kept duplicate would hand a nested lambda
                // the OUTERMOST shadowed binding (`fn f(x) = let x = x + 1 in
                // (y => x + y)` captured the param, diverging from the walker).
                // Deduping here also keeps `enclosing` duplicate-free for the
                // shadow-skip and grandchild chains below.
                if let Some(entry) = out.iter_mut().find(|(m, _)| m == n) {
                    entry.1 = CaptureSrc::Local(*slot);
                } else {
                    out.push((n.clone(), CaptureSrc::Local(*slot)));
                }
            }
        }
        let enclosing_names: Vec<String> =
            self.enclosing.iter().map(|(n, _)| n.clone()).collect();
        for n in enclosing_names {
            // A local of this function shadows an enclosing name of the same name.
            if out.iter().any(|(m, _)| *m == n) {
                continue;
            }
            if let Some(uv) = self.resolve_upvalue(&n) {
                out.push((n, CaptureSrc::Upvalue(uv)));
            }
        }
        out
    }

    fn emit(&mut self, op: Op, line: usize, col: usize) -> usize {
        let at = self.code.len();
        self.code.push(op);
        self.pos.push((line as u32, col as u32));
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

/// Does this expression mention a `@column` reference anywhere in its tree?
/// Drives the Unknown-receiver routing of DataFrame column verbs: a column
/// reference can never evaluate as a value (it always raises), so its presence
/// in an argument list unambiguously marks the call as a DataFrame/GroupBy
/// operation even when the checker couldn't pin the receiver down — matching
/// the tree-walker, which dispatches on the receiver's *runtime* type.
/// Exhaustive on purpose: a new `Expr` variant must decide its answer here.
fn mentions_column(e: &Expr) -> bool {
    match e {
        Expr::Column { .. } => true,
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Missing
        | Expr::Ident { .. } => false,
        Expr::Interp(parts) => parts.iter().any(|p| match p {
            crate::ast::InterpPart::Expr(e, _) => mentions_column(e),
            crate::ast::InterpPart::Lit(_) => false,
        }),
        Expr::Array(items) | Expr::Tuple(items) => items.iter().any(mentions_column),
        Expr::Record(fields) => fields.iter().any(|(_, v)| mentions_column(v)),
        Expr::RecordUpdate { base, fields, .. } => {
            mentions_column(base) || fields.iter().any(|(_, v)| mentions_column(v))
        }
        Expr::Field { recv, .. } => mentions_column(recv),
        Expr::Unary { expr, .. } => mentions_column(expr),
        Expr::Binary { left, right, .. } => mentions_column(left) || mentions_column(right),
        Expr::Call { args, .. } => args.iter().any(mentions_column),
        Expr::Method { recv, args, named, .. } => {
            mentions_column(recv)
                || args.iter().any(mentions_column)
                || named.iter().any(|(_, v)| mentions_column(v))
        }
        Expr::CallValue { callee, args, .. } => {
            mentions_column(callee) || args.iter().any(mentions_column)
        }
        Expr::Index { recv, index, .. } => mentions_column(recv) || mentions_column(index),
        Expr::Slice { recv, start, stop, step, .. } => {
            mentions_column(recv)
                || [start, stop, step].iter().any(|o| o.as_deref().is_some_and(mentions_column))
        }
        Expr::Lambda { body, .. } => mentions_column(body),
        Expr::Let { bindings, body } => {
            bindings.iter().any(|(_, v)| mentions_column(v)) || mentions_column(body)
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            mentions_column(cond) || mentions_column(then_branch) || mentions_column(else_branch)
        }
        Expr::Try { expr, .. } => mentions_column(expr),
        Expr::Match { scrutinee, arms, .. } => {
            mentions_column(scrutinee)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(mentions_column) || mentions_column(&a.body)
                })
        }
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
    /// Accumulated JIT `map`/`filter` kernel requests (see [`Program::map_kernels`]).
    map_kernels: Vec<ArrayKernel>,
    filter_kernels: Vec<ArrayKernel>,
    /// Accumulated fuseable pipelines (see [`Program::fused_kernels`]).
    fused_kernels: Vec<FusedKernel>,
    /// Accumulated JIT `scan` (prefix-fold) requests (see [`Program::scan_loops`]).
    scan_loops: Vec<ReduceLoop>,
    /// Set while emitting a fused pipeline's fall-through, so the recompiled chain takes
    /// the ordinary per-stage path instead of re-triggering fusion (which would loop).
    no_fuse: bool,
    /// Nesting depth of function/lambda bodies being compiled; 0 means top level.
    ///
    /// This is what keeps a forward reference *scoped*. The tree-walker binds a top-level
    /// `fn` when execution REACHES it, so a peer's body may call it (bodies run after the
    /// whole file is walked) but a top-level statement above it may not. A slot reserved by
    /// PASS ONE and not yet filled is therefore visible only while `body_depth > 0`.
    body_depth: usize,
    /// User functions the JIT can compile to pure `i64` natives — so a kernel/fused body
    /// may *call* them. Computed once (identically to `jit::build`) so the compile-time
    /// guard decision matches what the JIT will actually compile.
    jit_fns: std::collections::HashSet<String>,
    /// User functions that get a MIXED specialization, with their parameter kinds and return
    /// kind. The names alone are not enough to type a call inside a mixed map body — a
    /// `Float`-parameter callee needs its signature — so this carries them. Same
    /// single-source-of-truth contract as `jit_fns`: `jit::build` re-derives it.
    mixed_sigs: std::collections::HashMap<String, (Vec<crate::jit::NumKind>, crate::jit::NumKind)>,
    /// Inferred receiver types from the type checker (see [`crate::types::TypeMap`]),
    /// used to route receiver-polymorphic methods. `None` when compiling without a
    /// prior type-check (tests/fuzzers) — then such methods fall back as before.
    types: Option<crate::types::TypeMap>,
}

/// Compile a whole program to bytecode. Total for any type-checked program (see the
/// module docs); the [`Unsupported`] return is a defensive backstop, not a routine
/// tree-walker fallback. Optionally takes the type checker's inferred receiver types
/// to route receiver-polymorphic methods (DataFrame/Tensor column-verbs); pass `None`
/// to compile without a prior type-check (tests/fuzzers), where such methods route by
/// runtime receiver type instead.
pub fn compile_with_types(program: &[Stmt], types: Option<crate::types::TypeMap>) -> R<Program> {
    let mut c = Compiler {
        // Seed the math constants and the `python` interop entry point as immutable
        // globals so programs that use `pi`/`e`/`inf`/`python` compile (the
        // tree-walker predefines the same bindings).
        globals: vec!["pi".into(), "e".into(), "inf".into(), "python".into()],
        global_mut: vec![false, false, false, false],
        global_init: vec![
            Value::Float(std::f64::consts::PI),
            Value::Float(std::f64::consts::E),
            Value::Float(f64::INFINITY),
            Value::PyObject(std::rc::Rc::new(crate::python::PyHandle::namespace())),
        ],
        func_names: vec!["<main>".into()],
        func_arity: vec![0], // main takes no params
        funcs: vec![None], // slot 0 reserved for main
        builtins: Vec::new(),
        reduce_loops: Vec::new(),
        map_kernels: Vec::new(),
        filter_kernels: Vec::new(),
        fused_kernels: Vec::new(),
        scan_loops: Vec::new(),
        no_fuse: false,
        body_depth: 0,
        jit_fns: crate::jit::int_eligible_fns(program),
        mixed_sigs: crate::jit::mixed_fn_sigs(program),
        types,
    };

    // PASS ONE: reserve a function-table slot for every top-level `fn`, before compiling any
    // body. `compile_func` already reserved its OWN index before compiling its own body, so
    // that a recursive self-call could resolve; this is the same trick widened to peers, and
    // it is what makes mutual recursion compile:
    //
    //     fn even(n) = if n == 0 then true else odd(n - 1)
    //     fn odd(n)  = if n == 0 then false else even(n - 1)
    //
    // Without it, `odd` had no index when `even`'s body was compiled, so the call lowered to
    // a raise — while the TREE-WALKER ran the same program fine, because it resolves names at
    // call time. Two engines disagreeing about whether a program exists is the worst shape of
    // divergence this project can have, and it was hidden behind a front-end error that fired
    // first.
    //
    // Only the first `fn` of a given name reserves; a later one appends its own slot when
    // `compile_func` finds no free reservation, which preserves today's duplicate-definition
    // behaviour exactly.
    //
    // A name that SHADOWS A BUILTIN never reserves. Reserving it would make the shadow
    // retroactive — the walker resolves such a name at call time, so every call above the
    // definition reaches the builtin and only calls below reach the user's function:
    //
    //     print(round(1.4))     # 1  -- the builtin
    //     fn round(x) = 99
    //     print(round(1.4))     # 99 -- the user's
    //
    // `tests/corpus/j14_rounders_and_int_mixed.helix` is exactly this program (it calls the
    // builtin `round` twenty times, then defines `fn round(x) = 99` at line 55) and it
    // caught the retroactive reading: `[99, 99, 99, 99]` where the walker says `[1, 2, 3, 4]`.
    // Order-blind reservation cannot express "builtin until here, user's after", so a
    // shadowing name keeps the source-order path it has always had. The exclusion costs only
    // mutual recursion BETWEEN TWO BUILTIN NAMES, which resolves to the builtins anyway.
    for stmt in program {
        if let Stmt::Func { name, params, .. } = stmt
            && !c.func_names.iter().any(|n| n == name)
        {
            c.func_names.push(name.clone());
            c.func_arity.push(params.len() as u32);
            c.funcs.push(None);
        }
    }

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

    // A slot reserved by PASS ONE stays empty when the definition statement was *rejected*
    // rather than compiled: `fn inf(x) = ...` over the seeded immutable `inf`, or over a
    // user global bound earlier in the file. That arm emits a raise at the definition point
    // and returns without a body, and PASS ONE cannot predict it — a user global only enters
    // `globals` as compilation reaches its binding, so the collision is not knowable before
    // the walk. The slot must still hold a chunk, because indices are baked into every
    // emitted `Call`; it just has to be one that cannot answer.
    let funcs: Vec<Chunk> = (0..c.funcs.len())
        .map(|i| match c.funcs[i].take() {
            Some(chunk) => chunk,
            None => rejected_fn_chunk(&c.func_names[i], c.func_arity[i]),
        })
        .collect();
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
        map_kernels: c.map_kernels,
        filter_kernels: c.filter_kernels,
        fused_kernels: c.fused_kernels,
        scan_loops: c.scan_loops,
        global_names: std::rc::Rc::new(c.globals),
    })
}


mod analysis;
pub(crate) use analysis::*;

/// If `e` is a `range(...)` call with 1 or 2 arguments, return its `(start, end)`
/// where a 1-argument range has `None` start (i.e. 0). Enables range fusion.
fn as_range_call(e: &Expr) -> Option<(Option<&Expr>, &Expr)> {
    if let Expr::Call { name, args, .. } = e
        && name == "range" {
            return match args.len() {
                1 => Some((None, &args[0])),
                2 => Some((Some(&args[0]), &args[1])),
                _ => None,
            };
        }
    None
}

/// The chunk installed for a function slot that PASS ONE reserved and no definition ever
/// filled — see the call site in [`compile_with_types`]. Nothing should reach it: execution
/// stops at the raise the rejected definition emitted, and a call written ABOVE that
/// definition resolves to the colliding global, which wins `resolve`. It raises the same
/// error the definition point does, so that if a path here is ever found the result is that
/// error rather than a wrong answer or a panic in the compiler.
fn rejected_fn_chunk(name: &str, arity: u32) -> Chunk {
    let mut b = Builder::new();
    let (msg, hint) = crate::error::immutable_reassign(name);
    b.emit(
        Op::raise(std::rc::Rc::new(msg), std::rc::Rc::new(hint)),
        0,
        0,
    );
    b.emit(Op::Return, 0, 0);
    Chunk {
        code: b.code,
        consts: b.consts,
        pos: b.pos,
        n_params: arity,
        n_locals: b.max_slot.max(arity),
    }
}

/// Tail-call peephole: rewrite every `CallFn` whose control-flow successor leads straight to
/// `Return` (directly, or through a chain of unconditional `Jump`s) into a `TailCallFn`, which
/// reuses the current frame instead of pushing one. This is exactly "the call is in tail
/// position": its result is immediately returned, so the caller's frame is dead. Makes tail
/// recursion (an accept loop, a state machine) constant-space — no frame accumulation, no leak.
///
/// It works on the *final* bytecode (all jumps patched), so it uniformly covers tail calls
/// nested in `if`/`match`/`let`/`do`. A call inside a `try` region is followed by `TryOk`
/// (not `Jump`/`Return`), so it is correctly left as a plain `CallFn` — the frame must survive
/// for the handler to unwind to it.
fn tco_peephole(code: &mut [Op]) {
    // memo[pc]: -1 unknown, 0 no, 1 yes. Marking "no" before recursing breaks `Jump` cycles
    // (an infinite jump loop never reaches a `Return`).
    fn leads_to_return(code: &[Op], memo: &mut [i8], pc: usize) -> bool {
        if pc >= code.len() {
            return false;
        }
        if memo[pc] != -1 {
            return memo[pc] == 1;
        }
        memo[pc] = 0;
        let r = match &code[pc] {
            Op::Return => true,
            Op::Jump(t) => leads_to_return(code, memo, *t as usize),
            _ => false,
        };
        memo[pc] = i8::from(r);
        r
    }
    let mut memo = vec![-1i8; code.len()];
    for pc in 0..code.len() {
        if let Op::CallFn { idx, nargs } = code[pc]
            && leads_to_return(code, &mut memo, pc + 1)
        {
            code[pc] = Op::TailCallFn { idx, nargs };
        }
    }
}

impl Compiler {
    /// The type checker's inferred type for a method receiver expression, if a
    /// type-check ran. Keyed by the receiver's node address (stable: the AST is
    /// not cloned between `types::check` and here).
    fn recv_type(&self, recv: &Expr) -> Option<&crate::types::Type> {
        self.types.as_ref().and_then(|m| m.get(&(recv as *const Expr)))
    }

    /// Emit a DataFrame column verb (or, with `group_agg`, a GroupBy
    /// aggregation) whose receiver's type is only proven at runtime: the
    /// receiver, then an ADR-0001 missing-propagation guard, then the
    /// unevaluated-args op. The guard matters because the verb ops raise on a
    /// non-DataFrame receiver, while the walker propagates a `missing` one;
    /// without it the engines diverge on a `missing` fed through a
    /// dynamically-typed helper. `is_missing` is universal in `Op::Method`
    /// (DataFrame/GroupBy included), so the guard is total.
    ///
    /// `eval_args_on_missing` mirrors the walker's two dispatch routes:
    /// `where`/`filter` are comprehension-shaped — the receiver is checked
    /// FIRST and a `missing` one propagates with the predicate untouched —
    /// while every other verb goes through the value-method path, which
    /// evaluates arguments BEFORE dispatch, so `missing.sort(@a)` raises the
    /// column-reference error from evaluating `@a` and only an arg-clean call
    /// propagates.
    #[allow(clippy::too_many_arguments)]
    fn compile_df_verb_guarded(
        &mut self,
        b: &mut Builder,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        group_agg: bool,
        eval_args_on_missing: bool,
        line: usize,
        col: usize,
    ) -> R<()> {
        self.compile_expr(b, recv)?;
        let slot = b.declare_local("$dfrecv");
        b.emit(Op::StoreLocal(slot), line, col);
        b.emit(Op::LoadLocal(slot), line, col);
        b.emit(
            Op::Method(std::rc::Rc::new(MethodData {
                name: std::rc::Rc::new("is_missing".to_string()),
                nargs: 0,
            })),
            line,
            col,
        );
        // Not missing (the common case) → jump ahead to the verb itself.
        let jverb = b.emit(Op::JumpIfFalse(0), line, col);
        // Missing receiver: evaluate the args first when the walker would
        // (a `@col` raises the column-reference error here, exactly like the
        // walker's value-method path), then the receiver is the result.
        if eval_args_on_missing {
            for a in args {
                self.compile_expr(b, a)?;
                b.emit(Op::Pop, line, col);
            }
        }
        b.emit(Op::LoadLocal(slot), line, col);
        let jend = b.emit(Op::Jump(0), line, col);
        let verb_at = b.code.len() as u32;
        b.code[jverb] = Op::JumpIfFalse(verb_at);
        b.emit(Op::LoadLocal(slot), line, col);
        if group_agg {
            b.emit(
                Op::GroupByAgg(std::rc::Rc::new(GroupByAggData {
                    name: std::rc::Rc::new(name.to_string()),
                    args: std::rc::Rc::new(args.to_vec()),
                })),
                line,
                col,
            );
        } else {
            let locals = std::rc::Rc::new(b.in_scope_locals());
            b.emit(
                Op::DfColumnVerb(std::rc::Rc::new(DfColumnVerbData {
                    name: std::rc::Rc::new(name.to_string()),
                    args: std::rc::Rc::new(args.to_vec()),
                    locals,
                })),
                line,
                col,
            );
        }
        let end = b.code.len() as u32;
        b.code[jend] = Op::Jump(end);
        Ok(())
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
        b.emit(Op::raise(std::rc::Rc::new(msg), std::rc::Rc::new(hint)), line, col);
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

    fn resolve(&self, b: &mut Builder, name: &str) -> Option<NameRef> {
        if let Some(slot) = b.resolve_local(name) {
            return Some(NameRef::Local(slot));
        }
        // Closed over from an enclosing scope? (Before globals: lexical order.)
        if let Some(uv) = b.resolve_upvalue(name) {
            return Some(NameRef::Upvalue(uv));
        }
        if let Some(i) = self.globals.iter().position(|g| g == name) {
            return Some(NameRef::Global(i as u32));
        }
        if let Some(i) = self.func_names.iter().position(|f| f == name)
            && self.fn_slot_visible(i)
        {
            return Some(NameRef::Func(i as u32));
        }
        None
    }

    /// Can a `fn` slot be named from where we are compiling right now?
    ///
    /// A filled slot always can — its definition is behind us. An *unfilled* one is a PASS
    /// ONE reservation for a definition still ahead, and it is nameable only from inside a
    /// body (`body_depth > 0`), because a body does not run until the whole file is bound.
    /// At top level the walker would raise "not a known function", so we must not resolve:
    ///
    ///     print(f(1))       # walker: error -- `f` is not a known function
    ///     fn f(x) = x + 1
    ///
    /// The slot a function reserved for ITSELF is unfilled while its own body compiles, and
    /// stays visible by the same rule — which is what has always made self-recursion resolve.
    /// Has this `fn`'s DEFINITION been compiled already — i.e. does it sit above the point
    /// being compiled? Distinct from [`Self::fn_slot_visible`], and the distinction matters:
    /// name RESOLUTION is file-scoped (ADR 0027), but assignment LEGALITY is still about
    /// declaration order. `mut f = 5` above `fn f(x) = …` reassigns the mutable global, so
    /// the assign/destructure arms must not see the not-yet-compiled reservation and reject
    /// it. Conflating the two turned that program into "`f` is immutable and cannot be
    /// reassigned" at line 1.
    fn fn_slot_defined_above(&self, i: usize) -> bool {
        self.funcs[i].is_some()
    }

    fn fn_slot_visible(&self, _i: usize) -> bool {
        // ADR 0027: a top-level `fn` is FILE-SCOPED, so a reservation is nameable from
        // anywhere in the file — including a top-level statement above the definition, which
        // the walker now answers too (`hoist_top_level_fns`). This used to require
        // `body_depth > 0`, pinning the walker's resolve-at-call-time order-sensitivity;
        // that is exactly what the ADR decided to remove. Kept as a function rather than
        // inlined because the three call sites read better naming the rule.
        true
    }

    /// Is `name` bound to a *user* value/function (local, captured upvalue,
    /// enclosing-scope name, global, or top-level `fn`) - i.e. would it shadow a
    /// builtin of the same name? Read-only, unlike `resolve` (no upvalue capture).
    fn name_is_user_bound(&self, b: &Builder, name: &str) -> bool {
        b.resolve_local(name).is_some()
            || b.upvalues.iter().any(|(n, _)| n == name)
            || b.enclosing.iter().any(|(n, _)| n == name)
            || self.globals.iter().any(|g| g == name)
            || self
                .func_names
                .iter()
                .position(|f| f == name)
                .is_some_and(|i| self.fn_slot_visible(i))
    }

    /// `as_range_call`, but only when `range` is the *builtin* (not shadowed by a
    /// user binding) - otherwise range-fusion would wrongly fuse the user fn,
    /// diverging from the tree-walker (which calls it).
    fn builtin_range_call<'a>(
        &self,
        b: &Builder,
        e: &'a Expr,
    ) -> Option<(Option<&'a Expr>, &'a Expr)> {
        if self.name_is_user_bound(b, "range") {
            return None;
        }
        as_range_call(e)
    }

    fn compile_stmt(&mut self, b: &mut Builder, stmt: &Stmt) -> R<()> {
        match stmt {
            Stmt::Assign { name, mutable, value, line, col, .. } => {
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
                        let (msg, hint) = crate::error::immutable_reassign(name);
                        b.emit(
                            Op::raise(std::rc::Rc::new(msg), std::rc::Rc::new(hint)),
                            *line,
                            *col,
                        );
                    }
                } else {
                    // A name declared by a top-level `fn` (and not owned by a
                    // mutable global) cannot be (re)assigned — `mut` or plain:
                    // compiled `CallFn` targets bind at compile time, so a late
                    // rebinding could never be honored here; the walker's env
                    // rejects it with this same error. Without this, the
                    // assignment silently created a global that shadowed the fn
                    // in `resolve` while previously compiled calls kept hitting
                    // the original — an engine divergence.
                    // `fn_slot_visible` matters here: a PASS ONE reservation for a `fn`
                    // defined LATER must not reject an assignment written above it, since
                    // the walker has not bound that name yet either (`mut f = 5` above
                    // `fn f(x) = ...` reassigns the mutable global when the `fn` is reached).
                    if self
                        .func_names
                        .iter()
                        .position(|f| f == name)
                        .is_some_and(|i| self.fn_slot_defined_above(i))
                    {
                        let (msg, hint) = crate::error::immutable_reassign(name);
                        b.emit(
                            Op::raise(std::rc::Rc::new(msg), std::rc::Rc::new(hint)),
                            *line,
                            *col,
                        );
                        return Ok(());
                    }
                    let i = self.globals.len() as u32;
                    self.globals.push(name.clone());
                    self.global_mut.push(*mutable);
                    self.global_init.push(Value::Unit);
                    b.emit(Op::StoreGlobal(i), *line, *col);
                }
                Ok(())
            }
            // Stripped by the module loader before compilation (see `Stmt::Import`).
            Stmt::Import { .. } => Ok(()),
            Stmt::Func { name, params, body, line, col, .. } => {
                // A top-level `fn` binds its name exactly like the walker's env
                // bind: over an *immutable* global (the seeded constants
                // `pi`/`e`/`inf`/`python`, or an earlier `x = …`) it raises at
                // the definition point; over a *mutable* global it reassigns —
                // the global then holds the function value. Without this the fn
                // registered silently and later calls resolved to the stale
                // global (globals win `resolve`), so VM/JIT printed the old
                // value or "`inf` is a Float, not a function" where the walker
                // errored (immutable) or called the fn (mutable).
                if let Some(i) = self.globals.iter().position(|g| g == name) {
                    if !self.global_mut[i] {
                        let (msg, hint) = crate::error::immutable_reassign(name);
                        b.emit(
                            Op::raise(std::rc::Rc::new(msg), std::rc::Rc::new(hint)),
                            *line,
                            *col,
                        );
                        return Ok(());
                    }
                    let arity = params.len() as u32;
                    // `compile_func` may fill a slot reserved by the pre-pass rather than
                    // appending, so take the index from it instead of guessing `funcs.len()`.
                    let idx = self.compile_func(name, params, body)?;
                    b.emit(Op::MakeFunc { idx, arity }, *line, *col);
                    b.emit(Op::StoreGlobal(i as u32), *line, *col);
                    return Ok(());
                }
                self.compile_func(name, params, body).map(|_| ())
            }
            Stmt::Expr(e) => {
                self.compile_expr(b, e)?;
                b.emit(Op::Pop, 0, 0);
                Ok(())
            }
            Stmt::Destructure { names, mutable, value, line, col, .. } => {
                self.compile_expr(b, value)?;
                // Same fn-declaration rule as `Assign`: a destructure target that
                // names a top-level `fn` (not owned by a mutable global) rejects,
                // `mut` or plain — see the Assign arm for why.
                for name in names {
                    if !self.globals.iter().any(|g| g == name)
                        && self
                            .func_names
                            .iter()
                            .position(|f| f == name)
                            .is_some_and(|i| self.fn_slot_defined_above(i))
                    {
                        let (msg, hint) = crate::error::immutable_reassign(name);
                        b.emit(
                            Op::raise(std::rc::Rc::new(msg), std::rc::Rc::new(hint)),
                            *line,
                            *col,
                        );
                        return Ok(());
                    }
                }
                // Same mutability rule as `Assign`: `mut a, b = …` (re)declares each
                // as mutable; a plain destructure reassigning an *immutable* global
                // is an error. (The tree-walker checks arity first, then mutability;
                // an arity mismatch *and* an immutable target is a rare error-on-error
                // edge where the message may differ — both still reject.)
                if !*mutable {
                    for name in names {
                        if let Some(i) = self.globals.iter().position(|g| g == name)
                            && !self.global_mut[i] {
                                let (msg, hint) = crate::error::immutable_reassign(name);
                                b.emit(
                                    Op::raise(std::rc::Rc::new(msg), std::rc::Rc::new(hint)),
                                    *line,
                                    *col,
                                );
                                return Ok(());
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
    ) -> R<u32> {
        // Take the slot `reserve_top_level_fns` set aside for this name if there is one —
        // that pre-pass is what lets a body call a peer defined BELOW it. Otherwise reserve
        // one here, which is the original behaviour and still the right one for a second
        // `fn` of the same name (the first definition already filled the reserved slot, so
        // this finds none free and appends, exactly as before).
        //
        // Either way the index exists before the body is compiled, which is what made
        // recursive SELF-calls resolve and now makes mutual ones resolve too.
        let idx = match self
            .func_names
            .iter()
            .position(|n| n == name)
            .filter(|&i| self.funcs[i].is_none())
        {
            Some(i) => i,
            None => {
                self.func_names.push(name.to_string());
                self.func_arity.push(params.len() as u32);
                self.funcs.push(None);
                self.funcs.len() - 1
            }
        };

        let mut fb = Builder::new();
        for (pname, _) in params {
            fb.declare_local(pname);
        }
        self.body_depth += 1;
        let compiled = self.compile_expr(&mut fb, body);
        self.body_depth -= 1;
        compiled?;
        fb.emit(Op::Return, 0, 0);
        tco_peephole(&mut fb.code);

        let chunk = Chunk {
            code: fb.code,
            consts: fb.consts,
            pos: fb.pos,
            n_params: params.len() as u32,
            n_locals: fb.max_slot,
        };
        self.funcs[idx] = Some(chunk);
        Ok(idx as u32)
    }

    /// Compile an anonymous lambda body into its own chunk (like [`Self::compile_func`]
    /// but nameless) and return its function-table index plus the capture sources
    /// for its upvalues (in the *enclosing* frame's terms, for `MakeClosure`). A free
    /// variable that names an enclosing local/upvalue becomes an upvalue; anything
    /// else resolves to a global or function as before. `enclosing` is the defining
    /// function's capturable environment.
    fn compile_lambda(
        &mut self,
        params: &[String],
        body: &Expr,
        enclosing: Vec<(String, CaptureSrc)>,
    ) -> R<(u32, Vec<CaptureSrc>)> {
        let idx = self.funcs.len() as u32;
        self.func_names.push("<lambda>".to_string());
        self.func_arity.push(params.len() as u32);
        self.funcs.push(None);

        let mut fb = Builder::new();
        fb.enclosing = enclosing;
        for p in params {
            fb.declare_local(p);
        }
        // Deliberately does NOT bump `body_depth`: a lambda inherits the depth of wherever it
        // is written. Inside a `fn` body it is already >0, so `xs.map(x => peer(x))` sees a
        // peer defined below. Written at TOP LEVEL it stays 0, because a top-level lambda can
        // be called immediately — `print((x => f(x))(1))` above `fn f` must keep raising, as
        // the walker does. (The converse, a top-level lambda called only after the
        // definition, still does not resolve; that gap predates PASS ONE and is unchanged by
        // it, since at depth 0 the visible set is exactly "definitions compiled so far".)
        self.compile_expr(&mut fb, body)?;
        fb.emit(Op::Return, 0, 0);
        tco_peephole(&mut fb.code);

        let captures: Vec<CaptureSrc> = fb.upvalues.iter().map(|(_, src)| *src).collect();
        let chunk = Chunk {
            code: fb.code,
            consts: fb.consts,
            pos: fb.pos,
            n_params: params.len() as u32,
            n_locals: fb.max_slot,
        };
        self.funcs[idx as usize] = Some(chunk);
        Ok((idx, captures))
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
            // A `@column` outside a DataFrame verb. The type checker rejects this
            // before compile, so it is unreachable in the normal pipeline; emit a
            // runtime error (rather than `Unsupported`) so `compile` is total.
            Expr::Column { name, line, col } => {
                b.emit(
                    Op::raise(
                        std::rc::Rc::new(format!(
                            "`@{name}` is a column reference, only valid inside a DataFrame operation"
                        )),
                        std::rc::Rc::new(
                            "use `@column` inside a verb like `df.where(...)`, `df.select(...)`, or `df.group(...)`.".to_string(),
                        ),
                    ),
                    *line,
                    *col,
                );
            }
            Expr::Ident { name, line, col } => match self.resolve(b, name) {
                Some(NameRef::Local(slot)) => {
                    b.emit(Op::LoadLocal(slot), *line, *col);
                }
                Some(NameRef::Upvalue(uv)) => {
                    b.emit(Op::GetUpvalue(uv), *line, *col);
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
                    // The hint goes through the one suggester, like every other site;
                    // `assign it first, e.g. `None = ...`` is gone for good. An empty
                    // hint renders as no `help:` line at all.
                    let hint = crate::suggest::hint(name, crate::suggest::Site::Value, &[])
                        .unwrap_or_default();
                    b.emit(
                        Op::raise(
                            std::rc::Rc::new(format!("`{}` is not defined", name)),
                            std::rc::Rc::new(hint),
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
            // `try EXPR` compiles to a guarded region: `TryBegin(catch)` installs a
            // handler, the body runs, `TryOk(end)` wraps the value in the ok-record on
            // the normal path, and `TryErr` at `catch` wraps the caught message in the
            // err-record. The VM's central error handler unwinds to the nearest
            // handler — so `try` is native, no tree-walker fallback.
            Expr::Try { expr, line, col } => {
                let jbegin = b.emit(Op::TryBegin(0), *line, *col);
                self.compile_expr(b, expr)?;
                let jok = b.emit(Op::TryOk(0), *line, *col);
                let catch_ip = b.code.len() as u32;
                b.emit(Op::TryErr, *line, *col);
                let end_ip = b.code.len() as u32;
                b.code[jbegin] = Op::TryBegin(catch_ip);
                b.code[jok] = Op::TryOk(end_ip);
            }
            Expr::Match { scrutinee, arms, line, col } => {
                // Evaluate the scrutinee once into a temp local, then test each arm in
                // order: `LoadLocal + MatchArm` leaves the bound values (if any) and a
                // bool; `JumpIfFalse` skips to the next arm on a miss, else the values
                // are stored into the arm's locals and the body runs.
                b.scopes.push(Vec::new());
                let saved_next = b.next_slot;
                self.compile_expr(b, scrutinee)?;
                let m_slot = b.declare_local("$match"); // sentinel name; only the slot is used
                b.emit(Op::StoreLocal(m_slot), *line, *col);

                let mut end_jumps: Vec<usize> = Vec::new();
                for arm in arms {
                    let names = crate::interp::pattern_binding_names(&arm.pattern);
                    b.scopes.push(Vec::new());
                    let saved2 = b.next_slot;
                    let slots: Vec<u32> = names.iter().map(|n| b.declare_local(n)).collect();
                    b.emit(Op::LoadLocal(m_slot), *line, *col);
                    b.emit(Op::MatchArm(std::rc::Rc::new(arm.pattern.clone())), *line, *col);
                    let jpat = b.emit(Op::JumpIfFalse(0), *line, *col);
                    // Matched: the bound values are on the stack in order; store them
                    // into the arm's locals (top is the last, so store in reverse).
                    for slot in slots.iter().rev() {
                        b.emit(Op::StoreLocal(*slot), *line, *col);
                    }
                    // A guard (with the bindings in scope) must also hold; if it's
                    // false, fall through to the next arm. `GuardCheck`, not
                    // `JumpIfFalse`: same control flow, guard-specific error wording
                    // (shared with the walker via `interp::guard_bool`).
                    let jguard = match &arm.guard {
                        Some(g) => {
                            self.compile_expr(b, g)?;
                            Some(b.emit(Op::GuardCheck(0), *line, *col))
                        }
                        None => None,
                    };
                    self.compile_expr(b, &arm.body)?;
                    end_jumps.push(b.emit(Op::Jump(0), *line, *col));
                    b.scopes.pop();
                    b.next_slot = saved2;
                    let next_at = b.code.len() as u32;
                    b.code[jpat] = Op::JumpIfFalse(next_at);
                    if let Some(jg) = jguard {
                        b.code[jg] = Op::GuardCheck(next_at);
                    }
                }
                // Fell through every arm: no match (the tree-walker's error).
                b.emit(
                    Op::raise(
                        std::rc::Rc::new("no `match` arm matched the value".to_string()),
                        std::rc::Rc::new(
                            "add a `_ => ...` arm to handle any remaining case.".to_string(),
                        ),
                    ),
                    *line,
                    *col,
                );
                let end = b.code.len() as u32;
                for j in end_jumps {
                    b.code[j] = Op::Jump(end);
                }
                b.scopes.pop();
                b.next_slot = saved_next;
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
                // A user binding of this name shadows a builtin of the same name —
                // defining `fn sign(..)` calls *your* function, not the math builtin.
                // Only a name with no user binding falls through to the builtin.
                match self.resolve(b, name) {
                    Some(NameRef::Func(idx)) => {
                        for a in args {
                            self.compile_expr(b, a)?;
                        }
                        b.emit(Op::CallFn { idx, nargs: args.len() as u32 }, *line, *col);
                    }
                    // A value-bound function: load the value, then the args, and
                    // dispatch on the value's chunk at runtime (errors if not callable).
                    Some(NameRef::Global(i)) => {
                        b.emit(Op::LoadGlobal(i), *line, *col);
                        for a in args {
                            self.compile_expr(b, a)?;
                        }
                        b.emit(
                            Op::CallValue(std::rc::Rc::new(CallValueData {
                                nargs: args.len() as u32,
                                name: std::rc::Rc::new(name.clone()),
                            })),
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
                            Op::CallValue(std::rc::Rc::new(CallValueData {
                                nargs: args.len() as u32,
                                name: std::rc::Rc::new(name.clone()),
                            })),
                            *line,
                            *col,
                        );
                    }
                    // A captured (upvalue) function value, e.g. a function-valued
                    // parameter closed over by an inner lambda.
                    Some(NameRef::Upvalue(uv)) => {
                        b.emit(Op::GetUpvalue(uv), *line, *col);
                        for a in args {
                            self.compile_expr(b, a)?;
                        }
                        b.emit(
                            Op::CallValue(std::rc::Rc::new(CallValueData {
                                nargs: args.len() as u32,
                                name: std::rc::Rc::new(name.clone()),
                            })),
                            *line,
                            *col,
                        );
                    }
                    // No user binding → the builtin, or (unreachable past the checker)
                    // an unknown name. Args are still evaluated for their side effects.
                    None => {
                        if crate::registry::lookup(name).is_some() {
                            for a in args {
                                self.compile_expr(b, a)?;
                            }
                            let idx = self.builtin_idx(name);
                            b.emit(Op::CallBuiltin { idx, nargs: args.len() as u32 }, *line, *col);
                        } else {
                            for a in args {
                                self.compile_expr(b, a)?;
                            }
                            b.emit(
                                Op::raise(
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
            Expr::CallValue { callee, args, line, col } => {
                // The callee value sits below the args on the stack; `Op::CallValue`
                // dispatches on it at runtime (the same opcode a value-bound name uses),
                // erroring if it isn't a function.
                self.compile_expr(b, callee)?;
                for a in args {
                    self.compile_expr(b, a)?;
                }
                b.emit(
                    Op::CallValue(std::rc::Rc::new(CallValueData {
                        nargs: args.len() as u32,
                        name: std::rc::Rc::new(callee.call_label()),
                    })),
                    *line,
                    *col,
                );
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
                    if let InterpPart::Expr(e, _) = part {
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
                // Intern the field names to `Symbol`s once here, so every record
                // built from this op carries the shared integer keys.
                let names: Vec<crate::symbol::Symbol> =
                    fields.iter().map(|(k, _)| crate::symbol::Symbol::intern(k)).collect();
                b.emit(Op::MakeRecord(std::rc::Rc::new(names)), 0, 0);
            }
            Expr::RecordUpdate { base, fields, line, col } => {
                // Base first (it sits below the field values), then each update value in
                // order — same evaluation order as the tree-walker, so side effects match.
                self.compile_expr(b, base)?;
                for (_, v) in fields {
                    self.compile_expr(b, v)?;
                }
                let names: Vec<crate::symbol::Symbol> =
                    fields.iter().map(|(k, _)| crate::symbol::Symbol::intern(k)).collect();
                b.emit(Op::UpdateRecord(std::rc::Rc::new(names)), *line, *col);
            }
            Expr::Field { recv, name, line, col } => {
                self.compile_expr(b, recv)?;
                b.emit(Op::GetField(crate::symbol::Symbol::intern(name)), *line, *col);
            }
            Expr::Method { recv, name, args, line, col, .. } => {
                use crate::types::Type;
                let n = name.as_str();

                // 1. Type-directed column verbs. When the type checker proved the
                // receiver is a DataFrame/GroupBy, the args are *columns/predicates*
                // (not values), so route to the unevaluated-AST ops. This is the
                // only correct disambiguation — `where`/`sort`/`min` mean different
                // things per receiver type, and column args can't compile as values.
                //
                // An *Unknown* receiver (a DataFrame from a dynamic source, e.g. an
                // untyped helper-fn parameter) routes here too whenever an argument
                // mentions a `@column`: that syntax can only mean a column verb (a
                // column reference never evaluates as a value), so this matches the
                // walker's runtime dispatch instead of mis-compiling the call as an
                // array comprehension (`where`/`filter`) or a value method (`sort`).
                if matches!(
                    n,
                    "where" | "filter" | "drop_missing" | "select" | "sort" | "group" | "with"
                ) && (matches!(self.recv_type(recv), Some(Type::DataFrame))
                    || (matches!(self.recv_type(recv), Some(Type::Unknown) | None)
                        && args.iter().any(mentions_column)))
                {
                    let eval_args = !matches!(n, "where" | "filter");
                    return self
                        .compile_df_verb_guarded(b, recv, name, args, false, eval_args, *line, *col);
                }
                // `join` mixes an evaluated DataFrame operand with by-name key columns,
                // so it can't ride the column-verb op. Compile the receiver and the
                // right operand as values (left then right on the stack), and carry the
                // keys/join-type — parsed from the unevaluated tail — in the op itself.
                if n == "join"
                    && matches!(self.recv_type(recv), Some(Type::DataFrame) | Some(Type::Unknown))
                {
                    // Evaluate the receiver first (matching the tree-walker's order, so
                    // its side effects run before any error).
                    self.compile_expr(b, recv)?;
                    match args.first() {
                        Some(other) => {
                            self.compile_expr(b, other)?;
                            b.emit(
                                Op::DfJoin { spec: std::rc::Rc::new(args[1..].to_vec()) },
                                *line,
                                *col,
                            );
                        }
                        // No operand to join with. Emit the same diagnostic the
                        // tree-walker produces, keeping the compiler total (no
                        // `Unsupported` for a type-checked program).
                        None => {
                            b.emit(
                                Op::raise(
                                    std::rc::Rc::new(
                                        "`join` needs a DataFrame to join with".to_string(),
                                    ),
                                    std::rc::Rc::new(
                                        "e.g. `samples.join(meta, sample_id)`.".to_string(),
                                    ),
                                ),
                                *line,
                                *col,
                            );
                        }
                    }
                    return Ok(());
                }
                // Same Unknown-receiver rule as step 1: `g.mean(@v)` through an
                // untyped parameter is unambiguously a GroupBy aggregation (the
                // `@column` can't be a value), so don't let it fall to the
                // value-method path, whose arg compile raises at runtime.
                if matches!(n, "mean" | "sum" | "min" | "max" | "count" | "std")
                    && (matches!(self.recv_type(recv), Some(Type::GroupBy))
                        || (matches!(self.recv_type(recv), Some(Type::Unknown) | None)
                            && args.iter().any(mentions_column)))
                {
                    return self
                        .compile_df_verb_guarded(b, recv, name, args, true, true, *line, *col);
                }

                // 2. Comprehensions compile to inline bytecode loops (no closures).
                // For an Array receiver, `where`/`filter` are comprehensions (the
                // DataFrame case was handled above).
                //
                // Fusion: a chain of eligible map/filter stages feeding a `reduce`,
                // `count`, or another stage over an idempotent Int source compiles to ONE
                // native loop with no intermediate arrays. Detected at the outermost
                // method; falls back to the per-stage path for anything ineligible.
                if !self.no_fuse
                    && matches!(n, "map" | "filter" | "where" | "reduce" | "count")
                    && let Some(plan) = self.collect_fusion_chain(b, recv, n, args)
                {
                    return self.compile_fused(b, e, plan, *line, *col);
                }
                if matches!(n, "map" | "filter" | "where" | "reduce" | "scan") {
                    return self.compile_comprehension(b, recv, name, args, *line, *col);
                }
                if matches!(n, "any" | "all") {
                    return self.compile_any_all(b, recv, name, args, *line, *col);
                }
                if n == "position" {
                    return self.compile_position(b, recv, args, *line, *col);
                }

                // 3. `select`/`group` are DataFrame-only column verbs. A
                // statically-known DataFrame was routed in step 1; reaching here
                // means the receiver type is *not* a known DataFrame (most likely
                // `Unknown` — a DataFrame from a dynamic source). Emit the column-verb
                // op, which validates the receiver at runtime (a real DataFrame
                // works; anything else raises). The type checker already rejects
                // `array.select(...)`, so a wrong concrete type can't reach here.
                if matches!(n, "select" | "group" | "with") {
                    return self
                        .compile_df_verb_guarded(b, recv, name, args, false, true, *line, *col);
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
                b.emit(
                    Op::Method(std::rc::Rc::new(MethodData {
                        name: std::rc::Rc::new(name.clone()),
                        nargs: args.len() as u32,
                    })),
                    *line,
                    *col,
                );
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
                // compiled into its own chunk; free variables that name enclosing
                // locals become upvalues captured here, anything else resolves to a
                // global. With captures it's a closure (`MakeClosure`); without, a
                // plain function value (`MakeFunc`, no per-call allocation).
                let enclosing = b.capturable_env();
                let (idx, captures) = self.compile_lambda(params, body, enclosing)?;
                let arity = params.len() as u32;
                if captures.is_empty() {
                    b.emit(Op::MakeFunc { idx, arity }, 0, 0);
                } else {
                    b.emit(
                        Op::MakeClosure(std::rc::Rc::new(MakeClosureData { idx, arity, captures })),
                        0,
                        0,
                    );
                }
            }
            // NOTE: every `Expr` variant is now handled — `compile_expr` no longer
            // has a catch-all. The remaining whole-program fallbacks live in
            // `compile_stmt` (immutable reassignment) and the `Method` verb-with-args
            // path (DataFrame/Tensor column verbs).
        }
        Ok(())
    }

}

mod comprehensions;

//! The instruction set and compiled-program representation: the `Op` enum (one
//! stack-machine instruction per variant), `Chunk` (a function body's code +
//! constants), `Program` (the whole compiled unit the VM runs), and `ReduceLoop`
//! (a JIT-eligible reduce body). The compiler builds these; the VM executes them.

use super::*;

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
    /// `match` arm: pop the scrutinee and run this pattern. On a match, push the bound
    /// values (left-to-right) then `Bool(true)`; on no match, push `Bool(false)`. The
    /// compiler declares one local per bound name and stores them after a match.
    /// Reuses the tree-walker's `pattern_match`, so the engines agree exactly (and the
    /// `missing`/literal test is a clean `Bool`, not `==`'s 3-valued `missing`).
    MatchArm(std::rc::Rc<crate::ast::Pattern>),
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
    /// Push a **closure** over `funcs[idx]`: capture the listed sources from the
    /// current frame (an enclosing local slot or one of this frame's upvalues) by
    /// value into the new closure's upvalues. Emitted instead of `MakeFunc` for a
    /// lambda that closes over enclosing locals. Boxed to keep `Op` at 16 bytes.
    MakeClosure(std::rc::Rc<MakeClosureData>),
    /// Push upvalue `idx` of the currently executing closure.
    GetUpvalue(u32),
    /// Call a function *value*: the stack holds `[func, arg0..argN-1]` (the value
    /// was loaded before the args). Reads the callee chunk from the value; errors
    /// (using the call-site `name`) if it isn't a function or the arity is wrong.
    CallValue(std::rc::Rc<CallValueData>),
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
    /// Names are interned [`crate::symbol::Symbol`]s (resolved once at compile), so
    /// building a record is an integer copy per field — no per-record allocation.
    MakeRecord(std::rc::Rc<Vec<crate::symbol::Symbol>>),
    /// Pop a receiver; push `recv.<name>` (record field access). The field name is
    /// an interned `Symbol`, so the lookup compares a `u32` against the record keys.
    GetField(crate::symbol::Symbol),
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
    Method(std::rc::Rc<MethodData>),
    /// Pop a DataFrame receiver and apply a column-verb (`where`/`filter`/`select`/
    /// `sort`/`group`) whose `args` are *unevaluated* column/predicate ASTs. Bare
    /// names resolve against the frame's columns first, then `locals` (by slot),
    /// then globals. Emitted only when the type checker proved the receiver is a
    /// DataFrame, so the args are genuinely columns, not values.
    DfColumnVerb(std::rc::Rc<DfColumnVerbData>),
    /// Pop two DataFrames — the right operand then the left receiver — and join
    /// them on shared key columns. Unlike the column verbs, the right operand is a
    /// fully evaluated value, so it is compiled and pushed normally; `spec` carries
    /// the *unevaluated* tail — the key column identifiers and an optional trailing
    /// join-type string — parsed at runtime (where diagnostics are native).
    DfJoin {
        spec: std::rc::Rc<Vec<Expr>>,
    },
    /// Pop a GroupBy receiver and apply an aggregation over one *unevaluated*
    /// column (`mean`/`sum`/`min`/`max`/`count`/`std`). Emitted only when the type
    /// checker proved the receiver is a GroupBy.
    GroupByAgg(std::rc::Rc<GroupByAggData>),
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
    /// Fast path for a JIT-eligible `arr.map(it => body)`. At this point the receiver
    /// is on the stack top. If it is an `Int` array AND a native map kernel for
    /// `kernel_idx` compiled, the VM pops it, runs the native (optionally parallel)
    /// kernel over the packed `&[i64]`, pushes the resulting `Int` array, and jumps to
    /// `after` (the comprehension's convergence point). Otherwise it falls through to
    /// the identical `CompInit` bytecode loop — so every non-`Int`, `missing`, or
    /// no-JIT case takes the oracle-matched path.
    TryJitMap { kernel_idx: u32, after: u32 },
    /// Fast path for a JIT-eligible `arr.filter(it => pred)` / `where`. Same protocol
    /// as `TryJitMap`, but the native kernel evaluates the boolean predicate per
    /// element and keeps the elements for which it holds (order preserved).
    TryJitFilter { kernel_idx: u32, after: u32 },
    /// Fast path for a fuseable `map`/`filter`/`reduce` chain (see [`FusedKernel`]). The
    /// pipeline's source operands are on the stack (an `Int` array, or `[start,end]` for
    /// a range; plus the `init` for a `Reduce` sink). If they are all `Int` (range within
    /// the 100M cap) and a fused kernel compiled, the VM runs the single native loop,
    /// pushes the result (an `Int` array for `Collect`, a scalar for `Reduce`), and jumps
    /// to `after`. Otherwise it pops the same operands and falls through to the ordinary
    /// per-stage chain compilation (the oracle path).
    TryJitFused { kernel_idx: u32, after: u32 },
    /// Raise a runtime error with the given message and hint. Used where the
    /// program is statically known to be an error but the error should still fire
    /// at the point of execution (e.g. reassigning an immutable global, after its
    /// value expression — and any side effects — have run).
    Raise(std::rc::Rc<RaiseData>),
    /// `try EXPR` — push an error handler recording the current stack/frame/local/
    /// iterator depths. If ANY op errors before the matching `TryOk`, the VM unwinds
    /// to those depths and jumps to `catch_ip` (a `TryErr`) with the error message
    /// pushed on the stack — so `try` runs natively on the VM, no tree-walker.
    TryBegin(u32),
    /// Normal completion of a `try` body: pop the handler, wrap the result value
    /// (top of stack) in the ok-record `{ok:true, value, error:missing}`, and jump
    /// past the catch to `end_ip`.
    TryOk(u32),
    /// The catch target: wrap the caught error message (top of stack) in the
    /// err-record `{ok:false, value:missing, error:msg}`.
    TryErr,
    /// Discard the top of the stack (an expression-statement's value).
    Pop,
    /// Return the top of the stack from the current function frame.
    Return,
}

/// The payload of [`Op::DfColumnVerb`] (the only 3-field op), boxed behind one
/// `Rc` so `Op` stays small — the dispatch loop streams every instruction, so the
/// enum's width is a hot-path constant. DataFrame verbs are emitted at most once
/// per source occurrence and never in hot loops, so the extra indirection is free.
#[derive(Debug, Clone)]
pub struct DfColumnVerbData {
    pub name: std::rc::Rc<String>,
    pub args: std::rc::Rc<Vec<Expr>>,
    pub locals: std::rc::Rc<Vec<(String, u32)>>,
}

/// Payload of [`Op::Method`] — a value-method call's name and argument count, boxed
/// so `Op` stays small (the method name is shared across call sites of the same name).
#[derive(Debug, Clone)]
pub struct MethodData {
    pub name: std::rc::Rc<String>,
    pub nargs: u32,
}

/// Payload of [`Op::CallValue`] — a function-value call's argument count and the
/// call-site name (for error messages), boxed to keep `Op` small.
#[derive(Debug, Clone)]
pub struct CallValueData {
    pub nargs: u32,
    pub name: std::rc::Rc<String>,
}

/// Where a closure upvalue is captured from, in the *enclosing* frame's terms:
/// a local stack slot, or one of the enclosing closure's own upvalues. Resolved
/// at compile time; consumed by `Op::MakeClosure` at runtime.
#[derive(Debug, Clone, Copy)]
pub enum CaptureSrc {
    Local(u32),
    Upvalue(u32),
}

/// Payload of [`Op::MakeClosure`] — the chunk index, arity, and the ordered list
/// of capture sources used to build the new closure's upvalues.
#[derive(Debug, Clone)]
pub struct MakeClosureData {
    pub idx: u32,
    pub arity: u32,
    pub captures: Vec<CaptureSrc>,
}

/// Payload of [`Op::GroupByAgg`] — a grouped-aggregation's name and unevaluated
/// column argument, boxed to keep `Op` small.
#[derive(Debug, Clone)]
pub struct GroupByAggData {
    pub name: std::rc::Rc<String>,
    pub args: std::rc::Rc<Vec<Expr>>,
}

/// Payload of [`Op::Raise`] — a deferred error's message and hint, boxed to keep
/// `Op` small (this is a cold error-raising op, never in a hot loop).
#[derive(Debug, Clone)]
pub struct RaiseData {
    pub msg: std::rc::Rc<String>,
    pub hint: std::rc::Rc<String>,
}

impl Op {
    /// Construct a boxed [`Op::Raise`]; keeps the seven emit sites a plain call.
    pub fn raise(msg: std::rc::Rc<String>, hint: std::rc::Rc<String>) -> Op {
        Op::Raise(std::rc::Rc::new(RaiseData { msg, hint }))
    }
}

// The bytecode dispatch loop reads one `Op` per executed instruction (millions of
// times in a hot recursion), and a program's `code` is `Vec<Op>` — so `Op`'s width
// is both a hot-path and a memory constant. Keep the fat/rare variants boxed.
const _: () = assert!(
    std::mem::size_of::<Op>() <= 16,
    "Op grew past 2 words — box the offending variant (see DfColumnVerbData)",
);

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

/// A JIT-eligible `map`/`filter` body the compiler asked the JIT to compile into a
/// native per-element kernel over a packed `&[i64]`. For `map` the JIT lowers `body`
/// to a fused loop `dst[i] = body(src[i])`; for `filter` to a compaction loop keeping
/// `src[i]` where `body(src[i])` holds. Indexed by the `kernel_idx` of the matching
/// [`Op::TryJitMap`] / [`Op::TryJitFilter`].
#[derive(Debug, Clone)]
pub struct ArrayKernel {
    /// The element binder name (the lambda's single parameter).
    pub binder: String,
    /// The body (a value expression for `map`, a boolean predicate for `filter`),
    /// evaluated over `{binder}` as `i64`.
    pub body: Expr,
}

/// One stage of a fused pipeline (`xs.filter(g).map(f)…`). Each is a pure single-binder
/// `i64` transform the JIT threads through one native loop with no intermediate array.
#[derive(Debug, Clone)]
pub enum FusionStage {
    /// `cur = body(cur)`.
    Map { binder: String, body: Expr },
    /// keep `cur` only if `body(cur)` holds, else skip to the next element.
    Filter { binder: String, body: Expr },
}

/// What a fused pipeline produces.
#[derive(Debug, Clone)]
pub enum FusionSink {
    /// Build one output `Int` array from the surviving elements.
    Collect,
    /// Count the surviving elements (`.count()` after a filter) — allocates nothing.
    Count,
    /// Fold the surviving elements to a scalar with wrapping `i64` arithmetic (the
    /// explicit-accumulator `reduce`, matching the existing reduce-loop semantics).
    Reduce { pa: String, pb: String, body: Expr },
}

/// A fuseable linear pipeline the compiler asked the JIT to lower into a single native
/// loop: a source (an `Int` array, or a `range` counter), a chain of [`FusionStage`]s,
/// and a [`FusionSink`]. Indexed by the `kernel_idx` of [`Op::TryJitFused`].
#[derive(Debug, Clone)]
pub struct FusedKernel {
    /// `true` if the source is a `range(start,end)` counter (no input array); otherwise
    /// the source is an `Int` array passed in.
    pub source_is_range: bool,
    pub stages: Vec<FusionStage>,
    pub sink: FusionSink,
}

impl FusedKernel {
    /// How many operands `compile_fused` pushes for this shape (range: `start,end`;
    /// array: the array; plus the `init` value for a `Reduce` sink). The VM consumes
    /// exactly this many whether it takes the native path or falls through.
    pub fn n_operands(&self) -> usize {
        let src = if self.source_is_range { 2 } else { 1 };
        src + matches!(self.sink, FusionSink::Reduce { .. }) as usize
    }
}

/// One compiled code unit: a function body or the top-level `main`.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub code: Vec<Op>,
    pub consts: Vec<Value>,
    /// Source `(line, col)` per instruction, for error reporting. `u32` (not
    /// `usize`) halves this side-table — line/col never exceed `u32` in real source.
    pub pos: Vec<(u32, u32)>,
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
    /// JIT-eligible `map` bodies, indexed by [`Op::TryJitMap::kernel_idx`]. Handed to
    /// [`crate::jit::build`] (same single-source-of-truth contract as `reduce_loops`).
    pub map_kernels: Vec<ArrayKernel>,
    /// JIT-eligible `filter`/`where` predicates, indexed by
    /// [`Op::TryJitFilter::kernel_idx`].
    pub filter_kernels: Vec<ArrayKernel>,
    /// Fuseable `map`/`filter`/`reduce` pipelines, indexed by [`Op::TryJitFused`]'s
    /// `kernel_idx` — each lowered to a single intermediate-free native loop.
    pub fused_kernels: Vec<FusedKernel>,
    /// Global slot names, aligned with `global_init`. Lets the VM resolve a bare
    /// name to a global's runtime value inside a DataFrame predicate
    /// (`df.where(age > threshold)`) — the column-verb `resolve_var`.
    pub global_names: std::rc::Rc<Vec<String>>,
}

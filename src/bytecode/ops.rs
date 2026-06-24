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

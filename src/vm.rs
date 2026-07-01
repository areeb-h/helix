//! Stack-machine virtual machine that executes [`crate::bytecode`] programs.
//!
//! The hot loop is flat: fetch an [`Op`], advance the instruction pointer, act.
//! Compared with the tree-walker it removes two structural costs — repeated AST
//! traversal and per-variable `String` hashing — by working over pre-resolved
//! slot indices and a contiguous operand stack.
//!
//! Crucially, **recursion lives on the heap, not the native stack**: a call
//! pushes a [`Frame`] onto a `Vec`, so recursion is bounded only by memory (with
//! a high guard that turns a runaway into a clean error). This is the proper
//! fix to the depth limit the tree-walker needs a 2 GiB thread to paper over.
//!
//! Semantics are inherited wholesale from the interpreter: arithmetic, unary
//! ops, boolean three-valued logic, and builtins all route through the very same
//! functions the tree-walker uses, so the VM is observationally identical.

use std::collections::HashMap;

use crate::ast::BinOp;
use crate::bytecode::{Op, Program};
use crate::error::HelixError;
use crate::interp::{as_bool, eval_binary, tri, Interp};
use crate::value::Value;

/// Scalar fast path for binary operators — the overwhelmingly common case in
/// hot loops. Skips `eval_binary`'s `missing` test and array/tensor broadcasting
/// dispatch for plain `Int`/`Float` operands, falling back to the full
/// implementation for everything else. Each fast case is byte-for-byte identical
/// to what `arith` / `compare` / `values_equal` produce (verified by the parity
/// tests), including release-mode integer wrap and the `f64`-based integer
/// ordering the tree-walker already uses.
#[inline(always)]
fn binary(op: &BinOp, a: Value, b: Value, line: usize, col: usize) -> Result<Value, HelixError> {
    use BinOp::*;
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => {
            let (x, y) = (*x, *y);
            match op {
                // wrapping + exact i64 ordering — identical to `arith`/`compare`
                // and to the JIT (the f64-cast ordering was lossy above 2^53).
                Add => return Ok(Value::Int(x.wrapping_add(y))),
                Sub => return Ok(Value::Int(x.wrapping_sub(y))),
                Mul => return Ok(Value::Int(x.wrapping_mul(y))),
                Lt => return Ok(Value::Bool(x < y)),
                Gt => return Ok(Value::Bool(x > y)),
                Le => return Ok(Value::Bool(x <= y)),
                Ge => return Ok(Value::Bool(x >= y)),
                Eq => return Ok(Value::Bool(x == y)),
                Ne => return Ok(Value::Bool(x != y)),
                // Mod/Div with a nonzero divisor match `arith`/`eval_binary`
                // exactly (Int `%` stays Int via rem_euclid; `/` is always Float);
                // a zero divisor falls through to the error-raising full path.
                Mod if y != 0 => return Ok(Value::Int(x.rem_euclid(y))),
                FloorDiv if y != 0 => return Ok(Value::Int(x.div_euclid(y))),
                Div if y != 0 => return Ok(Value::Float(x as f64 / y as f64)),
                // Integer bitwise — identical to `bitwise()` in ops.rs. Shifts only
                // shortcut for an in-range amount; an out-of-range shift falls to the
                // full path, which raises (never a panic/UB).
                BitAnd => return Ok(Value::Int(x & y)),
                BitOr => return Ok(Value::Int(x | y)),
                BitXor => return Ok(Value::Int(x ^ y)),
                Shl if (0..=63).contains(&y) => return Ok(Value::Int(x << y)),
                Shr if (0..=63).contains(&y) => return Ok(Value::Int(x >> y)),
                _ => {} // Div/Mod by zero, Pow, out-of-range shift → full path
            }
        }
        (Value::Float(x), Value::Float(y)) => {
            let (x, y) = (*x, *y);
            match op {
                Add => return Ok(Value::Float(x + y)),
                Sub => return Ok(Value::Float(x - y)),
                Mul => return Ok(Value::Float(x * y)),
                Eq => return Ok(Value::Bool(x == y)),
                Ne => return Ok(Value::Bool(x != y)),
                // Float `%` never errors on zero (yields NaN), matching `eval_binary`.
                Mod => return Ok(Value::Float(x.rem_euclid(y))),
                FloorDiv if y != 0.0 => return Ok(Value::Float(x.div_euclid(y))),
                Div if y != 0.0 => return Ok(Value::Float(x / y)),
                // Float ordering can hit NaN, which the full path turns into an
                // error — so don't shortcut comparisons here.
                _ => {}
            }
        }
        _ => {}
    }
    eval_binary(op, a, b, line, col)
}

/// Heap-frame recursion guard. Far higher than the tree-walker's stack-bound
/// limit (frames are small heap allocations), yet still catches infinite
/// recursion long before it can exhaust memory.
const VM_MAX_DEPTH: usize = 1_000_000;

/// Upper bound on the memoization table, so a pathological program can't grow it
/// without limit; beyond this we simply stop caching (results stay correct, just
/// uncached). Memoizable functions have few distinct calls by design, so this is
/// only a safety backstop.
const MEMO_MAX_ENTRIES: usize = 5_000_000;

/// One scalar argument as a memo key. Floats are keyed by their **bit pattern**
/// (`to_bits`), which is exactly correct: bit-identical floats are the same value
/// (so the cached result is valid), distinct bits are distinct keys (so `+0.0`
/// vs `-0.0` are simply computed separately), and a NaN keys consistently against
/// itself. This makes pure float recursion (e.g. `fibf`) memoizable too.
#[derive(Hash, PartialEq, Eq, Clone)]
enum MemoArg {
    Int(i64),
    Float(u64),
}

/// Key into the memo table: a function index plus its scalar arguments. The 1- and
/// 2-argument cases (the overwhelming majority — `fib(n)`, `ackermann(m,n)`, …) are
/// stored **inline** so a memoizable call needs no heap allocation for its key; only
/// 3+ args fall back to a `Vec`.
#[derive(Hash, PartialEq, Eq, Clone)]
enum MemoKey {
    A1(usize, MemoArg),
    A2(usize, MemoArg, MemoArg),
    An(usize, Vec<MemoArg>),
}

/// Project a (gated-scalar) argument value into a hashable memo argument.
fn memo_arg(v: &Value) -> MemoArg {
    match v {
        Value::Int(n) => MemoArg::Int(*n),
        Value::Float(f) => MemoArg::Float(f.to_bits()),
        _ => MemoArg::Int(0), // unreachable: gated by all_scalar
    }
}

/// What a comprehension iterates: a materialized array, or — for a fused
/// `range(...)` — a lazy integer counter (no array allocated at all).
enum CompSource {
    Array { arr: std::rc::Rc<crate::value::ArrayData>, idx: usize },
    Range { cur: i64, end: i64 },
}

/// Active comprehension iterator state (a stack, so comprehensions nest).
/// `cur_val` is the element just yielded (used by `filter`); `builder` collects
/// results for `map`/`filter` and is ignored by `reduce`.
struct CompIter {
    source: CompSource,
    cur_val: Value,
    builder: crate::value::ColumnBuilder,
}

/// One active function invocation.
struct Frame {
    /// Index into [`Program::funcs`] of the code being run.
    func: usize,
    /// Instruction pointer within that function's `code`.
    ip: usize,
    /// Start of this frame's locals within the shared `locals` stack.
    base: usize,
    /// If set, this call is a memoization miss: store its return value under this
    /// key when the frame returns.
    memo_key: Option<MemoKey>,
    /// The captured environment of the closure being run (empty for a plain
    /// function). `GetUpvalue` reads these; `MakeClosure` may copy from them.
    upvalues: std::rc::Rc<Vec<Value>>,
}

/// An active `try` error handler (from `Op::TryBegin`). Records the exact depths of
/// every VM stack at the point the `try` was entered, so an error anywhere inside —
/// however deeply nested the call chain — unwinds back to here and resumes at
/// `catch_ip`. A `Vec` of these is a LIFO stack, so the innermost `try` catches first.
struct Handler {
    stack_len: usize,
    frame_depth: usize,
    locals_len: usize,
    iters_len: usize,
    catch_ip: usize,
}

/// Read a tuple/record accumulator's N `i64` slots into a buffer (field/element order),
/// or `None` if it isn't an N-element Tuple/Record of all `Int`s — the marshalling for the
/// native multi-slot fold kernel (tuple and record accumulators share the kernel).
fn acc_to_slots(v: &Value, n: usize) -> Option<Vec<i64>> {
    let items: &[Value] = match v {
        Value::Tuple(t) if t.len() == n => t,
        Value::Record(r) if r.len() == n => {
            let mut buf = Vec::with_capacity(n);
            for (_, el) in r.iter() {
                match el {
                    Value::Int(i) => buf.push(*i),
                    _ => return None,
                }
            }
            return Some(buf);
        }
        _ => return None,
    };
    let mut buf = Vec::with_capacity(n);
    for el in items.iter() {
        match el {
            Value::Int(i) => buf.push(*i),
            _ => return None,
        }
    }
    Some(buf)
}

/// Rebuild an accumulator value from the folded slots, matching `template`'s shape: a
/// Record reuses its field symbols (in order), anything else becomes a Tuple.
fn rebuild_acc(template: &Value, buf: Vec<i64>) -> Value {
    match template {
        Value::Record(r) => {
            let fields: Vec<(crate::symbol::Symbol, Value)> =
                r.iter().zip(buf).map(|((sym, _), i)| (*sym, Value::Int(i))).collect();
            Value::Record(std::rc::Rc::new(fields))
        }
        _ => Value::Tuple(std::rc::Rc::new(buf.into_iter().map(Value::Int).collect())),
    }
}

/// Like [`acc_to_slots`], but for an **all-`Float`** accumulator: each slot's f64 bit pattern
/// is packed into the `i64` buffer (the f64-tuple kernel reads/writes these 8-byte slots as
/// `f64` at the same addresses). A non-`Float` slot falls back. Keeping the buffer `Vec<i64>`
/// reuses the existing tuple-reduce runner ABI (`*mut i64`) unchanged.
fn acc_to_slots_f64(v: &Value, n: usize) -> Option<Vec<i64>> {
    let items: &[Value] = match v {
        Value::Tuple(t) if t.len() == n => t,
        Value::Record(r) if r.len() == n => {
            let mut buf = Vec::with_capacity(n);
            for (_, el) in r.iter() {
                match el {
                    Value::Float(f) => buf.push(f.to_bits() as i64),
                    _ => return None,
                }
            }
            return Some(buf);
        }
        _ => return None,
    };
    let mut buf = Vec::with_capacity(n);
    for el in items.iter() {
        match el {
            Value::Float(f) => buf.push(f.to_bits() as i64),
            _ => return None,
        }
    }
    Some(buf)
}

/// Rebuild an **all-`Float`** accumulator from the folded slot bit patterns (`f64::from_bits`),
/// matching `template`'s shape (Record reuses its field symbols, else Tuple).
fn rebuild_acc_f64(template: &Value, buf: Vec<i64>) -> Value {
    let f = |i: i64| Value::Float(f64::from_bits(i as u64));
    match template {
        Value::Record(r) => {
            let fields: Vec<(crate::symbol::Symbol, Value)> =
                r.iter().zip(buf).map(|((sym, _), i)| (*sym, f(i))).collect();
            Value::Record(std::rc::Rc::new(fields))
        }
        _ => Value::Tuple(std::rc::Rc::new(buf.into_iter().map(f).collect())),
    }
}

/// Execute a compiled program, printing output and returning the first runtime
/// error (if any) for the caller to render. `jit`, when present, supplies native
/// code for eligible integer functions.
pub fn run(program: &Program, jit: Option<&crate::jit::Jit>) -> Result<(), HelixError> {
    exec(program, jit).map(|_| ())
}

/// The execution core. Returns the final operand stack so tests can inspect the
/// last computed value; [`run`] discards it.
fn exec(program: &Program, jit: Option<&crate::jit::Jit>) -> Result<Vec<Value>, HelixError> {
    // Resolve each user function index to its native entry points once (if
    // JIT'd), so the hot `CallFn` path is a single array lookup.
    let jit_for_idx: Vec<Option<crate::jit::NativeFn>> = program
        .func_names
        .iter()
        .map(|n| jit.and_then(|j| j.lookup(n)))
        .collect();
    // A throwaway interpreter purely as the host for builtin dispatch (`print`,
    // math fns, `read_csv`, …) — builtins are pure functions of their args.
    let mut host = Interp::new();

    let mut globals: Vec<Value> = program.global_init.clone();
    let mut stack: Vec<Value> = Vec::with_capacity(256);
    let mut locals: Vec<Value> = Vec::with_capacity(256);
    let mut frames: Vec<Frame> = Vec::with_capacity(64);
    // Automatic memoization of pure, overlapping-recursive functions — the
    // "under the hood" cache. Safe because memoizable functions (per the
    // bytecode analysis) are pure functions of their integer arguments.
    let mut memo: HashMap<MemoKey, Value> = HashMap::new();
    let mut iters: Vec<CompIter> = Vec::new();
    // Active `try` handlers (LIFO). Empty in the overwhelmingly common case; an
    // error only consults it, so non-`try` programs pay nothing.
    let mut handlers: Vec<Handler> = Vec::new();
    // Shared empty upvalue list for every non-closure frame — cloning is a refcount
    // bump, so a plain call allocates nothing for upvalues.
    let no_upvalues: std::rc::Rc<Vec<Value>> = std::rc::Rc::new(Vec::new());

    let main = &program.funcs[0];
    locals.resize(main.n_locals as usize, Value::Unit);
    frames.push(Frame { func: 0, ip: 0, base: 0, memo_key: None, upvalues: no_upvalues.clone() });

    loop {
        let fi = frames.len() - 1;
        let func = frames[fi].func;
        let chunk = &program.funcs[func];
        let ip = frames[fi].ip;
        if ip >= chunk.code.len() {
            // Only `main` runs off the end (functions terminate with `Return`).
            break;
        }
        // Borrow the instruction rather than cloning it — this loop runs once
        // per executed op (tens of millions of times in a hot recursion), so a
        // per-dispatch clone is pure waste.
        let op = &chunk.code[ip];
        let (line, col) = {
            let (l, c) = chunk.pos[ip];
            (l as usize, c as usize)
        };
        frames[fi].ip = ip + 1; // default advance; control-flow ops overwrite it

        // Run the op inside a closure so an error can be CAUGHT (routed to the
        // nearest `try` handler) instead of propagating straight out of `exec`. An
        // in-arm `continue` becomes `return Ok(())` (nothing runs after the match, so
        // they're equivalent); a `return Err(..)` becomes the closure's error. LLVM
        // inlines this single-call closure, so the dispatch loop stays zero-overhead.
        let step: Result<(), HelixError> = (|| {
        match op {
            Op::Const(k) => stack.push(chunk.consts[*k as usize].clone()),
            Op::LoadLocal(slot) => {
                let base = frames[fi].base;
                stack.push(locals[base + *slot as usize].clone());
            }
            Op::StoreLocal(slot) => {
                let base = frames[fi].base;
                locals[base + *slot as usize] = stack.pop().unwrap();
            }
            Op::LoadGlobal(i) => stack.push(globals[*i as usize].clone()),
            Op::StoreGlobal(i) => globals[*i as usize] = stack.pop().unwrap(),
            Op::Unary(o) => {
                let v = stack.pop().unwrap();
                stack.push(host.eval_unary(o, v, line, col)?);
            }
            Op::Binary(o) => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(binary(o, a, b, line, col)?);
            }
            Op::LoadLocalBinary(a, c, o) => {
                let base = frames[fi].base;
                let av = locals[base + *a as usize].clone();
                let cv = locals[base + *c as usize].clone();
                stack.push(binary(o, av, cv, line, col)?);
            }
            Op::LoadLocalConstBinary(a, k, o) => {
                let base = frames[fi].base;
                let av = locals[base + *a as usize].clone();
                let cv = chunk.consts[*k as usize].clone();
                stack.push(binary(o, av, cv, line, col)?);
            }
            Op::ConstBinary(k, o) => {
                let v = stack.pop().unwrap();
                let cv = chunk.consts[*k as usize].clone();
                stack.push(binary(o, v, cv, line, col)?);
            }
            Op::Jump(t) => frames[fi].ip = *t as usize,
            Op::MatchArm(pat) => {
                // Reuse the tree-walker's matcher so the engines agree exactly. On a
                // match, push the bound values then `true`; else push `false`.
                let v = stack.pop().unwrap();
                match crate::interp::pattern_match(pat, &v) {
                    Some(binds) => {
                        for (_, val) in binds {
                            stack.push(val);
                        }
                        stack.push(Value::Bool(true));
                    }
                    None => stack.push(Value::Bool(false)),
                }
            }
            Op::JumpIfFalse(t) => {
                let c = stack.pop().unwrap();
                if matches!(c, Value::Missing) {
                    return Err(HelixError::new(
                        "`if` condition is `missing` — cannot choose a branch",
                        line,
                        col,
                    )
                    .hint("handle the missing case first, e.g. `if x.is_missing() then ... else ...`."));
                }
                let taken = as_bool(&c, line, col).map_err(|e| {
                    e.hint("an `if` condition must be a boolean, e.g. `if x > 0 then ... else ...`.")
                })?;
                if !taken {
                    frames[fi].ip = *t as usize;
                }
            }
            // Three-valued `and`: short-circuit on a determined `false`.
            Op::AndCheck(end) => {
                let ta = tri(stack.last().unwrap(), line, col)?;
                if ta == Some(false) {
                    *stack.last_mut().unwrap() = Value::Bool(false);
                    frames[fi].ip = *end as usize;
                }
            }
            Op::AndCombine => {
                let rb = stack.pop().unwrap();
                let ra = stack.pop().unwrap();
                let tb = tri(&rb, line, col)?;
                let ta = tri(&ra, line, col)?;
                stack.push(match (ta, tb) {
                    (_, Some(false)) => Value::Bool(false),
                    (Some(true), Some(true)) => Value::Bool(true),
                    _ => Value::Missing,
                });
            }
            // Three-valued `or`: short-circuit on a determined `true`.
            Op::OrCheck(end) => {
                let ta = tri(stack.last().unwrap(), line, col)?;
                if ta == Some(true) {
                    *stack.last_mut().unwrap() = Value::Bool(true);
                    frames[fi].ip = *end as usize;
                }
            }
            Op::OrCombine => {
                let rb = stack.pop().unwrap();
                let ra = stack.pop().unwrap();
                let tb = tri(&rb, line, col)?;
                let ta = tri(&ra, line, col)?;
                stack.push(match (ta, tb) {
                    (_, Some(true)) => Value::Bool(true),
                    (Some(false), Some(false)) => Value::Bool(false),
                    _ => Value::Missing,
                });
            }
            // `a ?? b`: keep `a` unless it's missing, in which case evaluate `b`.
            Op::CoalesceCheck(end) => {
                if matches!(stack.last().unwrap(), Value::Missing) {
                    stack.pop(); // drop the missing; fall through to `b`
                } else {
                    frames[fi].ip = *end as usize; // keep `a`, skip `b`
                }
            }
            Op::CallBuiltin { idx, nargs } => {
                let split = stack.len() - *nargs as usize;
                let args = stack.split_off(split);
                let name = &program.builtins[*idx as usize];
                let v = host.call_builtin(name, args, line, col)?;
                stack.push(v);
            }
            Op::CallFn { idx, nargs } => {
                let idx = *idx as usize;
                let nargs = *nargs as usize;
                let start = stack.len() - nargs;
                let all_int = stack[start..].iter().all(|v| matches!(v, Value::Int(_)));
                let all_scalar = stack[start..]
                    .iter()
                    .all(|v| matches!(v, Value::Int(_) | Value::Float(_)));

                // Memoization fast path (preferred over the JIT for the pure,
                // overlapping-recursive functions the analysis flagged): a cache
                // hit returns instantly; a miss runs the bytecode body so its
                // recursive calls also hit this path, then stores the result on
                // return. This turns exponential recursion (e.g. `fib`) linear —
                // for integer *and* float arguments.
                if program.memoizable[idx] && all_scalar {
                    let kargs = &stack[start..];
                    let key = match kargs.len() {
                        1 => MemoKey::A1(idx, memo_arg(&kargs[0])),
                        2 => MemoKey::A2(idx, memo_arg(&kargs[0]), memo_arg(&kargs[1])),
                        _ => MemoKey::An(idx, kargs.iter().map(memo_arg).collect()),
                    };
                    if let Some(cached) = memo.get(&key) {
                        let cached = cached.clone();
                        stack.truncate(start);
                        stack.push(cached);
                        return Ok(());
                    }
                    let callee = &program.funcs[idx];
                    if nargs != callee.n_params as usize {
                        let np = callee.n_params as usize;
                        return Err(HelixError::new(
                            format!(
                                "`{}` expects {} argument{}, got {}",
                                program.func_names[idx],
                                np,
                                if np == 1 { "" } else { "s" },
                                nargs
                            ),
                            line,
                            col,
                        ));
                    }
                    if frames.len() >= VM_MAX_DEPTH {
                        return Err(HelixError::new(
                            format!("maximum recursion depth ({}) exceeded", VM_MAX_DEPTH),
                            line,
                            col,
                        )
                        .hint("is the recursion missing a base case, or should this be a loop/comprehension?"));
                    }
                    let base = locals.len();
                    locals.extend(stack.drain(start..));
                    locals.resize(base + callee.n_locals as usize, Value::Unit);
                    frames.push(Frame { func: idx, ip: 0, base, memo_key: Some(key), upvalues: no_upvalues.clone() });
                    return Ok(());
                }

                // Native fast path: dispatch to the specialization matching the
                // argument types. All-Int + an i64 version → native i64 (Int
                // result). Otherwise all-numeric + an f64 version → native f64
                // (Float result; the float-only or mixed/float case). All of the
                // function's internal recursion then stays native.
                if let Some(nf) = jit_for_idx[idx]
                    && nargs == nf.arity
                {
                    let tail = &stack[start..];
                    // The f64 specialization always returns Float, so it is only
                    // valid when EVERY argument is Float (then every op, and
                    // returning a param, yields Float — matching the interpreter).
                    // For MIXED Int/Float args the result type depends on what the
                    // function does (e.g. `f(a,b)=b` keeps an Int `b`), so those
                    // fall through to the VM, which handles type-mixing correctly.
                    let all_float = tail.iter().all(|v| matches!(v, Value::Float(_)));
                    if all_int && let Some(ptr) = nf.i64_ptr {
                        let iargs: Vec<i64> = tail
                            .iter()
                            .map(|v| if let Value::Int(n) = v { *n } else { 0 })
                            .collect();
                        stack.truncate(start);
                        let r = unsafe { crate::jit::call_i64(ptr, &iargs) };
                        stack.push(Value::Int(r));
                        return Ok(());
                    }
                    if all_float && let Some(ptr) = nf.f64_ptr {
                        let fargs: Vec<f64> = tail.iter().map(|v| v.as_f64().unwrap()).collect();
                        stack.truncate(start);
                        let r = unsafe { crate::jit::call_f64(ptr, &fargs) };
                        stack.push(Value::Float(r));
                        return Ok(());
                    }
                }
                let callee = &program.funcs[idx];
                if nargs != callee.n_params as usize {
                    let np = callee.n_params as usize;
                    return Err(HelixError::new(
                        format!(
                            "`{}` expects {} argument{}, got {}",
                            program.func_names[idx],
                            np,
                            if np == 1 { "" } else { "s" },
                            nargs
                        ),
                        line,
                        col,
                    ));
                }
                if frames.len() >= VM_MAX_DEPTH {
                    return Err(HelixError::new(
                        format!("maximum recursion depth ({}) exceeded", VM_MAX_DEPTH),
                        line,
                        col,
                    )
                    .hint("is the recursion missing a base case, or should this be a loop/comprehension?"));
                }
                let base = locals.len();
                // `drain` moves the args straight into the callee's locals with
                // no intermediate allocation (vs `split_off`'s fresh `Vec`).
                locals.extend(stack.drain(start..));
                locals.resize(base + callee.n_locals as usize, Value::Unit);
                frames.push(Frame { func: idx, ip: 0, base, memo_key: None, upvalues: no_upvalues.clone() });
            }
            Op::MakeFunc { idx, arity } => {
                stack.push(Value::VmFunc { idx: *idx, arity: *arity });
            }
            Op::MakeClosure(d) => {
                // Capture each upvalue's value from the current frame: an enclosing
                // local (`base + slot`) or one of this frame's own upvalues.
                let frame = frames.last().unwrap();
                let captured: Vec<Value> = d
                    .captures
                    .iter()
                    .map(|src| match src {
                        crate::bytecode::CaptureSrc::Local(slot) => {
                            locals[frame.base + *slot as usize].clone()
                        }
                        crate::bytecode::CaptureSrc::Upvalue(i) => {
                            frame.upvalues[*i as usize].clone()
                        }
                    })
                    .collect();
                stack.push(Value::Closure(std::rc::Rc::new(crate::value::ClosureData {
                    idx: d.idx,
                    arity: d.arity,
                    upvalues: std::rc::Rc::new(captured),
                })));
            }
            Op::GetUpvalue(i) => {
                let v = frames.last().unwrap().upvalues[*i as usize].clone();
                stack.push(v);
            }
            Op::CallValue(d) => {
                let name = &d.name;
                let nargs = d.nargs as usize;
                let start = stack.len() - nargs;
                // The function value sits just below the args (loaded first). A
                // plain `VmFunc` has no upvalues; a `Closure` carries its captured
                // environment, which becomes the new frame's upvalues.
                let (idx, frame_upvalues) = match &stack[start - 1] {
                    Value::VmFunc { idx, .. } => (*idx as usize, no_upvalues.clone()),
                    Value::Closure(c) => (c.idx as usize, c.upvalues.clone()),
                    other => {
                        return Err(HelixError::new(
                            format!("`{}` is a {}, not a function", name, other.type_name()),
                            line,
                            col,
                        )
                        .hint("only functions and the built-ins `print`/`dna`/`range` can be called."));
                    }
                };
                let callee = &program.funcs[idx];
                if nargs != callee.n_params as usize {
                    let np = callee.n_params as usize;
                    return Err(HelixError::new(
                        format!(
                            "`{}` expects {} argument{}, got {}",
                            name,
                            np,
                            if np == 1 { "" } else { "s" },
                            nargs
                        ),
                        line,
                        col,
                    ));
                }
                if frames.len() >= VM_MAX_DEPTH {
                    return Err(HelixError::new(
                        format!("maximum recursion depth ({}) exceeded", VM_MAX_DEPTH),
                        line,
                        col,
                    )
                    .hint("is the recursion missing a base case, or should this be a loop/comprehension?"));
                }
                let base = locals.len();
                locals.extend(stack.drain(start..)); // args → callee locals
                stack.pop(); // discard the function value (now on top)
                locals.resize(base + callee.n_locals as usize, Value::Unit);
                frames.push(Frame { func: idx, ip: 0, base, memo_key: None, upvalues: frame_upvalues });
            }
            Op::Return => {
                let ret = stack.pop().unwrap();
                let frame = frames.pop().unwrap();
                locals.truncate(frame.base);
                // A memoization miss: record the result so future calls with the
                // same arguments return instantly (bounded for safety).
                if let Some(key) = frame.memo_key
                    && memo.len() < MEMO_MAX_ENTRIES {
                        memo.insert(key, ret.clone());
                    }
                stack.push(ret);
            }
            Op::MakeArray(n) => {
                let start = stack.len() - *n as usize;
                let items: Vec<Value> = stack.split_off(start);
                stack.push(Value::array_sniff(items));
            }
            Op::Index => {
                let idx = stack.pop().unwrap();
                let recv = stack.pop().unwrap();
                stack.push(crate::interp::eval_index(&recv, &idx, line, col)?);
            }
            Op::Interp(parts) => {
                let holes = parts
                    .iter()
                    .filter(|p| matches!(p, crate::ast::InterpPart::Expr(..)))
                    .count();
                let vals: Vec<Value> = stack.split_off(stack.len() - holes);
                let mut s = String::new();
                let mut vi = 0;
                for part in parts.iter() {
                    match part {
                        crate::ast::InterpPart::Lit(t) => s.push_str(t),
                        crate::ast::InterpPart::Expr(_, spec) => {
                            match spec {
                                Some(fs) => s.push_str(
                                    &fs.apply(&vals[vi]).map_err(|m| HelixError::new(m, line, col))?,
                                ),
                                // Hot path: format scalars straight into `s`, no throwaway String.
                                None => crate::value::write_value(&mut s, &vals[vi], line, col)?,
                            }
                            vi += 1;
                        }
                    }
                    // Mirror the tree-walker's cap so a doubling loop errors cleanly
                    // and identically on both engines (parity) instead of aborting.
                    if s.len() > crate::interp::MAX_STRING_LEN {
                        return Err(HelixError::new(
                            format!(
                                "interpolated string exceeds {} bytes",
                                crate::interp::MAX_STRING_LEN
                            ),
                            line,
                            col,
                        )
                        .hint("build large text incrementally or write it to a file instead."));
                    }
                }
                stack.push(Value::Str(std::rc::Rc::new(s)));
            }
            Op::MakeTuple(n) => {
                let start = stack.len() - *n as usize;
                let items: Vec<Value> = stack.split_off(start);
                stack.push(Value::Tuple(std::rc::Rc::new(items)));
            }
            Op::MakeRecord(names) => {
                let start = stack.len() - names.len();
                let vals: Vec<Value> = stack.split_off(start);
                // `names.iter().copied()` copies the interned `Symbol`s (a `u32`
                // each) — no per-record key allocation.
                let fields: Vec<(crate::symbol::Symbol, Value)> =
                    names.iter().copied().zip(vals).collect();
                stack.push(Value::Record(std::rc::Rc::new(fields)));
            }
            Op::UpdateRecord(names) => {
                let start = stack.len() - names.len();
                let vals: Vec<Value> = stack.split_off(start);
                let base = stack.pop().unwrap();
                let Value::Record(base_fields) = base else {
                    return Err(HelixError::new(
                        format!("`...` record update needs a record, got a {}", base.type_name()),
                        line,
                        col,
                    )
                    .hint("the spread base must be a record, e.g. `{ ...resp, status: 500 }`."));
                };
                // Clone the base, then set (override) or append each update field, in order.
                let mut out: Vec<(crate::symbol::Symbol, Value)> = (*base_fields).clone();
                for (name, val) in names.iter().copied().zip(vals) {
                    match out.iter_mut().find(|(s, _)| *s == name) {
                        Some(slot) => slot.1 = val,
                        None => out.push((name, val)),
                    }
                }
                stack.push(Value::Record(std::rc::Rc::new(out)));
            }
            Op::GetField(name) => {
                let recv = stack.pop().unwrap();
                stack.push(crate::interp::eval_field(&recv, *name, line, col)?);
            }
            Op::Slice(mask) => {
                // Bounds were pushed after the receiver in start/stop/step order;
                // pop the raw values back off in reverse, then resolve them in
                // forward order so error reporting matches the tree-walker exactly.
                let step_v = if mask & 4 != 0 { stack.pop() } else { None };
                let stop_v = if mask & 2 != 0 { stack.pop() } else { None };
                let start_v = if mask & 1 != 0 { stack.pop() } else { None };
                let recv = stack.pop().unwrap();
                let start = match &start_v {
                    Some(v) => crate::interp::slice_bound(v, line, col)?,
                    None => None,
                };
                let stop = match &stop_v {
                    Some(v) => crate::interp::slice_bound(v, line, col)?,
                    None => None,
                };
                let step = match &step_v {
                    Some(v) => crate::interp::slice_bound(v, line, col)?.unwrap_or(1),
                    None => 1,
                };
                if step == 0 {
                    return Err(HelixError::new("slice step cannot be zero", line, col));
                }
                stack.push(crate::interp::eval_slice(&recv, start, stop, step, line, col)?);
            }
            Op::Destructure(slots) => {
                let v = stack.pop().unwrap();
                let parts = crate::interp::destructure_parts(&v, slots.len(), line, col)?;
                for (slot, val) in slots.iter().zip(parts) {
                    globals[*slot as usize] = val;
                }
            }
            Op::DestructureBind(slots) => {
                // A comprehension multi-binder pattern: split the current element
                // into the named param locals (same helper the tree-walker uses).
                let v = stack.pop().unwrap();
                let parts = crate::interp::pattern_parts(&v, slots.len(), line, col)?;
                let base = frames[fi].base;
                for (slot, val) in slots.iter().zip(parts) {
                    locals[base + *slot as usize] = val;
                }
            }
            Op::Method(d) => {
                let (name, nargs) = (&d.name, &d.nargs);
                let split = stack.len() - *nargs as usize;
                let args: Vec<Value> = stack.split_off(split);
                let recv = stack.pop().unwrap();
                // `is_missing` is universal; DataFrame/GroupBy receivers bypass the
                // universal handler in `call_method`, so intercept it here (a
                // frame/group is never `missing` → `false`), matching the tree-walker.
                let result = if name.as_str() == "is_missing"
                    && matches!(recv, Value::DataFrame(_) | Value::GroupBy(_))
                {
                    if args.is_empty() {
                        Ok(Value::Bool(false))
                    } else {
                        Err(HelixError::new("`is_missing` takes no arguments", line, col))
                    }
                } else {
                    match &recv {
                        // Dispatch by receiver type, exactly as the tree-walker does.
                        Value::DataFrame(lf) => {
                            crate::interp::df_value_method(lf, name, args, line, col)
                        }
                        Value::GroupBy(_) => Err(HelixError::new(
                            format!("a GroupBy has no value-method `{}`", name),
                            line,
                            col,
                        )
                        .hint("aggregate with a column, e.g. `g.mean(col)`.")),
                        _ => crate::interp::call_method(&recv, name, args, line, col),
                    }
                }?;
                stack.push(result);
            }
            Op::DfColumnVerb(d) => {
                let (name, args, lbind) = (&d.name, &d.args, &d.locals);
                // A DataFrame column-verb (the type checker proved the receiver is a
                // DataFrame). `resolve_var` resolves a bare predicate name that
                // isn't a column to a local (via the captured slot map) or a global
                // — the same env the tree-walker uses.
                let recv = stack.pop().unwrap();
                let lf = match &recv {
                    Value::DataFrame(lf) => lf.clone(),
                    other => {
                        return Err(HelixError::new(
                            format!("expected a DataFrame, got {}", other.type_name()),
                            line,
                            col,
                        ))
                    }
                };
                let base = frames[fi].base;
                let resolve = |nm: &str| -> Option<Value> {
                    for (lname, slot) in lbind.iter().rev() {
                        if lname == nm {
                            return Some(locals[base + *slot as usize].clone());
                        }
                    }
                    program
                        .global_names
                        .iter()
                        .position(|g| g == nm)
                        .map(|i| globals[i].clone())
                };
                let result =
                    crate::interp::df_column_verb(&lf, name.as_str(), args.as_slice(), &resolve, line, col)?;
                stack.push(result);
            }
            Op::DfJoin { spec } => {
                // Stack order: left receiver pushed first, then the right operand.
                let right = stack.pop().unwrap();
                let left = stack.pop().unwrap();
                let result = match &left {
                    // DataFrame join: the right operand is another frame; keys are by-name.
                    Value::DataFrame(lf) => {
                        let rf = match &right {
                            Value::DataFrame(rf) => rf.clone(),
                            other => {
                                return Err(HelixError::new(
                                    format!(
                                        "`join` needs a DataFrame to join with, found {}",
                                        other.type_name()
                                    ),
                                    line,
                                    col,
                                ))
                            }
                        };
                        let (keys, how) = crate::interp::parse_join_spec(spec.as_slice(), line, col)?;
                        Value::dataframe(lf.join(&rf, &keys, &how, line, col)?)
                    }
                    // The receiver's static type was `Unknown` and turned out NOT to be a
                    // DataFrame — this is the value `xs.join(sep)` (an array of strings).
                    // Dispatch by runtime type exactly as the tree-walker does; `spec`
                    // (extra by-name keys) is empty for the array form.
                    _ => crate::interp::call_method(&left, "join", vec![right], line, col)?,
                };
                stack.push(result);
            }
            Op::GroupByAgg(d) => {
                let (name, args) = (&d.name, &d.args);
                let recv = stack.pop().unwrap();
                let (handle, keys) = match &recv {
                    Value::GroupBy(g) => (g.handle.clone(), g.keys.clone()),
                    other => {
                        return Err(HelixError::new(
                            format!("expected a GroupBy, got {}", other.type_name()),
                            line,
                            col,
                        ))
                    }
                };
                let result = crate::interp::groupby_agg(
                    &handle,
                    &keys,
                    name.as_str(),
                    args.as_slice(),
                    line,
                    col,
                )?;
                stack.push(result);
            }
            Op::CompInit(kind, missing_target) => {
                let v = stack.pop().unwrap();
                match v {
                    Value::Array(a) => {
                        iters.push(CompIter {
                            source: CompSource::Array { arr: a, idx: 0 },
                            cur_val: Value::Unit,
                            builder: crate::value::ColumnBuilder::default(),
                        });
                    }
                    // `missing.map(...)` etc. propagate (ADR-0001).
                    Value::Missing => frames[fi].ip = *missing_target as usize,
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "type {} has no method `{}`",
                                other.type_name(),
                                kind.method_name()
                            ),
                            line,
                            col,
                        )
                        .hint("`map`, `filter`, `where`, and `reduce` work on arrays."))
                    }
                }
            }
            Op::TryJitReduce { loop_idx, acc_slot, after } => {
                // Stack top: [.., start, end]; `acc_slot` already holds `init`.
                // Take the native loop only when start/end/init are all `Int` and
                // the range is within the materialization cap; anything else falls
                // through to the identical `CompInitRange` bytecode loop below — so
                // float accumulators, over-cap ranges (which must error exactly as
                // `CompInitRange` does), and no-JIT builds all take the same path
                // the tree-walker oracle matches.
                // A scalar accumulator has 1 body; a tuple accumulator N (the slot then
                // holds a `Tuple` of N `Int`s). A scalar body may carry `captures` — their
                // values sit above `[start, end]`; split them off (taken or not) so the
                // fall-through `CompInitRange` sees only `[start, end]`.
                let n_acc = program.reduce_loops[*loop_idx as usize].bodies.len();
                let n_caps = program.reduce_loops[*loop_idx as usize].captures.len();
                let cap_vals =
                    if n_caps > 0 { stack.split_off(stack.len() - n_caps) } else { Vec::new() };
                let base = frames[fi].base;
                let len = stack.len();
                let slot = base + *acc_slot as usize;
                let bounds = match (jit, &stack[len - 2], &stack[len - 1]) {
                    (Some(j), Value::Int(s), Value::Int(e))
                        if ((*e as i128) - (*s as i128)) <= 100_000_000 =>
                    {
                        j.reduce_loop(*loop_idx as usize).map(|ptr| (ptr, *s, *e))
                    }
                    _ => None,
                };
                let took_native = if let Some((ptr, s, e)) = bounds {
                    if n_acc == 1 && program.reduce_loops[*loop_idx as usize].float {
                        // A scalar f64 fold over the i64 counter (capture-free): a `Float`
                        // init confirms the f64 ABI; anything else falls back to the VM loop.
                        if let Value::Float(init) = locals[slot] {
                            let r = unsafe { crate::jit::call_reduce_f64(ptr, s, e, init) };
                            locals[slot] = Value::Float(r);
                            true
                        } else {
                            false
                        }
                    } else if n_acc == 1 {
                        if let Value::Int(init) = locals[slot] {
                            if n_caps == 0 {
                                let r = unsafe { crate::jit::call_reduce(ptr, s, e, init) };
                                locals[slot] = Value::Int(r);
                                true
                            } else {
                                // Every captured value must be an `Int` (else fall back to
                                // the bytecode loop, which reads the captures by name).
                                let caps: Option<Vec<i64>> = cap_vals
                                    .iter()
                                    .map(|v| if let Value::Int(i) = v { Some(*i) } else { None })
                                    .collect();
                                match caps {
                                    Some(c) => {
                                        let r = unsafe {
                                            crate::jit::call_reduce_caps(ptr, s, e, init, c.as_ptr())
                                        };
                                        locals[slot] = Value::Int(r);
                                        true
                                    }
                                    None => false,
                                }
                            }
                        } else {
                            false
                        }
                    } else {
                        // Marshal the N-slot tuple/record into a buffer, fold natively, rebuild
                        // the same-shaped accumulator. An f64 accumulator packs each slot's
                        // bit pattern (the kernel reads/writes them as f64 — same `*mut i64`
                        // runner); the i64 path takes Int slots. A wrong-typed slot falls back.
                        // (`tmpl` clone is an Rc bump, freeing the locals borrow.)
                        let tmpl = locals[slot].clone();
                        let is_float = program.reduce_loops[*loop_idx as usize].float;
                        let slots =
                            if is_float { acc_to_slots_f64(&tmpl, n_acc) } else { acc_to_slots(&tmpl, n_acc) };
                        match slots {
                            Some(mut buf) => {
                                unsafe { crate::jit::call_tuple_reduce(ptr, s, e, buf.as_mut_ptr()) };
                                locals[slot] =
                                    if is_float { rebuild_acc_f64(&tmpl, buf) } else { rebuild_acc(&tmpl, buf) };
                                true
                            }
                            None => false,
                        }
                    }
                } else {
                    false
                };
                if took_native {
                    stack.pop(); // end
                    stack.pop(); // start
                    frames[fi].ip = *after as usize;
                }
                // else: fall through (ip already advanced) to CompInitRange.
            }
            Op::TryJitMap { kernel_idx, after } => {
                // The captured values (if any) sit on top of the receiver array. Pop them
                // off (taken or not) so the bytecode fall-through sees only the array, then
                // dispatch by element type: an `Int` array → the `i64` kernel with `i64`
                // captures (a non-`Int` capture falls through); a `Float` array → the `f64`
                // kernel with `f64` captures (Int/Float coerce). Anything else falls
                // through to the identical `CompInit` bytecode loop (the oracle path).
                use crate::value::ArrayData;
                let kidx = *kernel_idx as usize;
                let n_caps = program.map_kernels[kidx].captures.len();
                let split = stack.len() - n_caps;
                let cap_vals = stack.split_off(split);
                enum Pick {
                    I64(*const u8, Vec<i64>),
                    F64(*const u8, Vec<f64>),
                    Mixed(*const u8),
                    No,
                }
                let pick = match (jit, stack.last()) {
                    (Some(j), Some(Value::Array(a))) => match &**a {
                        ArrayData::Ints(_) => {
                            if let Some(p) = j.map_kernel(kidx) {
                                // plain i64 kernel: every capture must be an `Int`
                                let caps: Option<Vec<i64>> = cap_vals
                                    .iter()
                                    .map(|v| if let Value::Int(i) = v { Some(*i) } else { None })
                                    .collect();
                                match caps {
                                    Some(c) => Pick::I64(p, c),
                                    None => Pick::No,
                                }
                            } else if cap_vals.is_empty() {
                                // no i64 kernel + capture-free → a mixed Int→Float body
                                // (`range.map(j => j*0.001)`) may have a mixed kernel.
                                match j.map_kernel_mixed(kidx) {
                                    Some(p) => Pick::Mixed(p),
                                    None => Pick::No,
                                }
                            } else {
                                Pick::No
                            }
                        }
                        ArrayData::Floats(_) => {
                            let caps: Option<Vec<f64>> = cap_vals.iter().map(|v| v.as_f64()).collect();
                            match (caps, j.map_kernel_f64(kidx)) {
                                (Some(c), Some(p)) => Pick::F64(p, c),
                                _ => Pick::No,
                            }
                        }
                        _ => Pick::No,
                    },
                    _ => Pick::No,
                };
                match pick {
                    Pick::I64(ptr, caps) => {
                        let arr = stack.pop().unwrap();
                        if let Value::Array(a) = &arr
                            && let ArrayData::Ints(v) = &**a
                        {
                            let out = unsafe { crate::jit::run_map_kernel(ptr, v, &caps) };
                            stack.push(Value::int_array(out));
                            frames[fi].ip = *after as usize;
                        } else {
                            stack.push(arr);
                        }
                    }
                    Pick::F64(ptr, caps) => {
                        let arr = stack.pop().unwrap();
                        if let Value::Array(a) = &arr
                            && let ArrayData::Floats(v) = &**a
                        {
                            let out = unsafe { crate::jit::run_map_kernel_f64(ptr, v, &caps) };
                            stack.push(Value::float_array(out));
                            frames[fi].ip = *after as usize;
                        } else {
                            stack.push(arr);
                        }
                    }
                    Pick::Mixed(ptr) => {
                        let arr = stack.pop().unwrap();
                        if let Value::Array(a) = &arr
                            && let ArrayData::Ints(v) = &**a
                        {
                            let out = unsafe { crate::jit::run_map_kernel_mixed(ptr, v) };
                            stack.push(Value::float_array(out));
                            frames[fi].ip = *after as usize;
                        } else {
                            stack.push(arr);
                        }
                    }
                    Pick::No => {} // fall through to the bytecode loop
                }
            }
            Op::TryJitFilter { kernel_idx, after } => {
                let ptr = match (jit, stack.last()) {
                    (Some(j), Some(Value::Array(a)))
                        if matches!(&**a, crate::value::ArrayData::Ints(_)) =>
                    {
                        j.filter_kernel(*kernel_idx as usize)
                    }
                    _ => None,
                };
                if let Some(ptr) = ptr {
                    let arr = stack.pop().unwrap();
                    if let Value::Array(a) = &arr
                        && let crate::value::ArrayData::Ints(v) = &**a
                    {
                        let out = unsafe { crate::jit::run_filter_kernel(ptr, v) };
                        stack.push(Value::int_array(out));
                        frames[fi].ip = *after as usize;
                    }
                }
            }
            Op::TryJitFused { kernel_idx, after } => {
                use crate::bytecode::FusionSink;
                // The pipeline's source operands sit on the stack (array, or [start,end];
                // plus init for a Reduce sink). Run the single native loop when they are
                // all `Int` (range within the 100M cap) and a kernel compiled; otherwise
                // consume the same operands and fall through to the per-stage chain.
                let kern = &program.fused_kernels[*kernel_idx as usize];
                let n = kern.n_operands();
                let len = stack.len();
                // Read a tuple-accumulator init (`want` `Int` slots) into a buffer.
                let result: Option<Value> = jit
                    .and_then(|j| j.fused_kernel(*kernel_idx as usize))
                    .and_then(|ptr| {
                        let ops = &stack[len - n..];
                        if kern.source_is_range {
                            // ops = [start, end] (+ init for Reduce). Cap-check the span.
                            let (Value::Int(s), Value::Int(e)) = (&ops[0], &ops[1]) else {
                                return None;
                            };
                            if (*e as i128 - *s as i128) > 100_000_000 {
                                return None;
                            }
                            match &kern.sink {
                                FusionSink::Reduce { bodies, float: false, .. } if bodies.len() == 1 => {
                                    match &ops[2] {
                                        Value::Int(init) => Some(Value::Int(unsafe {
                                            crate::jit::call_reduce(ptr, *s, *e, *init)
                                        })),
                                        _ => None,
                                    }
                                }
                                // tuple/record accumulator: ops[2] is its N-Int value.
                                FusionSink::Reduce { bodies, .. } => {
                                    acc_to_slots(&ops[2], bodies.len()).map(|mut buf| {
                                        unsafe {
                                            crate::jit::call_tuple_reduce(ptr, *s, *e, buf.as_mut_ptr())
                                        };
                                        rebuild_acc(&ops[2], buf)
                                    })
                                }
                                // count: the kernel ignores the third arg.
                                FusionSink::Count => Some(Value::Int(unsafe {
                                    crate::jit::call_reduce(ptr, *s, *e, 0)
                                })),
                                FusionSink::Collect => None,
                            }
                        } else if let Value::Array(a) = &ops[0]
                            && let crate::value::ArrayData::Ints(v) = &**a
                        {
                            match &kern.sink {
                                FusionSink::Collect => Some(Value::int_array(unsafe {
                                    crate::jit::run_fused_collect(ptr, v)
                                })),
                                FusionSink::Count => Some(Value::Int(unsafe {
                                    crate::jit::run_fused_count(ptr, v)
                                })),
                                FusionSink::Reduce { bodies, float: false, .. } if bodies.len() == 1 => {
                                    match &ops[1] {
                                        Value::Int(init) => Some(Value::Int(unsafe {
                                            crate::jit::run_fused_reduce(ptr, v, *init)
                                        })),
                                        _ => None,
                                    }
                                }
                                // tuple/record accumulator: ops[1] is its N-Int value.
                                FusionSink::Reduce { bodies, .. } => {
                                    acc_to_slots(&ops[1], bodies.len()).map(|mut buf| {
                                        unsafe {
                                            crate::jit::run_fused_tuple_reduce(ptr, v, buf.as_mut_ptr())
                                        };
                                        rebuild_acc(&ops[1], buf)
                                    })
                                }
                            }
                        } else if let Value::Array(a) = &ops[0]
                            && let crate::value::ArrayData::Floats(v) = &**a
                        {
                            // An `f64` reduce over a `Float` array: scalar (1 body, `Float`
                            // init → `run_fused_reduce_f64`) or tuple (N bodies, all-`Float`
                            // accumulator marshalled as bit patterns → `run_fused_tuple_reduce`,
                            // the same `*mut i64` runner the kernel reads/writes as f64).
                            match &kern.sink {
                                FusionSink::Reduce { bodies, float: true, .. } if bodies.len() == 1 => {
                                    match &ops[1] {
                                        Value::Float(init) => Some(Value::Float(unsafe {
                                            crate::jit::run_fused_reduce_f64(ptr, v, *init)
                                        })),
                                        _ => None,
                                    }
                                }
                                FusionSink::Reduce { bodies, float: true, .. } => {
                                    acc_to_slots_f64(&ops[1], bodies.len()).map(|mut buf| {
                                        unsafe {
                                            crate::jit::run_fused_tuple_reduce_f64(ptr, v, buf.as_mut_ptr())
                                        };
                                        rebuild_acc_f64(&ops[1], buf)
                                    })
                                }
                                _ => None,
                            }
                        } else {
                            None
                        }
                    });
                stack.truncate(len - n);
                if let Some(v) = result {
                    stack.push(v);
                    frames[fi].ip = *after as usize;
                }
            }
            Op::CompInitRange => {
                // Pop [start, end], validate as integers (matching `range`), and
                // iterate lazily — no array. Validate start first (arg order).
                let end_v = stack.pop().unwrap();
                let start_v = stack.pop().unwrap();
                let start = crate::interp::as_int(&start_v, "range", line, col)?;
                let end = crate::interp::as_int(&end_v, "range", line, col)?;
                let len = (end as i128) - (start as i128);
                if len > 100_000_000 {
                    return Err(HelixError::new(
                        format!("`range` would build {} elements, which is too large", len.max(0)),
                        line,
                        col,
                    )
                    .hint("ranges are materialized eagerly — keep them under 100 million elements."));
                }
                iters.push(CompIter {
                    source: CompSource::Range { cur: start, end },
                    cur_val: Value::Unit,
                    builder: crate::value::ColumnBuilder::default(),
                });
            }
            Op::CompNext(binder, end_target) => {
                let next = {
                    let it = iters.last_mut().unwrap();
                    match &mut it.source {
                        CompSource::Array { arr, idx } => {
                            if *idx < arr.len() {
                                let el = arr.get(*idx);
                                *idx += 1;
                                Some(el)
                            } else {
                                None
                            }
                        }
                        CompSource::Range { cur, end } => {
                            if *cur < *end {
                                let el = Value::Int(*cur);
                                *cur += 1;
                                Some(el)
                            } else {
                                None
                            }
                        }
                    }
                };
                match next {
                    Some(el) => {
                        iters.last_mut().unwrap().cur_val = el.clone();
                        let base = frames[fi].base;
                        locals[base + *binder as usize] = el;
                    }
                    None => frames[fi].ip = *end_target as usize,
                }
            }
            Op::CompMapPush => {
                let v = stack.pop().unwrap();
                iters.last_mut().unwrap().builder.push(v);
            }
            Op::CompFilterPush => {
                let keep = stack.pop().unwrap();
                let it = iters.last_mut().unwrap();
                match keep {
                    Value::Bool(true) => {
                        let el = it.cur_val.clone();
                        it.builder.push(el);
                    }
                    Value::Bool(false) => {}
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "`filter` expects a yes/no test, but the expression produced a {}",
                                other.type_name()
                            ),
                            line,
                            col,
                        )
                        .hint("write a comparison, e.g. `xs.filter(it > 50)`."))
                    }
                }
            }
            Op::CompEnd => {
                let it = iters.pop().unwrap();
                stack.push(it.builder.finish());
            }
            Op::CompEndDiscard => {
                iters.pop();
            }
            Op::CompBoolTest(is_all, sm_slot, short_target) => {
                let v = stack.pop().unwrap();
                match v {
                    Value::Bool(bv) => {
                        // `all` short-circuits on false; `any` on true.
                        if (*is_all && !bv) || (!*is_all && bv) {
                            frames[fi].ip = *short_target as usize;
                        }
                    }
                    Value::Missing => {
                        let base = frames[fi].base;
                        locals[base + *sm_slot as usize] = Value::Bool(true);
                    }
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "`{}` expects a yes/no test, but the expression produced a {}",
                                if *is_all { "all" } else { "any" },
                                other.type_name()
                            ),
                            line,
                            col,
                        )
                        .hint("write a comparison, e.g. `xs.any(it > 0)`."))
                    }
                }
            }
            Op::Raise(d) => {
                return Err(HelixError::new((*d.msg).clone(), line, col).hint((*d.hint).clone()));
            }
            Op::TryBegin(catch_ip) => {
                handlers.push(Handler {
                    stack_len: stack.len(),
                    frame_depth: frames.len(),
                    locals_len: locals.len(),
                    iters_len: iters.len(),
                    catch_ip: *catch_ip as usize,
                });
            }
            Op::TryOk(end_ip) => {
                // Body finished normally: drop the handler, wrap the value in the
                // ok-record, and jump past the catch.
                handlers.pop();
                let v = stack.pop().unwrap();
                stack.push(crate::interp::try_ok(v));
                frames[fi].ip = *end_ip as usize;
            }
            Op::TryErr => {
                // Reached via an unwind, which pushed the error message: wrap it in
                // the err-record. (The handler was already popped during the unwind.)
                let msg = match stack.pop().unwrap() {
                    Value::Str(s) => (*s).clone(),
                    other => other.to_string(),
                };
                stack.push(crate::interp::try_err(msg));
            }
            Op::Pop => {
                stack.pop();
            }
        }
        Ok(())
        })();
        if let Err(e) = step {
            match handlers.pop() {
                // Caught by the nearest active `try`: unwind every VM stack to the
                // depths recorded at `TryBegin`, resume at its catch handler with the
                // error message on the operand stack (consumed by `Op::TryErr`).
                Some(h) => {
                    stack.truncate(h.stack_len);
                    frames.truncate(h.frame_depth);
                    locals.truncate(h.locals_len);
                    iters.truncate(h.iters_len);
                    let tf = frames.len() - 1;
                    frames[tf].ip = h.catch_ip;
                    stack.push(Value::Str(std::rc::Rc::new(e.message)));
                }
                // No active `try`: propagate out of the VM as a real error.
                None => return Err(e),
            }
        }
    }

    Ok(stack)
}


#[cfg(test)]
mod tests;

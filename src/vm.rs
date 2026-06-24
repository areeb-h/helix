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
                Div if y != 0 => return Ok(Value::Float(x as f64 / y as f64)),
                _ => {} // Div/Mod by zero, Pow → full path
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

/// Key into the memo table: (function index, scalar arguments).
type MemoKey = (usize, Vec<MemoArg>);

/// What a comprehension iterates: a materialized array, or — for a fused
/// `range(...)` — a lazy integer counter (no array allocated at all).
enum CompSource {
    Array { arr: std::rc::Rc<Vec<Value>>, idx: usize },
    Range { cur: i64, end: i64 },
}

/// Active comprehension iterator state (a stack, so comprehensions nest).
/// `cur_val` is the element just yielded (used by `filter`); `builder` collects
/// results for `map`/`filter` and is ignored by `reduce`.
struct CompIter {
    source: CompSource,
    cur_val: Value,
    builder: Vec<Value>,
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

    let main = &program.funcs[0];
    locals.resize(main.n_locals as usize, Value::Unit);
    frames.push(Frame { func: 0, ip: 0, base: 0, memo_key: None });

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
        let (line, col) = chunk.pos[ip];
        frames[fi].ip = ip + 1; // default advance; control-flow ops overwrite it

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
                    let kargs: Vec<MemoArg> = stack[start..]
                        .iter()
                        .map(|v| match v {
                            Value::Int(n) => MemoArg::Int(*n),
                            Value::Float(f) => MemoArg::Float(f.to_bits()),
                            _ => MemoArg::Int(0), // unreachable: gated by all_scalar
                        })
                        .collect();
                    let key: MemoKey = (idx, kargs);
                    if let Some(cached) = memo.get(&key) {
                        let cached = cached.clone();
                        stack.truncate(start);
                        stack.push(cached);
                        continue;
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
                    frames.push(Frame { func: idx, ip: 0, base, memo_key: Some(key) });
                    continue;
                }

                // Native fast path: dispatch to the specialization matching the
                // argument types. All-Int + an i64 version → native i64 (Int
                // result). Otherwise all-numeric + an f64 version → native f64
                // (Float result; the float-only or mixed/float case). All of the
                // function's internal recursion then stays native.
                if let Some(nf) = jit_for_idx[idx] {
                    if nargs == nf.arity {
                        let tail = &stack[start..];
                        // The f64 specialization always returns Float, so it is
                        // only valid when EVERY argument is Float (then every op,
                        // and returning a param, yields Float — matching the
                        // interpreter). For MIXED Int/Float args the result type
                        // depends on what the function does (e.g. `f(a,b)=b` keeps
                        // an Int `b`), so those fall through to the VM, which
                        // handles type-mixing correctly.
                        let all_float = tail.iter().all(|v| matches!(v, Value::Float(_)));
                        if all_int && nf.i64_ptr.is_some() {
                            let iargs: Vec<i64> = tail
                                .iter()
                                .map(|v| if let Value::Int(n) = v { *n } else { 0 })
                                .collect();
                            stack.truncate(start);
                            let r = unsafe { crate::jit::call_i64(nf.i64_ptr.unwrap(), &iargs) };
                            stack.push(Value::Int(r));
                            continue;
                        }
                        if all_float && nf.f64_ptr.is_some() {
                            let fargs: Vec<f64> = tail.iter().map(|v| v.as_f64().unwrap()).collect();
                            stack.truncate(start);
                            let r = unsafe { crate::jit::call_f64(nf.f64_ptr.unwrap(), &fargs) };
                            stack.push(Value::Float(r));
                            continue;
                        }
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
                frames.push(Frame { func: idx, ip: 0, base, memo_key: None });
            }
            Op::MakeFunc { idx, arity } => {
                stack.push(Value::VmFunc { idx: *idx, arity: *arity });
            }
            Op::CallValue { nargs, name } => {
                let nargs = *nargs as usize;
                let start = stack.len() - nargs;
                // The function value sits just below the args (loaded first).
                let idx = match &stack[start - 1] {
                    Value::VmFunc { idx, .. } => *idx as usize,
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
                frames.push(Frame { func: idx, ip: 0, base, memo_key: None });
            }
            Op::Return => {
                let ret = stack.pop().unwrap();
                let frame = frames.pop().unwrap();
                locals.truncate(frame.base);
                // A memoization miss: record the result so future calls with the
                // same arguments return instantly (bounded for safety).
                if let Some(key) = frame.memo_key {
                    if memo.len() < MEMO_MAX_ENTRIES {
                        memo.insert(key, ret.clone());
                    }
                }
                stack.push(ret);
            }
            Op::MakeArray(n) => {
                let start = stack.len() - *n as usize;
                let items: Vec<Value> = stack.split_off(start);
                stack.push(Value::Array(std::rc::Rc::new(items)));
            }
            Op::Index => {
                let idx = stack.pop().unwrap();
                let recv = stack.pop().unwrap();
                stack.push(crate::interp::eval_index(&recv, &idx, line, col)?);
            }
            Op::Interp(parts) => {
                let holes = parts
                    .iter()
                    .filter(|p| matches!(p, crate::ast::InterpPart::Expr(_)))
                    .count();
                let vals: Vec<Value> = stack.split_off(stack.len() - holes);
                let mut s = String::new();
                let mut vi = 0;
                for part in parts.iter() {
                    match part {
                        crate::ast::InterpPart::Lit(t) => s.push_str(t),
                        crate::ast::InterpPart::Expr(_) => {
                            s.push_str(&vals[vi].to_string());
                            vi += 1;
                        }
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
                let fields: Vec<(String, Value)> =
                    names.iter().cloned().zip(vals).collect();
                stack.push(Value::Record(std::rc::Rc::new(fields)));
            }
            Op::GetField(name) => {
                let recv = stack.pop().unwrap();
                stack.push(crate::interp::eval_field(&recv, name, line, col)?);
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
                for (slot, val) in slots.iter().zip(parts.into_iter()) {
                    globals[*slot as usize] = val;
                }
            }
            Op::DestructureBind(slots) => {
                // A comprehension multi-binder pattern: split the current element
                // into the named param locals (same helper the tree-walker uses).
                let v = stack.pop().unwrap();
                let parts = crate::interp::pattern_parts(&v, slots.len(), line, col)?;
                let base = frames[fi].base;
                for (slot, val) in slots.iter().zip(parts.into_iter()) {
                    locals[base + *slot as usize] = val;
                }
            }
            Op::Method(name, nargs) => {
                let split = stack.len() - *nargs as usize;
                let args: Vec<Value> = stack.split_off(split);
                let recv = stack.pop().unwrap();
                let result = match &recv {
                    // Dispatch by receiver type, exactly as the tree-walker does.
                    Value::DataFrame(lf) => crate::interp::df_value_method(lf, name, args, line, col),
                    Value::GroupBy { .. } => Err(HelixError::new(
                        format!("a GroupBy has no value-method `{}`", name),
                        line,
                        col,
                    )
                    .hint("aggregate with a column, e.g. `g.mean(col)`.")),
                    _ => crate::interp::call_method(&recv, name, args, line, col),
                }?;
                stack.push(result);
            }
            Op::DfColumnVerb { name, args, locals: lbind } => {
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
            Op::GroupByAgg { name, args } => {
                let recv = stack.pop().unwrap();
                let (lf, keys) = match &recv {
                    Value::GroupBy { lf, keys } => (lf.clone(), keys.clone()),
                    other => {
                        return Err(HelixError::new(
                            format!("expected a GroupBy, got {}", other.type_name()),
                            line,
                            col,
                        ))
                    }
                };
                let result =
                    crate::interp::groupby_agg(&lf, &keys, name.as_str(), args.as_slice(), line, col)?;
                stack.push(result);
            }
            Op::CompInit(kind, missing_target) => {
                let v = stack.pop().unwrap();
                match v {
                    Value::Array(a) => {
                        iters.push(CompIter {
                            source: CompSource::Array { arr: a, idx: 0 },
                            cur_val: Value::Unit,
                            builder: Vec::new(),
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
                let base = frames[fi].base;
                let len = stack.len();
                let taken = match (
                    jit,
                    &stack[len - 2],
                    &stack[len - 1],
                    &locals[base + *acc_slot as usize],
                ) {
                    (Some(j), Value::Int(s), Value::Int(e), Value::Int(init)) => {
                        let span = (*e as i128) - (*s as i128);
                        if span <= 100_000_000 {
                            j.reduce_loop(*loop_idx as usize).map(|ptr| (ptr, *s, *e, *init))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some((ptr, s, e, init)) = taken {
                    stack.pop(); // end
                    stack.pop(); // start
                    let r = unsafe { crate::jit::call_reduce(ptr, s, e, init) };
                    locals[base + *acc_slot as usize] = Value::Int(r);
                    frames[fi].ip = *after as usize;
                }
                // else: fall through (ip already advanced) to CompInitRange.
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
                    builder: Vec::new(),
                });
            }
            Op::CompNext(binder, end_target) => {
                let next = {
                    let it = iters.last_mut().unwrap();
                    match &mut it.source {
                        CompSource::Array { arr, idx } => {
                            if *idx < arr.len() {
                                let el = arr[*idx].clone();
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
                stack.push(Value::Array(std::rc::Rc::new(it.builder)));
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
            Op::Raise(msg, hint) => {
                return Err(HelixError::new((**msg).clone(), line, col).hint((**hint).clone()));
            }
            Op::Pop => {
                stack.pop();
            }
        }
    }

    Ok(stack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bytecode, lexer, parser};

    /// Run a source string on the VM and return the value of its final
    /// expression (the trailing `Pop` is stripped so the value survives).
    fn vm_val(src: &str) -> Value {
        let toks = lexer::lex(src).unwrap();
        let ast = parser::parse(toks).unwrap();
        let mut prog = bytecode::compile_with_types(&ast, None).expect("expected this program to compile to bytecode");
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        exec(&prog, None).unwrap().pop().unwrap_or(Value::Unit)
    }

    /// The same source through the reference tree-walker.
    fn tw_val(src: &str) -> Value {
        let toks = lexer::lex(src).unwrap();
        let ast = parser::parse(toks).unwrap();
        let mut interp = Interp::new();
        let mut last = Value::Unit;
        for stmt in &ast {
            last = interp.exec(stmt).unwrap().value;
        }
        last
    }

    // ---------- differential fuzzing ----------
    //
    // Generate thousands of random programs and assert the bytecode VM and the
    // tree-walker produce the *same outcome* (same value, or both reject). This
    // automatically hunts the cross-engine divergence class the manual audit
    // found by hand (lossy comparison, overflow, etc.) — and guards against
    // regressions as the engines evolve.

    /// Deterministic PRNG (SplitMix64-style) so failures reproduce exactly.
    fn next(rng: &mut u64) -> u64 {
        *rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn pick(rng: &mut u64, n: u64) -> u64 {
        next(rng) % n
    }

    /// Random expression over the VM-compilable scalar core: int/float literals
    /// (including values near the i64 edge, to stress overflow + comparison),
    /// `+ - *`, comparisons, `if`, `let`, negation, and variable reads.
    fn gen_expr(rng: &mut u64, depth: u32, vars: &[String]) -> String {
        if depth == 0 || pick(rng, 3) == 0 {
            return match pick(rng, 4) {
                0 if !vars.is_empty() => vars[pick(rng, vars.len() as u64) as usize].clone(),
                1 => {
                    // occasionally a huge int to probe exact i64 comparison + wrap
                    let big = [0i64, 9_007_199_254_740_992, 9_007_199_254_740_993, i64::MAX, i64::MIN];
                    format!("{}", big[pick(rng, big.len() as u64) as usize])
                }
                2 => format!("{}.0", (next(rng) % 401) as i64 - 200),
                _ => format!("{}", (next(rng) % 4001) as i64 - 2000),
            };
        }
        match pick(rng, 15) {
            14 => {
                // Multi-binder comprehension (pattern binder `(p, q)`) over an array
                // → scalar/Bool. Exercises `DestructureBind` vs the tree-walker's
                // `eval_with_pattern`, including error paths (a scalar element can't
                // destructure into two params — both engines must reject identically).
                let pair = pick(rng, 2) == 0;
                let mk = |rng: &mut u64, vars: &[String]| -> String {
                    if pair {
                        format!("({}, {})", gen_expr(rng, 0, vars), gen_expr(rng, 0, vars))
                    } else {
                        gen_expr(rng, 0, vars) // non-pair → destructure error on both
                    }
                };
                let e0 = mk(rng, vars);
                let e1 = mk(rng, vars);
                match pick(rng, 3) {
                    0 => {
                        let op = ["+", "-", "*"][pick(rng, 3) as usize];
                        format!(
                            "(([{e0}, {e1}]).map((p, q) => p {op} q))[{}]",
                            gen_expr(rng, 0, vars)
                        )
                    }
                    1 => {
                        let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                        format!("([{e0}, {e1}]).all((p, q) => p {cop} q)")
                    }
                    _ => {
                        let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                        format!("([{e0}, {e1}]).any((p, q) => p {cop} q)")
                    }
                }
            }
            13 => {
                // any/all over a small array → Bool (exercises short-circuit loop)
                let m = if pick(rng, 2) == 0 { "any" } else { "all" };
                let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                format!(
                    "([{}, {}, {}]).{}(it {} {})",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    m,
                    cop,
                    gen_expr(rng, 0, vars)
                )
            }
            10 if pick(rng, 2) == 0 => {
                // fused range reduce → scalar (range bound may be huge → cap error,
                // negative → empty, or moderate → loop; all agree with the array path)
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                format!(
                    "(range({} % 5000)).reduce({}, (acc, x) => acc {} x)",
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars),
                    op
                )
            }
            11 => {
                // reduce over a small array → scalar (exercises the reduce loop)
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                format!(
                    "([{}, {}, {}, {}]).reduce({}, (acc, x) => acc {} x)",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, 0, vars),
                    op
                )
            }
            12 => {
                // map then index → scalar (exercises the map loop + binder)
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                format!(
                    "(([{}, {}, {}]).map(it {} {}))[{}]",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    op,
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars)
                )
            }
            8 => {
                // tuple literal indexed → a scalar element (exercises MakeTuple)
                format!(
                    "(({}, {}, {}))[{}]",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars)
                )
            }
            9 => {
                // record literal + field access → scalar (exercises MakeRecord/GetField)
                let field = ["a", "b", "c"][pick(rng, 3) as usize];
                format!(
                    "({{a: {}, b: {}, c: {}}}).{}",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    field
                )
            }
            10 => {
                // array sliced then indexed → scalar (exercises Slice)
                format!(
                    "(([{}, {}, {}, {}])[{}:{}])[{}]",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars)
                )
            }
            6 => {
                // array literal + index (exercises MakeArray / Index): in-bounds,
                // out-of-bounds, and non-int indices all resolve identically (a
                // value, or both engines reject).
                let n = 1 + pick(rng, 3);
                let elems: Vec<String> =
                    (0..n).map(|_| gen_expr(rng, depth - 1, vars)).collect();
                format!("([{}])[{}]", elems.join(", "), gen_expr(rng, depth - 1, vars))
            }
            7 => {
                // interpolation compared for equality (exercises Interp) — always
                // a Bool, so it composes back into the scalar grammar. Embed
                // leaves to keep the interpolated string un-nested.
                format!(
                    "(\"x{{{}}}\" == \"x{{{}}}\")",
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars)
                )
            }
            0 => {
                // includes % and / (zero divisors error identically on both engines)
                let op = ["+", "-", "*", "%", "/"][pick(rng, 5) as usize];
                format!("({} {} {})", gen_expr(rng, depth - 1, vars), op, gen_expr(rng, depth - 1, vars))
            }
            1 => format!("(-{})", gen_expr(rng, depth - 1, vars)),
            2 => {
                let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                format!("({} {} {})", gen_expr(rng, depth - 1, vars), cop, gen_expr(rng, depth - 1, vars))
            }
            3 => {
                let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                format!(
                    "if ({} {} {}) then ({}) else ({})",
                    gen_expr(rng, depth - 1, vars),
                    cop,
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                )
            }
            4 => {
                let name = format!("v{}", vars.len());
                let val = gen_expr(rng, depth - 1, vars);
                let mut vars2 = vars.to_vec();
                vars2.push(name.clone());
                format!("(let {} = ({}) in ({}))", name, val, gen_expr(rng, depth - 1, &vars2))
            }
            _ => format!("(-(-{}))", gen_expr(rng, depth - 1, vars)),
        }
    }

    fn run_vm(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let ast = parser::parse(toks).map_err(|_| ())?;
        let mut prog = bytecode::compile_with_types(&ast, None).map_err(|_| ())?;
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        match exec(&prog, None) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(_) => Err(()),
        }
    }

    /// A random scalar literal (incl. i64 extremes, to probe JIT vs interpreter
    /// overflow/comparison on the boundary).
    fn gen_lit(rng: &mut u64) -> String {
        match pick(rng, 3) {
            0 => format!("{}", (next(rng) % 4001) as i64 - 2000),
            1 => format!("{}.0", (next(rng) % 401) as i64 - 200),
            _ => format!("{}", [0i64, i64::MAX, i64::MIN][pick(rng, 3) as usize]),
        }
    }

    /// Like `run_vm`, but *with* the JIT enabled — so eligible functions execute
    /// as native code and are diffed against the tree-walker.
    fn run_vm_jit(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let ast = parser::parse(toks).map_err(|_| ())?;
        let mut prog = bytecode::compile_with_types(&ast, None).map_err(|_| ())?;
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        let jit = crate::jit::build(&ast, &prog.reduce_loops);
        match exec(&prog, jit.as_ref()) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(_) => Err(()),
        }
    }

    fn run_tw(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let ast = parser::parse(toks).map_err(|_| ())?;
        let mut interp = Interp::new();
        let mut last = Value::Unit;
        for stmt in &ast {
            match interp.exec(stmt) {
                Ok(o) => last = o.value,
                Err(_) => return Err(()),
            }
        }
        Ok(format!("{}", last))
    }

    /// Full pipeline (type-check → *type-directed* compile → VM), so
    /// receiver-polymorphic methods (DataFrame/Tensor column-verbs) route by the
    /// receiver's inferred type rather than falling back to the tree-walker.
    fn run_vm_typed(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let ast = parser::parse(toks).map_err(|_| ())?;
        let types = crate::types::check(&ast).map_err(|_| ())?;
        let mut prog = bytecode::compile_with_types(&ast, Some(types)).map_err(|_| ())?;
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        match exec(&prog, None) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(_) => Err(()),
        }
    }

    /// Type-directed routing: DataFrame column-verbs (`where`/`select`/`sort`/
    /// `group`) compile and run on the VM (not the tree-walker), matching the
    /// oracle. Locks in Phase 4 of the one-engine collapse.
    #[test]
    fn dataframe_column_verbs_run_on_vm() {
        let csv = "read_csv(\"examples/data/patients.csv\")";
        let cases = [
            format!("{csv}.where(age > 40).count()"),
            format!("{csv}.where(age > 40 and resting_hr < 75).count()"),
            format!("{csv}.where(age > 40).select(name, age).sort(age).count()"),
            // predicate referencing a global variable → the resolve_var path
            format!("t = 40\n{csv}.where(age > t).count()"),
            // grouped aggregation over an unevaluated column
            format!("read_csv(\"examples/data/genes.csv\").group(species).mean(expression).count()"),
        ];
        for src in &cases {
            assert_eq!(run_vm_typed(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
        // Concrete expected values (mirrors the interpreter's own test).
        assert_eq!(run_vm_typed(&format!("{csv}.where(age > 40).count()")), Ok("5".into()));
    }

    /// Reassignment/mutability now run on the VM (not via tree-walker fallback):
    /// an immutable reassignment raises the canonical error, `mut` re-declares, and
    /// a mutable reassignment updates — all matching the tree-walker.
    #[test]
    fn reassignment_matches_tree_walker_on_vm() {
        let cases = [
            "x = 1\nx = 2\nx",          // immutable reassignment → both error
            "mut x = 1\nx = 2\nx",      // mutable reassignment → 2
            "x = 1\nmut x = 2\nx",      // `mut` re-declares an immutable → 2
            "mut x = 1\nmut x = 2\nx",  // re-declare a mutable → 2
        ];
        for src in cases {
            assert_eq!(run_vm(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
    }

    /// One-engine gate: every shipped example must type-check and **compile to
    /// bytecode** — i.e. run on the VM, never fall back to the tree-walker. If a
    /// change reintroduces a fallback for an example, this fails loudly.
    #[test]
    fn examples_compile_on_the_vm() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("examples/ dir") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("helix") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let toks = lexer::lex(&src).unwrap_or_else(|_| panic!("lex failed: {path:?}"));
            let ast = parser::parse(toks).unwrap_or_else(|_| panic!("parse failed: {path:?}"));
            let types =
                crate::types::check(&ast).unwrap_or_else(|_| panic!("type-check failed: {path:?}"));
            bytecode::compile_with_types(&ast, Some(types)).unwrap_or_else(|_| {
                panic!("`{path:?}` falls back to the tree-walker — it should compile on the VM")
            });
            checked += 1;
        }
        assert!(checked >= 10, "expected the full example suite, only saw {checked}");
    }

    #[test]
    fn differential_vm_vs_tree_walker() {
        let mut rng = 0x1234_5678_9ABC_DEF0u64;
        for _ in 0..40_000 {
            let src = gen_expr(&mut rng, 5, &[]);
            match (run_vm(&src), run_tw(&src)) {
                // both succeed → values must be identical
                (Ok(a), Ok(b)) => assert_eq!(a, b, "VALUE divergence on `{src}`"),
                // both reject → fine (we don't require identical messages)
                (Err(()), Err(())) => {}
                // one accepts, the other rejects → a real divergence
                (v, t) => panic!("OUTCOME divergence on `{src}`: vm={v:?} tw={t:?}"),
            }
        }
    }

    #[test]
    fn differential_functions_with_jit() {
        let mut rng = 0xFEED_FACE_DEAD_BEEFu64;
        let params = vec!["a".to_string(), "b".to_string()];
        for _ in 0..10_000 {
            // a non-recursive function over (a, b), called with random scalars —
            // exercises CallFn + the i64/f64 JIT specializations against the
            // tree-walker. (Non-recursive ⇒ no native-stack risk.)
            let body = gen_expr(&mut rng, 4, &params);
            let src = format!(
                "fn f(a, b) = {}\nf({}, {})",
                body,
                gen_lit(&mut rng),
                gen_lit(&mut rng)
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "JIT/VM ≠ tree-walker on `{src}`"),
                (Err(()), Err(())) => {}
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// The native reduce loop (`TryJitReduce`) must equal the tree-walker. Drives
    /// `range(s, e).reduce(init, (acc, x) => body)` through the JIT-enabled runner
    /// with random `i64`-eligible bodies over `{acc, x}`, random (incl. negative
    /// and empty) ranges, and `Int`/`Float`/extreme inits — so the native path,
    /// the cap/float fall-throughs, and overflow wrapping are all diffed.
    #[test]
    fn differential_reduce_loops_with_jit() {
        let mut rng = 0x0DDC0FFEE_BADF00Du64;
        let binders = vec!["acc".to_string(), "x".to_string()];
        for _ in 0..10_000 {
            // A body over {acc, x}: when it stays in {ints, + - *, comparisons in
            // `if`, let} it is JIT-eligible (native path); otherwise (floats, /, %)
            // the guard falls back to the bytecode loop — both are diffed here.
            let body = gen_expr(&mut rng, 3, &binders);
            // Range bounds kept modest so the loop is cheap; `% 600 - 200` spans
            // negative (empty), zero, and positive lengths.
            let start = (next(&mut rng) % 600) as i64 - 200;
            let end = (next(&mut rng) % 600) as i64 - 200;
            let init = gen_lit(&mut rng);
            let src = format!(
                "(range({}, {})).reduce({}, (acc, x) => ({}))",
                start, end, init, body
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "reduce JIT ≠ tree-walker on `{src}`"),
                (Err(()), Err(())) => {}
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// First-class function values (`MakeFunc`/`CallValue`): lambdas and
    /// function-name aliases stored in variables and called, higher-order calls,
    /// free-vars-as-globals, function rendering, and the error paths. The VM must
    /// match the tree-walker on every shape. (These run the raw engines without the
    /// type checker, which in production additionally gates higher-order/capture.)
    #[test]
    fn first_class_functions_match_tree_walker() {
        let cases: &[&str] = &[
            "add = (a, b) => a + b\nadd(2, 3)",            // lambda → global, called
            "k = 10\nf = p => p + k\nf(5)",                // free var resolves to global
            "fn dbl(x) = x * 2\nh = dbl\nh(7)",            // bare fn name aliased + called
            "twice = n => n * 2\ntwice(twice(3))",         // nested application
            "fn apply(f, x) = f(x)\napply(p => p * 2, 5)", // higher-order (lambda arg)
            "fn dbl(x) = x * 2\nfn apply(f, x) = f(x)\napply(dbl, 5)", // higher-order (named)
            "add = (a, b) => a + b\n\"f={add}\"",          // function rendered in a string
            "fn dbl(x) = x * 2\nh = dbl\nh(1, 2)",         // error: wrong arity (both reject)
            "x = 5\nx(3)",                                  // error: calling a non-function
        ];
        for src in cases {
            match (run_vm(src), run_tw(src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "VM ≠ tree-walker on `{src}`"),
                (Err(()), Err(())) => {}
                (v, t) => panic!("divergence on `{src}`: vm={v:?} tw={t:?}"),
            }
        }
    }

    /// Robustness: random source over Helix-meaningful characters must never
    /// *panic* the lexer/parser/checker — only ever produce a value or a clean
    /// error. (Catches missing depth guards, index panics, etc.)
    #[test]
    fn parser_never_panics_on_random_input() {
        const CHARS: &[u8] = b"0123456789+-*/%()[]{}.,:<>=! \"abcxy_\nif then else let in fn mut and or not";
        let mut rng = 0xCAFE_F00D_1234_5678u64;
        for _ in 0..20_000 {
            let len = (next(&mut rng) % 60) as usize;
            let s: String = (0..len)
                .map(|_| CHARS[(next(&mut rng) % CHARS.len() as u64) as usize] as char)
                .collect();
            // Must return Ok or Err — never unwind. A panic fails the test.
            if let Ok(toks) = lexer::lex(&s) {
                if let Ok(ast) = parser::parse(toks) {
                    let _ = crate::types::check(&ast);
                }
            }
        }
    }

    /// The VM must be observationally identical to the tree-walker.
    fn assert_parity(src: &str) {
        assert_eq!(
            format!("{}", vm_val(src)),
            format!("{}", tw_val(src)),
            "VM and tree-walker disagree on: {src}"
        );
    }

    #[test]
    fn parity_scalar_and_control_flow() {
        for src in [
            "1 + 2 * 3 - 4",
            "2 ** 10",
            "7 % 3",
            "10 / 4",
            "-5 + 3",
            "not true",
            "1 < 2",
            "3 >= 3",
            "2 == 2",
            "2 != 3",
            "if 3 > 2 then 10 else 20",
            "if false then 1 else if true then 2 else 3",
            "true and false",
            "true or false",
            "false and missing",  // short-circuit: determined false
            "true or missing",    // short-circuit: determined true
            "missing and true",   // three-valued
            "missing ?? 42",
            "5 ?? 42",
            "let x = 10, y = x + 5 in x * y",
            "let a = 1 in let b = 2 in a + b",
            "[1, 2, 3][1]",
            "[10, 20, 30, 40][-1]",
            "[1 + 1, 2 * 3, 4][2]",
            "let xs = [5, 6, 7] in xs[0] + xs[2]",
            "let n = 42 in \"answer is {n}\"",
            "let x = 3, y = 4 in \"{x} + {y} = {x + y}\"",
            "3.5 + 1.5",
            "sqrt(144.0)",
            "abs(-7)",
            "max(3, 9)",
            "min(3, 9)",
            "pi > 3.0",
        ] {
            assert_parity(src);
        }
    }

    #[test]
    fn parity_comprehensions() {
        for src in [
            "[1, 2, 3, 4].map(it * 2)",
            "[1, 2, 3, 4, 5].filter(it > 2)",
            "[1, 2, 3, 4, 5, 6].where(it % 2 == 0)",
            "[1, 2, 3, 4].reduce(0, (acc, x) => acc + x)",
            "[1, 2, 3, 4].reduce(1, (acc, x) => acc * x)",
            "range(10).map(it * it)",
            "range(20).filter(it % 3 == 0).reduce(0, (acc, x) => acc + x)",
            // fused range reductions (no array materialized)
            "range(100).reduce(0, (acc, x) => acc + x)",
            "range(5, 15).reduce(1, (acc, x) => acc + x)",
            "let n = 50 in range(n).reduce(0, (acc, x) => acc + x)",
            "range(3, 3).reduce(99, (acc, x) => acc + x)",
            // named binder
            "[5, 10, 15].map(x => x + 1)",
            // nested comprehensions
            "[[1, 2], [3, 4]].map(row => row.reduce(0, (a, b) => a + b))",
            // body uses an outer variable
            "let k = 100 in [1, 2, 3].map(it + k)",
            // missing propagation
            "missing.map(it + 1)",
            // chained
            "range(8).map(it * 2).filter(it > 5).reduce(0, (a, x) => a + x)",
            // any / all, including short-circuit + missing three-valued logic
            "[1, 2, 3, 4].any(it > 3)",
            "[1, 2, 3, 4].all(it > 0)",
            "[1, 2, 3, 4].all(it > 2)",
            "[1, 2, 3].any(it > 10)",
            "range(100).any(it == 42)",
            "[1, missing, 3].any(it > 5)",
            "[1, missing, 3].all(it > 0)",
            "[1, missing, 3].any(it > 0)",
            "missing.all(it > 0)",
        ] {
            assert_parity(src);
        }
    }

    #[test]
    fn parity_value_methods_and_destructuring() {
        for src in [
            // array value-methods
            "[1, 2, 3, 4].sum()",
            "[10, 20, 30].mean()",
            "[3, 1, 2].sort()",
            "[1, 2, 3].count()",
            "[5, 1, 9, 2].max()",
            "[1, 2, 3, 4, 5].take(2)",
            "[1, 2, 3].reverse()",
            // string + dna value-methods
            "\"helix\".upper()",
            "dna(\"ATGCGC\").gc_content()",
            "dna(\"ATGATG\").find(\"GAT\")",
            "dna(\"ATGC\").kmers(2)",
            // destructuring + field/index, all on the VM
            "a, b = (3, 4)\na * b",
            "p, q, r = [1, 2, 3]\np + q + r",
            "{x: 7, y: 8}.x + {x: 7, y: 8}.y",
        ] {
            assert_parity(src);
        }
    }

    #[test]
    fn parity_functions_and_recursion() {
        assert_parity("fn sq(x) = x * x\nsq(9)");
        assert_parity("fn fib(n) = if n < 2 then n else fib(n - 1) + fib(n - 2)\nfib(15)");
        assert_parity("fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\nfact(10)");
        assert_parity("fn add(a, b) = a + b\nadd(40, 2)");
        assert_parity("mut acc = 0\nacc = acc + 100\nacc * 2");
    }

    /// The VM keeps its call stack on the heap, so recursion far deeper than the
    /// tree-walker's native-stack limit runs fine — on an ordinary test thread,
    /// with no 2 GiB stack needed. (`sum(1..100000) = 5000050000`.)
    #[test]
    fn deep_recursion_is_iterative() {
        let src = "fn sum(n, acc) = if n <= 0 then acc else sum(n - 1, acc + n)\nsum(100000, 0)";
        assert_eq!(format!("{}", vm_val(src)), "5000050000");
    }

    /// Automatic memoization turns overlapping recursion linear. `fib(40)` is
    /// ~165M calls naively — this only returns instantly (and correctly) if the
    /// pure, two-self-call `fib` is being memoized. Also confirms the result is
    /// unchanged (memoization is observably transparent).
    #[test]
    fn memoization_makes_overlapping_recursion_linear() {
        let v = vm_val("fn fib(n) = if n < 2 then n else fib(n - 1) + fib(n - 2)\nfib(40)");
        assert_eq!(format!("{}", v), "102334155");
    }

    /// Float recursion is memoized too (keyed by bit pattern). `fibf(40.0)` is
    /// ~165M calls naively — instant only if the float-keyed memo works.
    #[test]
    fn float_recursion_is_memoized() {
        let v = vm_val(
            "fn fibf(n) = if n < 2.0 then n else fibf(n - 1.0) + fibf(n - 2.0)\nfibf(40.0)",
        );
        assert_eq!(format!("{}", v), "102334155.0");
    }

    /// A function that reads a mutable global *through a callee* must NOT be
    /// memoized — its result isn't a function of its arguments alone. Here `f`
    /// reaches `mut g` via `leaf()`; with `g` changed between calls, the second
    /// `f(20)` must reflect the new `g` (= fib(21)·g = 10946·100), not a stale
    /// cached value.
    #[test]
    fn memoization_respects_transitive_mutable_reads() {
        let src = "mut g = 0\n\
                   fn leaf() = g\n\
                   fn f(n) = if n < 2 then leaf() else f(n - 1) + f(n - 2)\n\
                   g = 1\nx = f(20)\ng = 100\nf(20)";
        assert_eq!(format!("{}", vm_val(src)), "1094600");
    }

    /// Division must never be JIT-compiled: native `fdiv` returns inf on /0,
    /// but the interpreter errors — so a `/`-using function falls back and
    /// division by zero still raises (rather than silently producing inf).
    #[test]
    fn division_by_zero_is_not_jitted_to_inf() {
        let toks = lexer::lex("fn f(x) = 10.0 / x\nf(0.0)").unwrap();
        let ast = parser::parse(toks).unwrap();
        let prog = bytecode::compile_with_types(&ast, None).unwrap();
        let jit = crate::jit::build(&ast, &prog.reduce_loops);
        let err = exec(&prog, jit.as_ref()).unwrap_err();
        assert!(err.message.contains("division by zero"), "got: {}", err.message);
    }

    /// Runtime errors must still surface (and match the tree-walker's wording).
    #[test]
    fn errors_propagate() {
        let toks = lexer::lex("fn boom(n) = boom(n + 1)\nboom(0)").unwrap();
        let ast = parser::parse(toks).unwrap();
        let prog = bytecode::compile_with_types(&ast, None).unwrap();
        let err = run(&prog, None).unwrap_err();
        assert!(err.message.contains("maximum recursion depth"));
    }

    /// The JIT must produce identical results to the bytecode VM for the integer
    /// functions it compiles. Run each program both ways and compare.
    fn jit_val(src: &str) -> Value {
        let toks = lexer::lex(src).unwrap();
        let ast = parser::parse(toks).unwrap();
        let mut prog = bytecode::compile_with_types(&ast, None).expect("expected this program to compile");
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        let jit = crate::jit::build(&ast, &prog.reduce_loops);
        exec(&prog, jit.as_ref()).unwrap().pop().unwrap_or(Value::Unit)
    }

    #[test]
    fn jit_matches_vm() {
        for src in [
            "fn fib(n) = if n < 2 then n else fib(n - 1) + fib(n - 2)\nfib(20)",
            "fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\nfact(12)",
            "fn sum(n, acc) = if n <= 0 then acc else sum(n - 1, acc + n)\nsum(1000, 0)",
            "fn ack(m, n) = if m == 0 then n + 1 else if n == 0 then ack(m - 1, 1) else ack(m - 1, ack(m, n - 1))\nack(2, 3)",
            "fn sq(x) = let y = x * x in y + 1\nsq(7)",
            // Float specialization (f64 native code).
            "fn scale(x) = x * 2.5\nscale(4.0)",
            "fn sq(x) = x * x\nsq(3.0)", // same fn, picked as f64 for a Float arg
            "fn norm(a, b) = a / (a + b)\nnorm(1.0, 3.0)",
            "fn pow2(n, acc) = if n <= 0.0 then acc else pow2(n - 1.0, acc * 2.0)\npow2(10.0, 1.0)",
            // NB: forward-referenced mutual recursion (even/odd) is not covered —
            // the single-pass bytecode compiler can't resolve it yet (follow-up).
        ] {
            assert_eq!(format!("{}", jit_val(src)), format!("{}", vm_val(src)), "JIT≠VM on: {src}");
        }
    }
}

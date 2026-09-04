//! `.hbc` (Helix Bytecode Container) emitter — lowers a compiled [`Program`] to the
//! byte-exact on-disk format consumed by ctype's `hvm` interpreter (ADR 0023).
//!
//! ctype (the capability OS this language is being grown into an execution
//! substrate for) embeds a `no_std`, zero-allocation VM called `hvm` and runs
//! `.hbc` blobs directly in ring 0. This module is the *producer* side of that
//! seam: it maps the dependency-free CORE subset of Helix bytecode — `+ - *` and
//! comparisons over Int/Float/Bool, frame locals, direct and tail calls, and
//! `if` / `while` control flow — onto hvm's opcodes, and rejects anything outside
//! that subset with a precise, source-attributed error. (`/` is deliberately
//! excluded: Helix `/` float-promotes, hvm's DIV does not — see [`binop_sel`].)
//!
//! Two representations have to be reconciled:
//!
//!   * **Jump targets.** Helix `Jump`/`JumpIfFalse` name an *instruction index*;
//!     hvm names a *byte offset* within the function's code. A two-pass lowering
//!     computes the byte offset of every instruction, then patches each jump.
//!   * **Constant pools.** Helix keeps a *per-chunk* pool of arbitrary `Value`s;
//!     hvm keeps *one program-global* pool of only `Int`/`Float`/`Bool`. The
//!     emitter interns (with dedup) each referenced scalar into the shared pool and
//!     remaps indices, erroring on any non-scalar constant.
//!
//! The output parses byte-for-byte under `hvm::Program::parse` and executes
//! identically on the host VM and in the kernel. The format is specified
//! authoritatively in Helix ADR 0023 and mirrored by ctype ADR 0010; the reference
//! decoder/encoder is `hvm/src/lib.rs` (`Program::parse` / `ProgramBuilder::serialize`)
//! in the ctype repo.

use crate::ast::BinOp;
use crate::bytecode::{Chunk, Op, Program};
use crate::value::Value;

/// hvm `.hbc` v0 container magic + version (must match `hvm/src/lib.rs`).
const MAGIC: [u8; 4] = *b"HBC0";
const VERSION: u32 = 0;

/// hvm opcode bytes (`hvm/src/lib.rs` `mod op`).
mod op {
    pub const CONST: u8 = 0x01;
    pub const LOAD_LOCAL: u8 = 0x02;
    pub const STORE_LOCAL: u8 = 0x03;
    pub const BINARY: u8 = 0x04;
    pub const JUMP: u8 = 0x05;
    pub const JUMP_IF_FALSE: u8 = 0x06;
    pub const CALL_FN: u8 = 0x07;
    pub const TAIL_CALL_FN: u8 = 0x08;
    pub const RETURN: u8 = 0x09;
    pub const POP: u8 = 0x0A;
    /// `CALL_HOST host_idx:u32 nargs:u32` — invoke a host function (a kernel
    /// capability). The emitter produces it only for the `print` builtin.
    pub const CALL_HOST: u8 = 0x0B;
}

/// Host-function ABI: the stable indices the emitter's `CALL_HOST` ops use, which the
/// runtime's host handler dispatches to capabilities. Must match ctype's kernel host
/// handler (`kernel-rs/src/helixvm.rs`). Grows as more builtins map to capabilities.
mod host {
    /// Write the integer value + newline to the console (gated on `CAP_PRINT`). Returns
    /// the value. Helix's `print`/`emit`/`elog` all lower to this — on a single serial
    /// console the rich-vs-streaming distinction collapses to "write a line".
    pub const PRINT: u32 = 0;
    /// Pause for the argument's worth of time (gated on `CAP_SLEEP`) — Helix `sleep`.
    /// Returns the argument. Enables paced / live-updating programs.
    pub const SLEEP: u32 = 1;
    /// Read one integer from the console (gated on `CAP_GETKEY`) — Helix `read_int`.
    /// Takes no argument; returns the integer read. Enables interactive programs.
    pub const READ_INT: u32 = 2;

    /// The host index a Helix builtin maps to, or `None` if it is not a host call the
    /// `.hbc` runtime supports (then the emitter rejects it).
    pub fn for_builtin(name: &str) -> Option<u32> {
        match name {
            "print" | "emit" | "elog" => Some(PRINT),
            "sleep" => Some(SLEEP),
            "read_int" => Some(READ_INT),
            _ => None,
        }
    }
}

/// hvm binary sub-op selector bytes (`hvm/src/lib.rs` `mod binop`).
mod binop {
    pub const ADD: u8 = 0;
    pub const SUB: u8 = 1;
    pub const MUL: u8 = 2;
    // selector 3 = hvm `DIV` — reserved but NOT emitted: Helix `/` float-promotes and
    // hvm's DIV does not (see `binop_sel`'s `Div` rejection), so the emitter never
    // produces it. Re-add when hvm gains Int→Float promotion.
    pub const LT: u8 = 4;
    pub const LE: u8 = 5;
    pub const GT: u8 = 6;
    pub const GE: u8 = 7;
    pub const EQ: u8 = 8;
    pub const NE: u8 = 9;
}

/// hvm constant-pool tag bytes.
const TAG_INT: u8 = 1;
const TAG_FLOAT: u8 = 2;
const TAG_BOOL: u8 = 3;

/// Why a program (or one function) could not be lowered to the hvm core `.hbc`.
/// Carries a fully-rendered, source-attributed message.
#[derive(Debug)]
pub struct EmitError {
    pub message: String,
}

impl EmitError {
    fn new(msg: impl Into<String>) -> Self {
        EmitError { message: msg.into() }
    }
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// One emitted function's metadata, for the caller's index table. Because hvm
/// addresses functions by index (there is no name table in `.hbc`), this is how the
/// caller learns which index to run.
#[derive(Debug)]
pub struct EmittedFn {
    pub name: String,
    pub index: u32,
    pub nargs: u32,
    pub nlocals: u32,
}

/// The result of a successful emit: the `.hbc` bytes and the emitted-function table.
#[derive(Debug)]
pub struct Emitted {
    pub bytes: Vec<u8>,
    pub funcs: Vec<EmittedFn>,
    pub nconsts: usize,
}

/// One of the three constant types hvm supports.
#[derive(Clone, Copy)]
enum HConst {
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// Program-global hvm constant pool with dedup (mirrors `ProgramBuilder::konst`,
/// so the emitted pool matches the reference emitter's byte-for-byte).
#[derive(Default)]
struct ConstPool {
    consts: Vec<HConst>,
}

impl ConstPool {
    fn intern(&mut self, v: &Value) -> Result<u32, EmitError> {
        let hc = match v {
            Value::Int(i) => HConst::Int(*i),
            Value::Float(f) => HConst::Float(*f),
            Value::Bool(b) => HConst::Bool(*b),
            other => {
                return Err(EmitError::new(format!(
                    "a `{}` constant is outside the hvm core — `.hbc` v0 supports only Int, Float, and Bool constants",
                    value_kind(other)
                )));
            }
        };
        for (i, c) in self.consts.iter().enumerate() {
            let same = match (c, &hc) {
                (HConst::Int(a), HConst::Int(b)) => a == b,
                (HConst::Bool(a), HConst::Bool(b)) => a == b,
                // Float dedup by bit pattern (matches the reference emitter).
                (HConst::Float(a), HConst::Float(b)) => a.to_bits() == b.to_bits(),
                _ => false,
            };
            if same {
                return Ok(i as u32);
            }
        }
        self.consts.push(hc);
        Ok((self.consts.len() - 1) as u32)
    }
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::Bool(_) => "Bool",
        Value::Str(_) => "String",
        Value::Array(_) => "Array",
        Value::Tuple(_) => "Tuple",
        Value::Record(_) => "Record",
        _ => "non-scalar",
    }
}

/// Map a Helix [`BinOp`] to an hvm binary selector, or reject it.
///
/// `Div` is deliberately NOT mapped even though hvm has a `DIV` selector: Helix's
/// `/` is *always float division* (Int operands promote — `7 / 2` is `3.5`), while
/// hvm's `DIV` on two Ints is integer division (`7 / 2` is `3`) and its Float `DIV`
/// yields `inf` on a zero divisor where Helix raises. Emitting it would make the
/// same blob compute different answers on host and in ring 0 — the exact divergence
/// the seam guarantees can't happen (proven empirically before this guard was
/// added). `/` joins the reject list until hvm grows Int→Float promotion.
fn binop_sel(bop: &BinOp) -> Result<u8, EmitError> {
    Ok(match bop {
        BinOp::Add => binop::ADD,
        BinOp::Sub => binop::SUB,
        BinOp::Mul => binop::MUL,
        BinOp::Div => {
            return Err(EmitError::new(
                "`/` is outside the hvm v0 core — Helix `/` is float division with Int→Float \
                 promotion, which hvm's DIV does not implement (it would integer-divide Ints, \
                 silently diverging from the host); division awaits an hvm ISA extension",
            ));
        }
        BinOp::Lt => binop::LT,
        BinOp::Le => binop::LE,
        BinOp::Gt => binop::GT,
        BinOp::Ge => binop::GE,
        BinOp::Eq => binop::EQ,
        BinOp::Ne => binop::NE,
        other => {
            return Err(EmitError::new(format!(
                "the `{}` operator is outside the hvm core — `.hbc` v0 supports + - * < <= > >= == !=",
                other.symbol()
            )));
        }
    })
}

/// Emit a `.hbc` blob containing `entry_name` and every function reachable from it
/// via direct/tail calls, with the entry placed at index 0 (so the caller runs
/// index 0, matching hvm's `demo_ids` convention).
pub fn emit(program: &Program, entry_name: &str) -> Result<Emitted, EmitError> {
    let entry_idx = resolve_entry(program, entry_name)?;

    // Reachable functions, entry first (BFS over direct/tail calls). `<main>` and
    // any unrelated top-level function are excluded unless the entry calls them.
    let order = reachable_order(program, entry_idx)?;

    // old function index -> new (emitted) index.
    let mut new_index: Vec<Option<u32>> = vec![None; program.funcs.len()];
    for (new, &old) in order.iter().enumerate() {
        new_index[old] = Some(new as u32);
    }

    // Precompute, per builtin index, the host-call index it lowers to (or `None` if
    // the builtin isn't a supported host call). `CallBuiltin` ops index this table.
    let builtin_host: Vec<Option<u32>> =
        program.builtins.iter().map(|b| host::for_builtin(b)).collect();

    // Lower every reachable function, accreting the shared constant pool.
    let mut pool = ConstPool::default();
    let mut chunks: Vec<(u32, u32, Vec<u8>)> = Vec::with_capacity(order.len());
    let mut funcs_meta: Vec<EmittedFn> = Vec::with_capacity(order.len());
    for (new, &old) in order.iter().enumerate() {
        let chunk = &program.funcs[old];
        let name = program.func_names.get(old).cloned().unwrap_or_default();
        let code = lower_chunk(chunk, &new_index, &mut pool, &name, &builtin_host)?;
        chunks.push((chunk.n_params, chunk.n_locals, code));
        funcs_meta.push(EmittedFn {
            name,
            index: new as u32,
            nargs: chunk.n_params,
            nlocals: chunk.n_locals,
        });
    }

    let bytes = serialize(&pool, &chunks);
    Ok(Emitted { bytes, funcs: funcs_meta, nconsts: pool.consts.len() })
}

/// Locate the entry function by name (an exact `func_names` match).
fn resolve_entry(program: &Program, name: &str) -> Result<usize, EmitError> {
    if let Some(i) = program.func_names.iter().position(|n| n == name) {
        return Ok(i);
    }
    let available: Vec<&str> = program
        .func_names
        .iter()
        .map(|s| s.as_str())
        .filter(|n| *n != "<main>")
        .collect();
    Err(EmitError::new(format!(
        "no function named `{name}` in the program (available: {})",
        if available.is_empty() { "none".to_string() } else { available.join(", ") }
    )))
}

/// The functions reachable from `entry` (breadth-first over `CallFn`/`TailCallFn`),
/// with `entry` first. This is the set that gets emitted and reindexed.
fn reachable_order(program: &Program, entry: usize) -> Result<Vec<usize>, EmitError> {
    let mut order = Vec::new();
    let mut seen = vec![false; program.funcs.len()];
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(entry);
    seen[entry] = true;
    while let Some(f) = queue.pop_front() {
        order.push(f);
        for opx in &program.funcs[f].code {
            let callee = match opx {
                Op::CallFn { idx, .. } | Op::TailCallFn { idx, .. } => Some(*idx as usize),
                _ => None,
            };
            if let Some(c) = callee {
                if c >= program.funcs.len() {
                    return Err(EmitError::new(format!(
                        "call to out-of-range function index {c}"
                    )));
                }
                if !seen[c] {
                    seen[c] = true;
                    queue.push_back(c);
                }
            }
        }
    }
    Ok(order)
}

/// Lower one Helix chunk to hvm code bytes. Two passes: measure each instruction's
/// byte length (validating it is in-subset) to build an instruction-index → byte-
/// offset table, then emit while patching jump targets through that table.
fn lower_chunk(
    chunk: &Chunk,
    new_index: &[Option<u32>],
    pool: &mut ConstPool,
    fname: &str,
    builtin_host: &[Option<u32>],
) -> Result<Vec<u8>, EmitError> {
    let n = chunk.code.len();

    // Pass A — off[j] = byte offset where instruction j's lowering begins; off[n]
    // is the total code length. Lengths are fixed per op kind, so this needs no
    // operand values (jump targets resolve in pass B).
    let mut off = vec![0u32; n + 1];
    for (j, opx) in chunk.code.iter().enumerate() {
        let len = op_len(opx, builtin_host).map_err(|e| in_fn(fname, j, e))?;
        off[j + 1] = off[j] + len;
    }
    let total = off[n];

    // Pass B — emit.
    let mut out: Vec<u8> = Vec::with_capacity(total as usize);
    for (j, opx) in chunk.code.iter().enumerate() {
        emit_op(opx, chunk, &off, new_index, pool, &mut out, builtin_host)
            .map_err(|e| in_fn(fname, j, e))?;
    }
    debug_assert_eq!(out.len() as u32, total, "measured and emitted lengths disagree");
    Ok(out)
}

fn in_fn(fname: &str, j: usize, e: EmitError) -> EmitError {
    EmitError::new(format!("in function `{fname}` at instruction {j}: {}", e.message))
}

/// The number of hvm bytes an in-subset op lowers to (also validates the op kind and
/// its operator, so pass A fails fast on anything unsupported).
fn op_len(opx: &Op, builtin_host: &[Option<u32>]) -> Result<u32, EmitError> {
    Ok(match opx {
        Op::Const(_) => 5,
        Op::LoadLocal(_) => 5,
        Op::StoreLocal(_) => 5,
        Op::Binary(b) => {
            binop_sel(b)?;
            2
        }
        // Superinstructions expand to their primitive sequences.
        Op::LoadLocalBinary(_, _, b) => {
            binop_sel(b)?;
            12 // LOAD_LOCAL + LOAD_LOCAL + BINARY
        }
        Op::LoadLocalConstBinary(_, _, b) => {
            binop_sel(b)?;
            12 // LOAD_LOCAL + CONST + BINARY
        }
        Op::ConstBinary(_, b) => {
            binop_sel(b)?;
            7 // CONST + BINARY
        }
        Op::Jump(_) => 5,
        Op::JumpIfFalse(_) => 5,
        Op::CallFn { .. } => 9,
        Op::TailCallFn { .. } => 9,
        // A host-mapped builtin (print/emit/elog/sleep) lowers to a CALL_HOST
        // (op + host_idx u32 + nargs u32); others fall through and are rejected.
        Op::CallBuiltin { idx, .. }
            if builtin_host.get(*idx as usize).copied().flatten().is_some() =>
        {
            9
        }
        Op::Return => 1,
        Op::Pop => 1,
        other => return Err(EmitError::new(unsupported_op(other))),
    })
}

/// Emit one op's hvm bytes into `out`.
fn emit_op(
    opx: &Op,
    chunk: &Chunk,
    off: &[u32],
    new_index: &[Option<u32>],
    pool: &mut ConstPool,
    out: &mut Vec<u8>,
    builtin_host: &[Option<u32>],
) -> Result<(), EmitError> {
    match opx {
        Op::Const(k) => {
            let gi = intern_chunk_const(chunk, *k, pool)?;
            out.push(op::CONST);
            put_u32(out, gi);
        }
        Op::LoadLocal(i) => {
            out.push(op::LOAD_LOCAL);
            put_u32(out, *i);
        }
        Op::StoreLocal(i) => {
            out.push(op::STORE_LOCAL);
            put_u32(out, *i);
        }
        Op::Binary(b) => {
            out.push(op::BINARY);
            out.push(binop_sel(b)?);
        }
        Op::LoadLocalBinary(a, b, bop) => {
            out.push(op::LOAD_LOCAL);
            put_u32(out, *a);
            out.push(op::LOAD_LOCAL);
            put_u32(out, *b);
            out.push(op::BINARY);
            out.push(binop_sel(bop)?);
        }
        Op::LoadLocalConstBinary(a, k, bop) => {
            out.push(op::LOAD_LOCAL);
            put_u32(out, *a);
            let gi = intern_chunk_const(chunk, *k, pool)?;
            out.push(op::CONST);
            put_u32(out, gi);
            out.push(op::BINARY);
            out.push(binop_sel(bop)?);
        }
        Op::ConstBinary(k, bop) => {
            let gi = intern_chunk_const(chunk, *k, pool)?;
            out.push(op::CONST);
            put_u32(out, gi);
            out.push(op::BINARY);
            out.push(binop_sel(bop)?);
        }
        Op::Jump(t) => {
            out.push(op::JUMP);
            put_u32(out, jump_target(*t, off)?);
        }
        Op::JumpIfFalse(t) => {
            out.push(op::JUMP_IF_FALSE);
            put_u32(out, jump_target(*t, off)?);
        }
        Op::CallFn { idx, nargs } => {
            let ni = remap_fn(*idx, new_index)?;
            out.push(op::CALL_FN);
            put_u32(out, ni);
            put_u32(out, *nargs);
        }
        Op::TailCallFn { idx, nargs } => {
            let ni = remap_fn(*idx, new_index)?;
            out.push(op::TAIL_CALL_FN);
            put_u32(out, ni);
            put_u32(out, *nargs);
        }
        // A host-mapped builtin → `CALL_HOST host_idx nargs`. The runtime's handler
        // performs the effect and pushes a result, so these are expressions.
        Op::CallBuiltin { idx, nargs }
            if builtin_host.get(*idx as usize).copied().flatten().is_some() =>
        {
            let h = builtin_host[*idx as usize].unwrap();
            out.push(op::CALL_HOST);
            put_u32(out, h);
            put_u32(out, *nargs);
        }
        Op::Return => out.push(op::RETURN),
        Op::Pop => out.push(op::POP),
        other => return Err(EmitError::new(unsupported_op(other))),
    }
    Ok(())
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Intern chunk-local constant `k` into the program-global pool, returning its hvm
/// index.
fn intern_chunk_const(chunk: &Chunk, k: u32, pool: &mut ConstPool) -> Result<u32, EmitError> {
    let v = chunk
        .consts
        .get(k as usize)
        .ok_or_else(|| EmitError::new(format!("constant index {k} out of range")))?;
    pool.intern(v)
}

/// Remap a Helix function index to its emitted index (must be in the reachable set).
fn remap_fn(idx: u32, new_index: &[Option<u32>]) -> Result<u32, EmitError> {
    new_index
        .get(idx as usize)
        .copied()
        .flatten()
        .ok_or_else(|| EmitError::new(format!("call to function {idx}, which is not in the emitted set")))
}

/// Translate a Helix instruction-index jump target to an hvm byte offset.
fn jump_target(t: u32, off: &[u32]) -> Result<u32, EmitError> {
    let ninstr = off.len() - 1; // off has n+1 entries; valid indices are 0..ninstr.
    let ti = t as usize;
    if ti >= ninstr {
        return Err(EmitError::new(format!(
            "jump target {t} is not a valid instruction index (0..{}) — jumps past the end aren't supported",
            ninstr
        )));
    }
    Ok(off[ti])
}

/// Serialize the container. Byte-identical to `hvm::build::ProgramBuilder::serialize`:
/// header, constant pool, function table (16 bytes/entry, cumulative `code_off`),
/// then all code blobs concatenated.
fn serialize(pool: &ConstPool, chunks: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(pool.consts.len() as u32).to_le_bytes());
    for c in &pool.consts {
        match c {
            HConst::Int(v) => {
                out.push(TAG_INT);
                out.extend_from_slice(&(*v as u64).to_le_bytes());
            }
            HConst::Float(v) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&v.to_bits().to_le_bytes());
            }
            HConst::Bool(v) => {
                out.push(TAG_BOOL);
                out.extend_from_slice(&(*v as u64).to_le_bytes());
            }
        }
    }
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    let mut code_off = 0u32;
    for (nargs, nlocals, code) in chunks {
        out.extend_from_slice(&nargs.to_le_bytes());
        out.extend_from_slice(&nlocals.to_le_bytes());
        out.extend_from_slice(&code_off.to_le_bytes());
        out.extend_from_slice(&(code.len() as u32).to_le_bytes());
        code_off += code.len() as u32;
    }
    for (_, _, code) in chunks {
        out.extend_from_slice(code);
    }
    out
}

/// A human-readable reason a given op is outside the hvm core subset. Only the
/// common, likely-to-be-hit variants get a tailored category; the exotic tail
/// (DataFrame/GroupBy verbs, comprehension iterators, JIT fast paths) falls to the
/// generic message.
fn unsupported_op(opx: &Op) -> String {
    let what = match opx {
        Op::LoadGlobal(_) | Op::StoreGlobal(_) => "global variables",
        Op::Unary(_) => "unary operators (`-x`, `!x`)",
        Op::MatchArm(_) => "`match`",
        Op::AndCheck(_) | Op::AndCombine | Op::OrCheck(_) | Op::OrCombine | Op::CoalesceCheck(_) => {
            "short-circuit `and` / `or` / `??`"
        }
        Op::MakeFunc { .. } | Op::MakeClosure(_) | Op::GetUpvalue(_) | Op::CallValue(_) => {
            "first-class functions / closures"
        }
        Op::CallBuiltin { .. } => "builtin calls",
        Op::MakeArray(_) | Op::Index | Op::Slice(_) => "arrays / indexing / slicing",
        Op::Interp(_) => "string interpolation",
        Op::MakeTuple(_) => "tuples",
        Op::MakeRecord(_) | Op::UpdateRecord(_) | Op::GetField(_) | Op::GetFieldOrMissing(_) => "records",
        Op::Destructure(_) | Op::DestructureBind(_) => "destructuring",
        Op::Method(_) => "value methods",
        _ => "this operation",
    };
    format!(
        "{what} is outside the hvm core subset — not serializable to `.hbc` (the v0 minimal core is Int/Float/Bool `+ - *` and comparisons, frame locals, `if`/`while`, and direct + tail calls)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile a source string all the way to a bytecode `Program`.
    fn compile_src(src: &str) -> Program {
        let toks = crate::lexer::lex(src).expect("lex");
        let stmts = crate::parser::parse(toks).expect("parse");
        let types = crate::types::check(&stmts).expect("check");
        crate::bytecode::compile_with_types(&stmts, Some(types)).expect("compile")
    }

    fn rd_u32(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    /// `fn compute() = 2 + 3` emits a well-formed, minimal container: HBC0 header,
    /// version 0, two Int constants, one function (the entry) at index 0.
    #[test]
    fn emit_compute_layout() {
        let prog = compile_src("fn compute() = 2 + 3\n");
        let e = emit(&prog, "compute").expect("emit");

        assert_eq!(&e.bytes[0..4], b"HBC0", "magic");
        assert_eq!(rd_u32(&e.bytes, 4), 0, "version");
        assert_eq!(rd_u32(&e.bytes, 8), e.nconsts as u32, "nconsts field matches");
        assert_eq!(e.nconsts, 2, "constants 2 and 3 interned");

        // The entry is reindexed to 0; <main> is not reachable from it, so it is dropped.
        assert_eq!(e.funcs.len(), 1);
        assert_eq!(e.funcs[0].name, "compute");
        assert_eq!(e.funcs[0].index, 0);
        assert_eq!(e.funcs[0].nargs, 0);

        // nfuncs sits right after the constant pool (12 + nconsts*9).
        assert_eq!(rd_u32(&e.bytes, 12 + 2 * 9), 1, "nfuncs");
    }

    /// A recursive function lowers: its self-call is reindexed to 0, and the whole
    /// blob is self-consistent (function-table code slices lie within the code blob).
    #[test]
    fn emit_recursion_reindexes() {
        let prog = compile_src("fn fib(n) = if n < 2 then n else fib(n - 1) + fib(n - 2)\n");
        let e = emit(&prog, "fib").expect("emit");
        assert_eq!(e.funcs.len(), 1, "only fib is reachable from fib");
        assert_eq!(e.funcs[0].index, 0);
        assert_eq!(e.funcs[0].nargs, 1);

        // Decode the (single) function-table entry and confirm the code slice fits.
        let nconsts = rd_u32(&e.bytes, 8) as usize;
        let table_off = 12 + nconsts * 9 + 4; // header + pool + nfuncs
        let code_off = rd_u32(&e.bytes, table_off + 8) as usize;
        let code_len = rd_u32(&e.bytes, table_off + 12) as usize;
        let blob_start = table_off + 16; // one 16-byte entry
        assert!(blob_start + code_off + code_len <= e.bytes.len(), "code slice in bounds");
    }

    /// Anything outside the core subset (here: an array literal) is refused with a
    /// clear error rather than emitting a blob the VM can't run.
    #[test]
    fn rejects_out_of_core() {
        let prog = compile_src("fn f() = [1, 2, 3]\n");
        let err = emit(&prog, "f").expect_err("array must be rejected");
        assert!(
            err.message.contains("outside the hvm core") || err.message.contains("arrays"),
            "unexpected message: {}",
            err.message
        );
    }

    /// `/` must be refused: Helix `/` is float division with Int→Float promotion,
    /// hvm's DIV integer-divides Ints — emitting it produced a blob that computed
    /// `7 / 2` as `3` in ring 0 while the host says `3.5` (a proven divergence the
    /// seam's bit-identical guarantee forbids). Refuse, don't diverge.
    #[test]
    fn rejects_division() {
        let prog = compile_src("fn d() = 7 / 2\n");
        let err = emit(&prog, "d").expect_err("`/` must be rejected");
        assert!(err.message.contains('/'), "unexpected message: {}", err.message);
    }

    /// Host-mapped builtins (`print`/`emit`/`elog` → CALL_HOST 0; `sleep` → CALL_HOST 1)
    /// emit; other builtins are still rejected.
    #[test]
    fn host_mapped_builtins_emit() {
        for src in [
            "fn f(n) = print(n)\n",
            "fn f(n) = emit(n)\n",
            "fn f(n) = sleep(n)\n",
            "fn f() = print(read_int())\n",
        ] {
            assert!(emit(&compile_src(src), "f").is_ok(), "should emit: {src}");
        }
        // A non-host builtin (e.g. `sqrt`) is not a host call — still rejected.
        assert!(
            emit(&compile_src("fn f(n) = sqrt(n)\n"), "f").is_err(),
            "sqrt is not host-mapped and must be rejected"
        );
    }
}

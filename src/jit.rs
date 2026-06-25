//! Cranelift JIT — Track B of the performance roadmap.
//!
//! Compiles the numeric-recursion core of a program to **native machine code**,
//! specialized per concrete type (the Julia recipe: monomorphization). The VM
//! dispatches to a native version when the argument types match, falling back to
//! bytecode otherwise. Once execution enters native code, all internal recursion
//! stays native, which is where the speed comes from.
//!
//! **Currently only the `i64` specialization is emitted** (see [`build`]): with
//! all-`Int` args every op yields `Int`, exactly matching the interpreter. The
//! `f64` codegen below is complete but **dormant** — a float-arg function can
//! still return an `Int` (a literal, or an Int-only subexpression), so emitting it
//! would diverge from the interpreter on result type; float functions run on the
//! VM instead. So the IEEE-754 NaN-comparison edge the f64 path would introduce
//! (a NaN compare is `false`, where the interpreter raises) is **latent, not
//! active** — there is no live JIT/interpreter divergence today.
//!
//! A function is eligible (for a given numeric kind) when its body uses only
//! constructs that lower to that kind: literals, params/`let` locals, the kind's
//! arithmetic (`+ - *` for both; `/` additionally for `f64`), an `if` with a
//! comparison condition, and calls to other functions eligible in the same kind.
//! Eligibility is a fixpoint (a fn calling an ineligible fn is ineligible) and
//! caps arity at 4.
//!
//! Semantics match the interpreter's *release* behaviour: integer arithmetic
//! wraps on overflow.
//!
//! SAFETY: calling generated code is inherently `unsafe`. The two `unsafe` blocks
//! are confined to [`call_i64`]/[`call_f64`], guarded by the VM's type/arity
//! check so the native ABI contract always holds. The JIT deals only in scalar
//! `i64`/`f64` — no heap, no `Rc` — so it adds no leak surface.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F64, I64};
use cranelift_codegen::ir::{AbiParam, InstBuilder, Type, Value as ClValue};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::ast::{BinOp, Expr, Stmt, TypeAnn};

const MAX_ARITY: usize = 4;

/// The two scalar specializations.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum NumKind {
    Int,
    Float,
}

impl NumKind {
    fn cl_type(self) -> Type {
        match self {
            NumKind::Int => I64,
            NumKind::Float => F64,
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            NumKind::Int => "i",
            NumKind::Float => "f",
        }
    }
}

/// The native entry points for one user function (whichever specializations
/// compiled), plus its arity. `Copy` so the VM can pull it out cheaply.
#[derive(Clone, Copy)]
pub struct NativeFn {
    pub i64_ptr: Option<*const u8>,
    pub f64_ptr: Option<*const u8>,
    pub arity: usize,
}

/// Owns the JIT module (and thus the executable code) plus the name → entry-point
/// table. Must outlive every native call.
pub struct Jit {
    _module: JITModule,
    by_name: HashMap<String, NativeFn>,
    /// Native `extern "C" fn(i64,i64,i64)->i64` reduce loops, indexed by the
    /// `loop_idx` of [`crate::bytecode::Op::TryJitReduce`]. `None` for a site that
    /// the JIT declined (kept as a slot so indices stay aligned with the compiler).
    reduce_ptrs: Vec<Option<*const u8>>,
}

impl Jit {
    pub fn lookup(&self, name: &str) -> Option<NativeFn> {
        self.by_name.get(name).copied()
    }
    /// The native reduce loop for site `idx`, if one compiled.
    pub fn reduce_loop(&self, idx: usize) -> Option<*const u8> {
        self.reduce_ptrs.get(idx).copied().flatten()
    }
}

struct FnDef<'a> {
    name: &'a str,
    params: &'a [(String, Option<TypeAnn>)],
    body: &'a Expr,
}

/// Compile every eligible numeric function — and every JIT-eligible `reduce` loop
/// the compiler flagged — to native code, or `None` if nothing is eligible (so the
/// caller skips JIT setup).
pub fn build(program: &[Stmt], reduce_loops: &[crate::bytecode::ReduceLoop]) -> Option<Jit> {
    // The native call transmutes Cranelift output to `extern "C"`, which matches
    // the convention we force (SystemV) only on x86-64 Linux. On every other
    // target, decline to JIT and let the VM run everything — correct, just not
    // native. (This is the conservative fix for the ABI-soundness hazard.)
    if !cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        return None;
    }

    let funcs: Vec<FnDef> = program
        .iter()
        .filter_map(|s| match s {
            Stmt::Func { name, params, body, .. } => Some(FnDef { name, params, body }),
            _ => None,
        })
        .collect();
    if funcs.is_empty() && reduce_loops.is_empty() {
        return None;
    }

    let mut module = make_module()?;
    let mut compiled: Vec<(String, NumKind, FuncId, usize)> = Vec::new();

    // Only the i64 specialization is type-safe: with all-`Int` args every op (and
    // returning a param) yields `Int`, exactly matching the interpreter. The f64
    // specialization always returns `Float`, but a float-arg function can still
    // produce an `Int` (a literal, or an Int-only subexpression) — so f64 codegen
    // would diverge from the interpreter on result type. Float functions run on
    // the VM instead (correct, and they're not hot after recursion is excluded).
    for kind in [NumKind::Int] {
        let eligible = eligible_set(&funcs, kind);
        if eligible.is_empty() {
            continue;
        }

        // Declare every function of this kind first so intra-kind calls resolve.
        let mut fn_ids: HashMap<&str, FuncId> = HashMap::new();
        for f in &funcs {
            if eligible.contains(f.name) {
                let mut sig = module.make_signature();
                // Force SystemV to match the `extern "C"` transmute on x86-64 Linux
                // (rather than relying on the ISA default).
                sig.call_conv = CallConv::SystemV;
                for _ in f.params {
                    sig.params.push(AbiParam::new(kind.cl_type()));
                }
                sig.returns.push(AbiParam::new(kind.cl_type()));
                let id = module
                    .declare_function(&format!("{}${}", f.name, kind.suffix()), Linkage::Local, &sig)
                    .ok()?;
                fn_ids.insert(f.name, id);
            }
        }

        // Define each body.
        let mut ctx = module.make_context();
        let mut bctx = FunctionBuilderContext::new();
        for f in &funcs {
            if !eligible.contains(f.name) {
                continue;
            }
            ctx.func.signature.call_conv = CallConv::SystemV;
            for _ in f.params {
                ctx.func.signature.params.push(AbiParam::new(kind.cl_type()));
            }
            ctx.func.signature.returns.push(AbiParam::new(kind.cl_type()));

            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut bctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            builder.seal_block(entry);

            let mut vars: HashMap<&str, Variable> = HashMap::new();
            for (i, (pname, _)) in f.params.iter().enumerate() {
                let pv = builder.block_params(entry)[i];
                let var = builder.declare_var(kind.cl_type());
                builder.def_var(var, pv);
                vars.insert(pname.as_str(), var);
            }

            let ret = gen_value(&mut builder, f.body, &mut vars, &fn_ids, &mut module, kind);
            builder.ins().return_(&[ret]);
            builder.finalize();

            module.define_function(fn_ids[f.name], &mut ctx).ok()?;
            module.clear_context(&mut ctx);

            compiled.push((f.name.to_string(), kind, fn_ids[f.name], f.params.len()));
        }
    }

    // Compile each flagged `reduce` loop into a native `fn(i64,i64,i64)->i64`. We
    // re-check eligibility defensively: if a site somehow isn't compilable, its
    // slot stays `None` and the VM keeps running the bytecode loop for it.
    let mut reduce_ids: Vec<Option<FuncId>> = Vec::with_capacity(reduce_loops.len());
    {
        let mut ctx = module.make_context();
        let mut bctx = FunctionBuilderContext::new();
        for (i, rl) in reduce_loops.iter().enumerate() {
            if !reduce_loop_eligible(&rl.body, &rl.pa, &rl.pb) {
                reduce_ids.push(None);
                continue;
            }
            let mut sig = module.make_signature();
            sig.call_conv = CallConv::SystemV;
            for _ in 0..3 {
                sig.params.push(AbiParam::new(I64));
            }
            sig.returns.push(AbiParam::new(I64));
            let id = match module.declare_function(&format!("reduce${i}"), Linkage::Local, &sig) {
                Ok(id) => id,
                Err(_) => {
                    reduce_ids.push(None);
                    continue;
                }
            };
            match define_reduce_loop(&mut module, &mut ctx, &mut bctx, id, rl) {
                Some(()) => reduce_ids.push(Some(id)),
                None => reduce_ids.push(None),
            }
        }
    }

    if compiled.is_empty() && reduce_ids.iter().all(|r| r.is_none()) {
        return None;
    }
    module.finalize_definitions().ok()?;

    let mut by_name: HashMap<String, NativeFn> = HashMap::new();
    for (name, kind, id, arity) in compiled {
        let ptr = module.get_finalized_function(id);
        let entry = by_name
            .entry(name)
            .or_insert(NativeFn { i64_ptr: None, f64_ptr: None, arity });
        match kind {
            NumKind::Int => entry.i64_ptr = Some(ptr),
            NumKind::Float => entry.f64_ptr = Some(ptr),
        }
    }

    let reduce_ptrs: Vec<Option<*const u8>> = reduce_ids
        .into_iter()
        .map(|id| id.map(|id| module.get_finalized_function(id)))
        .collect();

    Some(Jit { _module: module, by_name, reduce_ptrs })
}

/// Build the JIT module, or `None` on any setup failure (so the caller falls
/// back to the VM instead of aborting — these `unwrap`s previously turned a
/// renamed Cranelift flag or an unsupported host into a hard process abort).
fn make_module() -> Option<JITModule> {
    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").ok()?;
    flag_builder.set("is_pic", "false").ok()?;
    // Optimize generated code — the e-graph mid-end (constant folding, GVN,
    // strength reduction, branch/alias opts). Default is "none"; this is a free
    // codegen-quality win, paid once at compile time.
    flag_builder.set("opt_level", "speed").ok()?;
    let isa_builder = cranelift_native::builder().ok()?;
    let isa = isa_builder.finish(settings::Flags::new(flag_builder)).ok()?;
    let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    Some(JITModule::new(builder))
}

// ---------- eligibility ----------

/// True if `body` is a pure `i64` expression over `{pa, pb}` the JIT can compile
/// into a native reduce loop: integer literals, the two binders, `+ - *`,
/// comparisons inside an `if`, and `let` — but **no** floats, division, function
/// calls, or other free identifiers. The bytecode compiler calls this to decide
/// whether to emit a `TryJitReduce` guard, so it is the single source of truth for
/// reduce-loop eligibility (and is platform-independent — the native code is only
/// emitted by `build`, which is gated to x86-64 Linux).
pub fn reduce_loop_eligible(body: &Expr, pa: &str, pb: &str) -> bool {
    // An empty eligible-fn set makes any `Call` ineligible (no cross-fn calls in a
    // native reduce loop), exactly matching the codegen below.
    let no_fns: HashSet<&str> = HashSet::new();
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(pa);
    locals.insert(pb);
    value_eligible(body, &no_fns, &locals, NumKind::Int)
}

fn eligible_set<'a>(funcs: &[FnDef<'a>], kind: NumKind) -> HashSet<&'a str> {
    // Exclude every function on a recursion *cycle* — directly self-recursive OR
    // mutually recursive. A JIT'd function recurses on the native stack with no
    // depth guard, so unbounded recursion (a missing base case) would overflow the
    // native stack and crash the process instead of raising a clean, catchable
    // error. This is a transitive call-graph check, not just a direct self-call
    // test: the JIT's memory safety must NOT silently depend on the front-end's
    // define-before-use rule (which makes mutual recursion unrepresentable today,
    // but is a front-end policy that could change — see `recursive_funcs`).
    // Recursive functions run on the depth-guarded VM (or are memoized) instead.
    let recursive = recursive_funcs(funcs);
    let mut eligible: HashSet<&str> = funcs
        .iter()
        .filter(|f| f.params.len() <= MAX_ARITY && !recursive.contains(f.name))
        .map(|f| f.name)
        .collect();
    loop {
        let snapshot = eligible.clone();
        let mut changed = false;
        for f in funcs {
            if snapshot.contains(f.name) {
                let locals: HashSet<&str> = f.params.iter().map(|(n, _)| n.as_str()).collect();
                if !value_eligible(f.body, &snapshot, &locals, kind) {
                    eligible.remove(f.name);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    eligible
}

/// Names of functions that lie on a call-graph cycle — directly self-recursive
/// (`f` calls `f`) or mutually recursive (`f` -> `g` -> ... -> `f`). Such a
/// function can reach itself through call edges, so JIT-compiling it would put
/// unguarded recursion on the native stack. The check is *transitive* by design:
/// it keeps the JIT memory-safe regardless of whether the front-end permits the
/// cycle to be written. Today the parser's define-before-use rule makes mutual
/// recursion unrepresentable, so this currently coincides with the direct
/// self-call test — but the JIT no longer *depends* on that front-end policy.
fn recursive_funcs<'a>(funcs: &[FnDef<'a>]) -> HashSet<&'a str> {
    let n = funcs.len();
    // Call graph over the user functions: edge i -> j iff funcs[i]'s body calls
    // funcs[j] (by name). `body_calls` is the per-edge primitive.
    let adj: Vec<Vec<usize>> = funcs
        .iter()
        .map(|f| (0..n).filter(|&j| body_calls(f.body, funcs[j].name)).collect())
        .collect();
    let mut recursive = HashSet::new();
    for i in 0..n {
        // Reachability: can function i reach itself through call edges?
        let mut seen = vec![false; n];
        let mut stack = adj[i].clone();
        while let Some(u) = stack.pop() {
            if u == i {
                recursive.insert(funcs[i].name);
                break;
            }
            if !seen[u] {
                seen[u] = true;
                stack.extend_from_slice(&adj[u]);
            }
        }
    }
    recursive
}

/// True if `e` contains a call to function `name`. The per-edge primitive for the
/// `recursive_funcs` call graph. Only the node kinds that can appear in an eligible
/// body need traversal; anything else means the function is ineligible anyway.
fn body_calls(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Call { name: callee, args, .. } => {
            callee == name || args.iter().any(|a| body_calls(a, name))
        }
        Expr::Binary { left, right, .. } => {
            body_calls(left, name) || body_calls(right, name)
        }
        Expr::Unary { expr, .. } => body_calls(expr, name),
        Expr::If { cond, then_branch, else_branch, .. } => {
            body_calls(cond, name)
                || body_calls(then_branch, name)
                || body_calls(else_branch, name)
        }
        Expr::Let { bindings, body } => {
            bindings.iter().any(|(_, v)| body_calls(v, name))
                || body_calls(body, name)
        }
        _ => false,
    }
}

fn value_eligible(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, kind: NumKind) -> bool {
    match e {
        Expr::Int(_) => true,
        // A float literal is only representable in the `f64` specialization.
        Expr::Float(_) => kind == NumKind::Float,
        Expr::Ident { name, .. } => locals.contains(name.as_str()),
        Expr::Binary { op, left, right, .. } => {
            // NOTE: `Div` is intentionally excluded. For `Int`, the interpreter
            // returns a `Float` (`10 / 2 == 5.0`), so `/` is not i64-closed at all;
            // and native `fdiv` yields ±inf where the interpreter errors on /0.
            // Functions using `/` fall back to the VM/interpreter.
            let op_ok = match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => true,
                // `Int % Int` *is* i64-closed (`a.rem_euclid(b)`). We JIT it only
                // when the divisor is a **positive integer constant**: that rules
                // out `%0` (which must raise "modulo by zero") and the negative-
                // divisor sign subtleties, so native `rem_euclid` is total and
                // matches the interpreter exactly. (Float kind is unused today.)
                BinOp::Mod => {
                    kind == NumKind::Int && matches!(**right, Expr::Int(n) if n > 0)
                }
                _ => false,
            };
            op_ok
                && value_eligible(left, eligible, locals, kind)
                && value_eligible(right, eligible, locals, kind)
        }
        Expr::Call { name, args, .. } => {
            eligible.contains(name.as_str())
                && args.iter().all(|a| value_eligible(a, eligible, locals, kind))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            cond_eligible(cond, eligible, locals, kind)
                && value_eligible(then_branch, eligible, locals, kind)
                && value_eligible(else_branch, eligible, locals, kind)
        }
        Expr::Let { bindings, body } => {
            let mut locals2 = locals.clone();
            for (n, v) in bindings {
                if !value_eligible(v, eligible, &locals2, kind) {
                    return false;
                }
                locals2.insert(n.as_str());
            }
            value_eligible(body, eligible, &locals2, kind)
        }
        _ => false,
    }
}

fn cond_eligible(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, kind: NumKind) -> bool {
    match e {
        Expr::Binary { op, left, right, .. } => matches!(
            op,
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne
        ) && value_eligible(left, eligible, locals, kind)
            && value_eligible(right, eligible, locals, kind),
        _ => false,
    }
}

// ---------- codegen ----------

/// Emit a native loop for `range(start,end).reduce(init, (pa,pb)=>body)`:
/// `acc=init; x=start; while x<end { acc=body(acc,x); x+=1 } return acc`.
/// Integer arithmetic wraps (`iadd`/`imul`), matching the interpreter's release
/// semantics, and the empty range returns `init` — identical to the VM loop.
fn define_reduce_loop(
    module: &mut JITModule,
    ctx: &mut cranelift_codegen::Context,
    bctx: &mut FunctionBuilderContext,
    fid: FuncId,
    rl: &crate::bytecode::ReduceLoop,
) -> Option<()> {
    ctx.func.signature.call_conv = CallConv::SystemV;
    for _ in 0..3 {
        ctx.func.signature.params.push(AbiParam::new(I64));
    }
    ctx.func.signature.returns.push(AbiParam::new(I64));

    let mut b = FunctionBuilder::new(&mut ctx.func, bctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let start = b.block_params(entry)[0];
    let end = b.block_params(entry)[1];
    let init = b.block_params(entry)[2];

    let acc_var = b.declare_var(I64);
    let x_var = b.declare_var(I64);
    let end_var = b.declare_var(I64);
    b.def_var(acc_var, init);
    b.def_var(x_var, start);
    b.def_var(end_var, end);

    let header = b.create_block();
    let body_blk = b.create_block();
    let exit_blk = b.create_block();

    b.ins().jump(header, &[]);

    // header: `x < end ?` — branch into the body or out. Sealed only after the
    // body's back-edge is emitted (its two predecessors are entry and body).
    b.switch_to_block(header);
    let xv = b.use_var(x_var);
    let ev = b.use_var(end_var);
    let cond = b.ins().icmp(IntCC::SignedLessThan, xv, ev);
    b.ins().brif(cond, body_blk, &[], exit_blk, &[]);

    b.switch_to_block(body_blk);
    b.seal_block(body_blk);
    let mut vars: HashMap<&str, Variable> = HashMap::new();
    vars.insert(rl.pa.as_str(), acc_var);
    vars.insert(rl.pb.as_str(), x_var);
    let no_fns: HashMap<&str, FuncId> = HashMap::new(); // bodies contain no calls
    let new_acc = gen_value(&mut b, &rl.body, &mut vars, &no_fns, module, NumKind::Int);
    b.def_var(acc_var, new_acc);
    let xv2 = b.use_var(x_var);
    let one = b.ins().iconst(I64, 1);
    let nx = b.ins().iadd(xv2, one);
    b.def_var(x_var, nx);
    b.ins().jump(header, &[]);

    b.seal_block(header);

    b.switch_to_block(exit_blk);
    b.seal_block(exit_blk);
    let result = b.use_var(acc_var);
    b.ins().return_(&[result]);

    b.finalize();
    module.define_function(fid, ctx).ok()?;
    module.clear_context(ctx);
    Some(())
}

fn gen_value<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
    kind: NumKind,
) -> ClValue {
    match e {
        Expr::Int(i) => match kind {
            NumKind::Int => b.ins().iconst(I64, *i),
            NumKind::Float => b.ins().f64const(*i as f64),
        },
        Expr::Float(f) => b.ins().f64const(*f),
        Expr::Ident { name, .. } => {
            let var = vars[name.as_str()];
            b.use_var(var)
        }
        Expr::Binary { op, left, right, .. } => {
            let l = gen_value(b, left, vars, fn_ids, module, kind);
            let r = gen_value(b, right, vars, fn_ids, module, kind);
            match (kind, op) {
                (NumKind::Int, BinOp::Add) => b.ins().iadd(l, r),
                (NumKind::Int, BinOp::Sub) => b.ins().isub(l, r),
                (NumKind::Int, BinOp::Mul) => b.ins().imul(l, r),
                (NumKind::Int, BinOp::Mod) => {
                    // `a.rem_euclid(b)` for a positive constant `b` (guaranteed by
                    // `value_eligible`): `r = a % b; if r < 0 { r + b } else { r }`.
                    // `b > 0` also avoids the `srem(i64::MIN, -1)` overflow trap.
                    let rem = b.ins().srem(l, r);
                    let zero = b.ins().iconst(I64, 0);
                    let fixed = b.ins().iadd(rem, r);
                    let is_neg = b.ins().icmp(IntCC::SignedLessThan, rem, zero);
                    b.ins().select(is_neg, fixed, rem)
                }
                (NumKind::Float, BinOp::Add) => b.ins().fadd(l, r),
                (NumKind::Float, BinOp::Sub) => b.ins().fsub(l, r),
                (NumKind::Float, BinOp::Mul) => b.ins().fmul(l, r),
                // `Div` is deliberately not eligible (see `value_eligible`): native
                // `fdiv` returns inf where the interpreter errors on /0.
                _ => unreachable!("ineligible operator reached codegen"),
            }
        }
        Expr::Call { name, args, .. } => {
            let fid = fn_ids[name.as_str()];
            let fref = module.declare_func_in_func(fid, b.func);
            let argv: Vec<ClValue> = args
                .iter()
                .map(|a| gen_value(b, a, vars, fn_ids, module, kind))
                .collect();
            let call = b.ins().call(fref, &argv);
            b.inst_results(call)[0]
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            let then_b = b.create_block();
            let else_b = b.create_block();
            let merge_b = b.create_block();

            let cv = gen_cond(b, cond, vars, fn_ids, module, kind);
            b.ins().brif(cv, then_b, &[], else_b, &[]);

            let rvar = b.declare_var(kind.cl_type());

            b.switch_to_block(then_b);
            b.seal_block(then_b);
            let tv = gen_value(b, then_branch, vars, fn_ids, module, kind);
            b.def_var(rvar, tv);
            b.ins().jump(merge_b, &[]);

            b.switch_to_block(else_b);
            b.seal_block(else_b);
            let ev = gen_value(b, else_branch, vars, fn_ids, module, kind);
            b.def_var(rvar, ev);
            b.ins().jump(merge_b, &[]);

            b.switch_to_block(merge_b);
            b.seal_block(merge_b);
            b.use_var(rvar)
        }
        Expr::Let { bindings, body } => {
            let mut saved: Vec<(&'a str, Option<Variable>)> = Vec::new();
            for (n, v) in bindings {
                let vv = gen_value(b, v, vars, fn_ids, module, kind);
                let var = b.declare_var(kind.cl_type());
                b.def_var(var, vv);
                saved.push((n.as_str(), vars.insert(n.as_str(), var)));
            }
            let r = gen_value(b, body, vars, fn_ids, module, kind);
            for (n, old) in saved.into_iter().rev() {
                match old {
                    Some(o) => {
                        vars.insert(n, o);
                    }
                    None => {
                        vars.remove(n);
                    }
                }
            }
            r
        }
        _ => unreachable!("ineligible node reached codegen"),
    }
}

fn gen_cond<'a>(
    b: &mut FunctionBuilder,
    e: &'a Expr,
    vars: &mut HashMap<&'a str, Variable>,
    fn_ids: &HashMap<&str, FuncId>,
    module: &mut JITModule,
    kind: NumKind,
) -> ClValue {
    match e {
        Expr::Binary { op, left, right, .. } => {
            let l = gen_value(b, left, vars, fn_ids, module, kind);
            let r = gen_value(b, right, vars, fn_ids, module, kind);
            match kind {
                NumKind::Int => {
                    let cc = match op {
                        BinOp::Lt => IntCC::SignedLessThan,
                        BinOp::Gt => IntCC::SignedGreaterThan,
                        BinOp::Le => IntCC::SignedLessThanOrEqual,
                        BinOp::Ge => IntCC::SignedGreaterThanOrEqual,
                        BinOp::Eq => IntCC::Equal,
                        BinOp::Ne => IntCC::NotEqual,
                        _ => unreachable!("only comparisons reach cond codegen"),
                    };
                    b.ins().icmp(cc, l, r)
                }
                NumKind::Float => {
                    let cc = match op {
                        BinOp::Lt => FloatCC::LessThan,
                        BinOp::Gt => FloatCC::GreaterThan,
                        BinOp::Le => FloatCC::LessThanOrEqual,
                        BinOp::Ge => FloatCC::GreaterThanOrEqual,
                        BinOp::Eq => FloatCC::Equal,
                        BinOp::Ne => FloatCC::NotEqual,
                        _ => unreachable!("only comparisons reach cond codegen"),
                    };
                    b.ins().fcmp(cc, l, r)
                }
            }
        }
        _ => unreachable!("ineligible condition reached codegen"),
    }
}

/// Call an `i64`-specialized JIT function. SAFETY: see module docs; the VM
/// guarantees `ptr` is a finalized `extern "C" fn(i64×n)->i64` and `args.len()==n`.
pub unsafe fn call_i64(ptr: *const u8, args: &[i64]) -> i64 {
    unsafe {
        match args.len() {
            0 => std::mem::transmute::<*const u8, extern "C" fn() -> i64>(ptr)(),
            1 => std::mem::transmute::<*const u8, extern "C" fn(i64) -> i64>(ptr)(args[0]),
            2 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64) -> i64>(ptr)(args[0], args[1]),
            3 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64) -> i64>(ptr)(
                args[0], args[1], args[2],
            ),
            4 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64, i64) -> i64>(ptr)(
                args[0], args[1], args[2], args[3],
            ),
            _ => unreachable!("JIT arity is capped at {MAX_ARITY}"),
        }
    }
}

/// Call a native reduce loop. SAFETY: the VM guarantees `ptr` is a finalized
/// `extern "C" fn(i64,i64,i64)->i64` produced by [`define_reduce_loop`].
pub unsafe fn call_reduce(ptr: *const u8, start: i64, end: i64, init: i64) -> i64 {
    unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64) -> i64>(ptr)(start, end, init)
    }
}

/// Call an `f64`-specialized JIT function. SAFETY: as [`call_i64`], with an
/// `extern "C" fn(f64×n)->f64` contract.
pub unsafe fn call_f64(ptr: *const u8, args: &[f64]) -> f64 {
    unsafe {
        match args.len() {
            0 => std::mem::transmute::<*const u8, extern "C" fn() -> f64>(ptr)(),
            1 => std::mem::transmute::<*const u8, extern "C" fn(f64) -> f64>(ptr)(args[0]),
            2 => std::mem::transmute::<*const u8, extern "C" fn(f64, f64) -> f64>(ptr)(args[0], args[1]),
            3 => std::mem::transmute::<*const u8, extern "C" fn(f64, f64, f64) -> f64>(ptr)(
                args[0], args[1], args[2],
            ),
            4 => std::mem::transmute::<*const u8, extern "C" fn(f64, f64, f64, f64) -> f64>(ptr)(
                args[0], args[1], args[2], args[3],
            ),
            _ => unreachable!("JIT arity is capped at {MAX_ARITY}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> Expr {
        Expr::Call { name: name.to_string(), args: vec![], line: 0, col: 0 }
    }

    // The parser's define-before-use rule means a mutual-recursion cycle can't be
    // *written* in Helix today, so these cases are constructed as raw ASTs. They
    // assert the JIT stays memory-safe independent of that front-end policy: a
    // function on a call cycle must never reach the unguarded native path.
    #[test]
    fn recursive_funcs_catches_mutual_recursion() {
        let p: Vec<(String, Option<TypeAnn>)> = vec![];
        let (fb, gb) = (call("g"), call("f")); // f -> g -> f
        let (leaf, caller) = (Expr::Int(0), call("leaf")); // caller -> leaf (acyclic)
        let funcs = vec![
            FnDef { name: "f", params: &p, body: &fb },
            FnDef { name: "g", params: &p, body: &gb },
            FnDef { name: "leaf", params: &p, body: &leaf },
            FnDef { name: "caller", params: &p, body: &caller },
        ];
        let rec = recursive_funcs(&funcs);
        assert!(rec.contains("f") && rec.contains("g"), "f->g->f cycle must be flagged");
        assert!(!rec.contains("leaf") && !rec.contains("caller"), "acyclic fns are not recursive");
        // ...and eligible_set must keep the cycle off the native path.
        let elig = eligible_set(&funcs, NumKind::Int);
        assert!(!elig.contains("f") && !elig.contains("g"));
    }

    #[test]
    fn recursive_funcs_catches_direct_self_recursion() {
        let p: Vec<(String, Option<TypeAnn>)> = vec![];
        let body = call("fac"); // fac -> fac
        let funcs = vec![FnDef { name: "fac", params: &p, body: &body }];
        assert!(recursive_funcs(&funcs).contains("fac"));
    }

    #[test]
    fn recursive_funcs_allows_acyclic_chain() {
        let p: Vec<(String, Option<TypeAnn>)> = vec![];
        let (ab, bb, cb) = (call("b"), call("c"), Expr::Int(0)); // a -> b -> c (leaf)
        let funcs = vec![
            FnDef { name: "a", params: &p, body: &ab },
            FnDef { name: "b", params: &p, body: &bb },
            FnDef { name: "c", params: &p, body: &cb },
        ];
        assert!(recursive_funcs(&funcs).is_empty(), "an acyclic call chain has no recursion");
    }
}

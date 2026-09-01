//! The JIT's eligibility analysis — the UNGATED half (ADR 0032). The bytecode
//! compiler consults these to decide which guard opcodes to emit, so they run
//! in every build: bytecode is identical with or without the `jit` feature;
//! only whether a native kernel answers the guard differs.
#![cfg_attr(not(feature = "jit"), allow(dead_code))]

use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, Expr, Stmt, TypeAnn, UnOp};
use crate::bytecode::{Capture, CaptureKind, IndexBound};

use super::{FnDef, NumKind, MAX_ARITY};

// ---------- eligibility ----------

/// True if `body` is a pure `i64` expression over `{pa, pb}` the JIT can compile
/// into a native reduce loop: integer literals, the two binders, `+ - *`,
/// comparisons inside an `if`, and `let` — but **no** floats, division, function
/// calls, or other free identifiers. The bytecode compiler calls this to decide
/// whether to emit a `TryJitReduce` guard, so it is the single source of truth for
/// reduce-loop eligibility (and is platform-independent — the native code is only
/// emitted by `build`, which is gated to x86-64 Linux).
/// The names of user functions the JIT can compile as pure `i64` natives — so a kernel
/// or reduce body may *call* them. Computed identically at bytecode-compile time (to
/// decide whether to emit a guard) and at JIT-build time (to compile the kernel), so the
/// two always agree. Platform-independent (only codegen is x86-64-gated).
pub fn int_eligible_fns(program: &[Stmt]) -> std::collections::HashSet<String> {
    let funcs: Vec<FnDef> = program
        .iter()
        .filter_map(|s| match s {
            Stmt::Func { name, params, body, .. } => Some(FnDef { name, params, body }),
            _ => None,
        })
        .collect();
    eligible_set(&funcs, NumKind::Int).into_iter().map(str::to_string).collect()
}

/// True if `body` is a pure `i64` value expression over `{pa, pb}` and calls only the
/// JIT-eligible functions in `fns` — what `define_reduce_loop`/`define_fused_kernel` can
/// lower. `fns` is empty for a self-contained body.
pub fn reduce_loop_eligible(body: &Expr, pa: &str, pb: &str, fns: &HashSet<&str>) -> bool {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(pa);
    locals.insert(pb);
    value_eligible(body, fns, &locals, NumKind::Int)
}

/// Like [`reduce_loop_eligible`], but a **scalar** body referencing free (captured)
/// variables is still eligible — each free variable is recorded (in first-appearance
/// order) as a [`Capture`] and passed to the kernel as a loop-invariant `caps[i]`. Two
/// capture shapes: a bare free `i64` variable (the nested-fold case: an inner
/// `range(..).reduce(..)` reading the outer `map` variable → [`CaptureKind::Scalar`]), and
/// a free array indexed by the loop counter `pb` (`arr[pb]`, the dot-product case →
/// [`CaptureKind::ArrayI64`]). Returns the ordered captures (possibly empty), or `None` if
/// the body is ineligible, captures more than [`MAX_CAPTURES`], or uses a name both bare
/// and indexed (a contradictory kind). Same i64-closed rules as `value_eligible(Int)`.
/// What an indexed collector returns: the ordered captures, the bounds obligations the VM
/// must discharge, and any synthetic `$aff` base/coef terms (expressions the compile site
/// evaluates once in the enclosing scope — a site that pushes bare idents only must
/// decline when this is non-empty).
pub type IndexedCaptures = (Vec<Capture>, Vec<IndexBound>, Vec<(String, Expr)>);

pub fn reduce_loop_captures(
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
) -> Option<IndexedCaptures> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(pa);
    locals.insert(pb);
    let mut caps: Vec<Capture> = Vec::new();
    let mut bounds: Vec<IndexBound> = Vec::new();
    // Synthetic `$aff` base/coef terms from affine indices (`a[2*i]`) — expressions the
    // compile site evaluates once in the enclosing scope. A site whose capture-push loop
    // cannot evaluate an expression (it pushes bare idents only) must DECLINE when this
    // is non-empty rather than push an unresolvable name.
    let mut synth: Vec<(String, Expr)> = Vec::new();
    if value_eligible_cap_indexed(body, fns, &locals, pb, &mut caps, &mut bounds, &mut synth)
        && caps.len() <= MAX_CAPTURES
    {
        Some((caps, bounds, synth))
    } else {
        None
    }
}

/// Record capture `name` with `kind` in first-appearance order, deduping, returning its slot
/// position — or `None` if `name` was already recorded with a *different* kind (a body that
/// reads `a` both bare and as `a[…]` is contradictory: scalar or array? → fall back rather than
/// guess). Positions are what [`IndexBound`] obligations reference, so codegen and the VM stay
/// driven by one unambiguous ordered list.
fn record_cap_pos(caps: &mut Vec<Capture>, name: &str, kind: CaptureKind) -> Option<usize> {
    if let Some(pos) = caps.iter().position(|c| c.name == name) {
        return if caps[pos].kind == kind { Some(pos) } else { None };
    }
    caps.push(Capture { name: name.to_string(), kind });
    Some(caps.len() - 1)
}

/// `record_cap_pos`, discarding the position — for the bare-scalar case that needs no bound.
fn record_cap(caps: &mut Vec<Capture>, name: &str, kind: CaptureKind) -> bool {
    record_cap_pos(caps, name, kind).is_some()
}

/// Append a bounds obligation, deduping (a repeated `arr[j]` needs only one range check).
fn push_bound(bounds: &mut Vec<IndexBound>, b: IndexBound) {
    if !bounds.contains(&b) {
        bounds.push(b);
    }
}

/// Relabel every `Scalar` cap that is used only as a VALUE (never as an index) to
/// [`CaptureKind::ScalarValue`] — a purely-VALUE scalar. Both map indexed analyses call this
/// before returning, so the i64 and mixed derivations of the same body produce byte-identical
/// capture lists and the dual-spec re-gate matches. A scalar that IS index-referenced stays
/// `Scalar`: an index is an integer, so it is `i64` in both specs even when the same name also
/// appears in a value position (`n` in both `a[i*n+k]` and `n * x[i]` is necessarily `Int`, so
/// `i64` is correct there too). Reduce captures never pass through here, so the reduce path
/// keeps `Scalar` and is untouched. Idempotent.
///
/// A scalar is index-referenced when a bound names it DIRECTLY (a `Scalar` index, or an
/// `Affine` `base`/`coef` that is a bare ident), OR when it appears inside a COMPOUND affine
/// term — `a` and `b` in `x[i + a + b]`, folded into a synthetic `$aff` slot. The affine
/// codegen recomputes the whole index (`i + a + b`) from the individual `a`/`b` caps, so those
/// too are index arithmetic and must stay `i64`; missing them let the mixed kernel type the
/// index in `f64` and emit ill-typed IR (the Cranelift verifier caught it and the kernel
/// silently declined — a perf cliff, not a divergence). So `synth`'s expressions are scanned
/// for cap names as well.
fn relabel_value_scalars(caps: &mut [Capture], bounds: &[IndexBound], synth: &[(String, Expr)]) {
    let mut index_ref = vec![false; caps.len()];
    for b in bounds {
        match *b {
            IndexBound::Scalar { scalar, .. } => index_ref[scalar as usize] = true,
            IndexBound::Affine { base, coef, .. } => {
                index_ref[base as usize] = true;
                index_ref[coef as usize] = true;
            }
            IndexBound::Counter { .. } => {}
        }
    }
    // A cap named inside any synthetic affine term (`$aff0 = a + b`) is part of the index.
    for i in 0..caps.len() {
        if caps[i].kind == CaptureKind::Scalar
            && synth.iter().any(|(_, e)| expr_uses_ident(e, &caps[i].name))
        {
            index_ref[i] = true;
        }
    }
    for (i, c) in caps.iter_mut().enumerate() {
        if c.kind == CaptureKind::Scalar && !index_ref[i] {
            c.kind = CaptureKind::ScalarValue;
        }
    }
}

/// Reduce-only twin of [`value_eligible_cap`] that additionally accepts `arr[pb]` — a free
/// array indexed by exactly the loop counter — recording it as a [`CaptureKind::ArrayI64`].
/// A bare free ident is a [`CaptureKind::Scalar`] cap (as before). `pb` is threaded so the
/// index shape can be checked. NOT shared with the map kernel (whose `value_eligible_cap`
/// still rejects `Index`), so array-indexing stays scoped to the reduce path until the map
/// variant lands. i64-closed subset — identical operator rules to `value_eligible_cap`.
fn value_eligible_cap_indexed(
    e: &Expr,
    eligible: &HashSet<&str>,
    locals: &HashSet<&str>,
    pb: &str,
    caps: &mut Vec<Capture>,
    bounds: &mut Vec<IndexBound>,
    synth: &mut Vec<(String, Expr)>,
) -> bool {
    match e {
        Expr::Int(_) => true,
        Expr::Float(_) => false,
        Expr::Ident { name, .. } => {
            if locals.contains(name.as_str()) {
                true
            } else {
                record_cap(caps, name, CaptureKind::Scalar)
            }
        }
        Expr::Index { recv, index, .. } => match (&**recv, &**index) {
            // `arr[pb]`: a free array read by exactly the loop counter → a Counter bound (the
            // VM range-checks `[start,end) ⊆ [0,len)`; the counter's values are exactly that).
            (Expr::Ident { name: arr, .. }, Expr::Ident { name: idx, .. })
                if !locals.contains(arr.as_str()) && idx == pb =>
            {
                match record_cap_pos(caps, arr, CaptureKind::ArrayI64) {
                    Some(ap) => {
                        push_bound(bounds, IndexBound::Counter { array: ap as u32 });
                        true
                    }
                    None => false,
                }
            }
            // `arr[i]`: a free array indexed by a free SCALAR capture (not the counter, not a
            // local) — the all-pairs shape (`codes[i]` with the outer binder `i`). Records `arr`
            // as an array cap and `i` as a scalar cap, and a Scalar (point) bound the VM checks
            // as `0 <= i < len(arr)`. `idx != arr` rules out `a[a]`.
            (Expr::Ident { name: arr, .. }, Expr::Ident { name: idx, .. })
                if !locals.contains(arr.as_str())
                    && !locals.contains(idx.as_str())
                    && idx != arr =>
            {
                let ap = match record_cap_pos(caps, arr, CaptureKind::ArrayI64) {
                    Some(p) => p,
                    None => return false,
                };
                let sp = match record_cap_pos(caps, idx, CaptureKind::Scalar) {
                    Some(p) => p,
                    None => return false,
                };
                push_bound(bounds, IndexBound::Scalar { array: ap as u32, scalar: sp as u32 });
                true
            }
            // `arr[AFFINE(pb)]`: any other index affine in the counter — `a[2*i]`,
            // `a[i + 1]`, `a[i*n + k]`. The same admission, by the same helpers, as the
            // mixed map's arm (see `infer_f64_indexed`): validate the WHOLE index first
            // as a pure `i64` expression over the counter, free scalars and `Int`
            // literals (codegen lowers exactly that expression from `vars`, so it must
            // be checked verbatim; every leaf effect-free and non-trapping, which is
            // what licenses `affine_split`'s algebraic folding), then split it into
            // counter-free `base`/`coef` terms that land as Scalar cap slots — bare
            // idents reuse the body's own caps, compound terms get a synthetic `$aff`
            // slot the compile site evaluates once. The VM discharges the bound from
            // the two ENDPOINT indices in i128 — over the range endpoints for a reduce
            // (whose `pb` IS the counter), and composed with the lazy range's
            // `start/step` for a map (`map_index_caps`), which declines any other
            // source. There is no `pa` here — the empty string, never a legal ident,
            // fills `index_scalars_eligible`'s reject slot; the REAL accumulator (and
            // every other local) is refused by the `locals` scan below, because a
            // loop-varying name in the index would make the once-evaluated base/coef
            // caps stale, and a `let`-local does not even exist in the enclosing scope
            // the compile site evaluates them in.
            (Expr::Ident { name: arr, .. }, idx) if !locals.contains(arr.as_str()) => {
                if locals.iter().any(|l| *l != pb && expr_uses_ident(idx, l)) {
                    return false;
                }
                let Some(ap) = record_cap_pos(caps, arr, CaptureKind::ArrayI64) else {
                    return false;
                };
                if index_scalars_eligible(idx, "", pb, caps).is_none() {
                    return false;
                }
                let Some((base, coef)) = affine_split(idx, pb) else {
                    return false;
                };
                let (Some(bp), Some(cp)) = (
                    record_index_term(caps, synth, base),
                    record_index_term(caps, synth, coef),
                ) else {
                    return false;
                };
                push_bound(
                    bounds,
                    IndexBound::Affine { array: ap as u32, base: bp as u32, coef: cp as u32 },
                );
                true
            }
            _ => false,
        },
        Expr::Binary { op, left, right, .. } => {
            let op_ok = match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => true,
                BinOp::Mod => matches!(**right, Expr::Int(n) if n > 0),
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => true,
                BinOp::Shl | BinOp::Shr => matches!(**right, Expr::Int(n) if (0..=63).contains(&n)),
                BinOp::FloorDiv => matches!(**right, Expr::Int(n) if n > 0),
                _ => false,
            };
            op_ok
                && value_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds, synth)
                && value_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds, synth)
        }
        Expr::Call { name, args, .. } => {
            eligible.contains(name.as_str())
                && jit_builtin_arity_ok(name, args.len())
                && args
                    .iter()
                    .all(|a| value_eligible_cap_indexed(a, eligible, locals, pb, caps, bounds, synth))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            cond_eligible_cap_indexed(cond, eligible, locals, pb, caps, bounds, synth)
                && value_eligible_cap_indexed(then_branch, eligible, locals, pb, caps, bounds, synth)
                && value_eligible_cap_indexed(else_branch, eligible, locals, pb, caps, bounds, synth)
        }
        Expr::Let { bindings, body, .. } => {
            let mut locals2 = locals.clone();
            for (n, v) in bindings {
                // A `let` that REBINDS the loop counter `pb` breaks the invariant the `Index`
                // arm relies on: `arr[pb]` no longer means `arr[counter]` — codegen would emit
                // an UNCHECKED load at the let-bound index, past what the VM's counter-range
                // pre-check validated → an out-of-bounds native read. It also can't shadow a
                // captured scalar index without changing what a `Scalar` bound refers to —
                // nor a name an `Affine` bound's `base`/`coef` slot refers to, for the same
                // reason: the bound was proved against the ENCLOSING-scope value, and codegen
                // would recompute the index from the let-bound one. (`$aff` slots cannot
                // collide — `$` is not a legal identifier character.) Refuse to JIT any such
                // `let`; the VM/tree-walker evaluate such a body correctly.
                if n.as_str() == pb
                    || bounds.iter().any(|b| {
                        let names_cap = |pos: u32| {
                            caps.get(pos as usize).is_some_and(|c| c.name == *n)
                        };
                        match b {
                            IndexBound::Scalar { scalar, .. } => names_cap(*scalar),
                            IndexBound::Affine { base, coef, .. } => {
                                names_cap(*base) || names_cap(*coef)
                            }
                            IndexBound::Counter { .. } => false,
                        }
                    })
                {
                    return false;
                }
                if !value_eligible_cap_indexed(v, eligible, &locals2, pb, caps, bounds, synth) {
                    return false;
                }
                locals2.insert(n.as_str());
            }
            value_eligible_cap_indexed(body, eligible, &locals2, pb, caps, bounds, synth)
        }
        _ => false,
    }
}

/// Condition twin of [`value_eligible_cap_indexed`] — comparisons/`and`/`or` whose operands
/// may index a captured array by the loop counter.
fn cond_eligible_cap_indexed(
    e: &Expr,
    eligible: &HashSet<&str>,
    locals: &HashSet<&str>,
    pb: &str,
    caps: &mut Vec<Capture>,
    bounds: &mut Vec<IndexBound>,
    synth: &mut Vec<(String, Expr)>,
) -> bool {
    match e {
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds, synth)
                && cond_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds, synth)
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne)
                && value_eligible_cap_indexed(left, eligible, locals, pb, caps, bounds, synth)
                && value_eligible_cap_indexed(right, eligible, locals, pb, caps, bounds, synth)
        }
        _ => false,
    }
}

/// Is `body` a pure-`f64` reduce body over exactly `{pa, pb}` (accumulator and element)?
/// The same safe subset as the f64 map kernel — `+ - *`, the inline float builtins
/// (`sqrt`/`abs`/`min`/`max`), int/float literals — but BOTH binders are allowed and NO
/// free (captured) variable is (a capture's runtime type is unknown, so it can't be folded
/// as `f64`). `.reduce` is naive left-to-right, so the kernel's `fadd`/`fmul` in this order
/// is bit-exact to the interpreter (the property `differential_float_reduce_oracle` locks).
pub(crate) fn float_reduce_body_eligible<'e>(
    e: &'e Expr,
    pa: &str,
    pb: &str,
    locals: &HashSet<&'e str>,
    user_fns: &HashSet<&str>,
) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) => true,
        Expr::Ident { name, .. } => {
            name == pa || name == pb || locals.contains(name.as_str())
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                && float_reduce_body_eligible(left, pa, pb, locals, user_fns)
                && float_reduce_body_eligible(right, pa, pb, locals, user_fns)
        }
        Expr::Call { name, args, .. } => {
            jit_float_builtin_arity(name) == Some(args.len())
                && !user_fns.contains(name.as_str())
                && args.iter().all(|a| float_reduce_body_eligible(a, pa, pb, locals, user_fns))
        }
        // A `let` binds one more local per binding — scoped exactly as the walker
        // scopes it, and as `gen_f64_typed`'s `Let` arm scopes the Cranelift variable.
        // Rebinding the accumulator or the counter declines: those names are the
        // kernel's wiring, and a decline costs only the fast path (ADR 0029: slow-but-
        // correct is acceptable, wrong is not). This closes the field's ~19-23× trap:
        // `let d = xs[j] - t[j] in acc + d*d` fell to the interpreter while the
        // written-twice spelling compiled.
        Expr::Let { bindings, body, .. } => {
            let mut inner = locals.clone();
            for (n, v) in bindings {
                if n == pa || n == pb {
                    return false;
                }
                if !float_reduce_body_eligible(v, pa, pb, &inner, user_fns) {
                    return false;
                }
                inner.insert(n.as_str());
            }
            float_reduce_body_eligible(body, pa, pb, &inner, user_fns)
        }
        _ => false,
    }
}

/// Decide whether `reduce(init, (pa, pb) => body)` can JIT as a **scalar `f64`** fold — a
/// `Float`-literal init (so the accumulator is `f64`) and a pure-`f64` body over `{pa, pb}`.
/// Returns the body, or `None`. (The source must be a `Float` array; the VM checks that at
/// dispatch and falls back otherwise.)
pub fn reduce_jit_f64_body(init: &Expr, body: &Expr, pa: &str, pb: &str, user_fns: &HashSet<&str>) -> Option<Expr> {
    if init_admits_scalar_f64(init)
        && float_reduce_body_eligible(body, pa, pb, &HashSet::new(), user_fns)
    {
        Some(body.clone())
    } else {
        None
    }
}

/// Bottom-up kind of a **mixed f64-range-reduce** body node: `pa` (the accumulator) is
/// `f64`, `pb` (the `i64` range counter) is `i64`. `None` if the node falls outside the
/// eligible shape. Mirrors [`gen_reduce_f64_mixed`] and the interpreter's `arith` exactly —
/// `+ - *` (Int OP Int stays `i64`/wrapping, mixed → `f64`), `sqrt`→Float, `abs` preserves
/// kind, `min`/`max` require both args the SAME kind (a mixed `min(float,int)` returns
/// whichever original operand wins → runtime-dependent type → rejected). No captures.
fn infer_reduce_f64_kind<'e>(
    e: &'e Expr,
    pa: &str,
    pb: &str,
    locals: &HashMap<&'e str, NumKind>,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        // A USER function call — ONE arm for both specializations, exactly as
        // `infer_mixed_kind` does for the map. Splitting it into an `fns`-guarded arm and an
        // `msigs`-guarded arm below is what cost 66x there: `fns` means "i64-closed BODY", not
        // "Int parameters", so a callee can be in BOTH sets, the i64 arm claims the call site
        // by name, and Rust match arms cannot fall through to the mixed one. Merged here from
        // the start rather than repeating that.
        //
        // All-`Int` arguments to an i64-closed callee take the i64 path — that is the contract
        // its specialization was compiled under, so the result is an `i64` and types `Int`, and
        // the body promotes it at the first float precisely where the interpreter does.
        // Otherwise the MIXED specialization applies, and only when the argument kinds EQUAL
        // its parameter kinds: the callee was compiled for exactly those, and there is no
        // promoting at the boundary.
        //
        // The mixed callee's ABI carries a poison pointer, so a `/0` or NaN compare inside it
        // bails the whole reduce. `body_raises` already counts a mixed call for exactly this
        // reason, and `ReduceLoop::raises` carries that answer to both the kernel builder and
        // the VM — which is what makes admitting this arm safe rather than a way to swallow
        // the callee's error.
        Expr::Call { name, args, .. } if user_fns.contains(name.as_str()) => {
            // Typed exactly once, into a `Vec`, before anything is decided.
            let mut kinds = Vec::with_capacity(args.len());
            for a in args {
                kinds.push(infer_reduce_f64_kind(a, pa, pb, locals, fns, user_fns, msigs)?);
            }
            if kinds.iter().all(|k| *k == NumKind::Int) && fns.contains(name.as_str()) {
                if !jit_builtin_arity_ok(name, args.len()) {
                    return None;
                }
                return Some(NumKind::Int);
            }
            let (params, ret) = msigs.get(name.as_str())?;
            if kinds.len() != params.len() || kinds.iter().zip(params).any(|(k, w)| k != w) {
                return None;
            }
            Some(*ret)
        }
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Ident { name, .. } => {
            if name == pa {
                Some(NumKind::Float) // the f64 accumulator
            } else if name == pb {
                Some(NumKind::Int) // the i64 range counter
            } else {
                // A `let` local answers the kind its init inferred; otherwise None —
                // captures are excluded, a free var's runtime type is unknown.
                locals.get(name.as_str()).copied()
            }
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_reduce_f64_kind(left, pa, pb, locals, fns, user_fns, msigs)?;
            let rk = infer_reduce_f64_kind(right, pa, pb, locals, fns, user_fns, msigs)?;
            Some(if lk == NumKind::Float || rk == NumKind::Float {
                NumKind::Float
            } else {
                NumKind::Int
            })
        }
        // `/` is ALWAYS float division in Helix (even `Int / Int`), matching the interpreter's
        // `Div`. Both operands must be eligible; the result is `f64`. The interpreter RAISES on a
        // zero divisor while native `fdiv` yields inf/nan — so this only JITs under the caller's
        // `min`/`max` exclusion (see `f64_range_body_eligible`) + the VM's `is_finite` guard, which
        // together make a division-by-zero fall back to the exact-erroring bytecode loop.
        Expr::Binary { op: BinOp::Div, left, right, .. } => {
            infer_reduce_f64_kind(left, pa, pb, locals, fns, user_fns, msigs)?;
            infer_reduce_f64_kind(right, pa, pb, locals, fns, user_fns, msigs)?;
            Some(NumKind::Float)
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_reduce_f64_kind(&args[0], pa, pb, locals, fns, user_fns, msigs)?;
                    Some(NumKind::Float)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_reduce_f64_kind(&args[0], pa, pb, locals, fns, user_fns, msigs)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_reduce_f64_kind(&args[0], pa, pb, locals, fns, user_fns, msigs),
                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                ("to_int" | "sign", 1) => {
                    infer_reduce_f64_kind(&args[0], pa, pb, locals, fns, user_fns, msigs)?;
                    Some(NumKind::Int)
                }
                ("min" | "max", 2) => {
                    let ka = infer_reduce_f64_kind(&args[0], pa, pb, locals, fns, user_fns, msigs)?;
                    let kb = infer_reduce_f64_kind(&args[1], pa, pb, locals, fns, user_fns, msigs)?;
                    if ka == kb { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        // A `let` scope — sequential bindings, each typed by its init and visible to
        // the ones after it (the walker's semantics); rebinding `pa`/`pb` declines.
        Expr::Let { bindings, body, .. } => {
            let mut inner = locals.clone();
            for (n, v) in bindings {
                if n == pa || n == pb {
                    return None;
                }
                let k = infer_reduce_f64_kind(v, pa, pb, &inner, fns, user_fns, msigs)?;
                inner.insert(n.as_str(), k);
            }
            infer_reduce_f64_kind(body, pa, pb, &inner, fns, user_fns, msigs)
        }
        _ => None,
    }
}

/// Decide whether `range(..).reduce(init, (pa, pb) => body)` can JIT as a **scalar `f64`**
/// fold over the `i64` range counter: a `Float`-literal init and a mixed body whose inferred
/// root type is `Float` (so the result stores into the `f64` accumulator). Capture-free.
/// Returns the body, or `None`. (Unlike the array f64 reduce — where the element is itself
/// `f64` — here `pb` is the `i64` counter, so the body is lowered per-node, not pure-`f64`.)
pub fn reduce_jit_f64_range_body(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<Expr> {
    if init_admits_scalar_f64(init) && f64_range_body_eligible(body, pa, pb, fns, user_fns, msigs) {
        Some(body.clone())
    } else {
        None
    }
}

/// Whether a scalar `f64` range-reduce body is JIT-eligible: root type `Float` (per
/// [`infer_reduce_f64_kind`], which now admits `/`). No restriction on `min`/`max` or nested
/// division is needed — the codegen threads a **poison flag** that records a zero divisor at the
/// division site itself (see [`gen_f64_typed`]), so the VM falls back on the exact `/0` the
/// interpreter raises on, regardless of whether a later op or iteration would "rescue" the inf.
/// Shared by the compile gate and the build re-gate so the two never drift.
fn f64_range_body_eligible(
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> bool {
    infer_reduce_f64_kind(body, pa, pb, &HashMap::new(), fns, user_fns, msigs)
        == Some(NumKind::Float)
}

// `expr_has_div` / `reduce_body_divides` lived here. Both are gone: the poison decision is now
// `ReduceLoop::raises`, set once at compile time by `body_raises` (the predicate the map side
// already used) and READ by both the kernel builder and the VM. `expr_has_div` could not have
// been widened in place anyway — its `Call` arm recursed into a call's ARGUMENTS but never into
// the callee's BODY, so `fn f(x) = 1.0 / x` used as `acc + f(i)` reported no division at all.

/// Whether `e` reads the identifier `name` anywhere. Used to prove a multi-accumulator `term` is
/// FREE of the accumulator. Literals plainly reference nothing; the arithmetic/index/call nodes
/// recurse; and CRUCIALLY any OTHER node shape (`let`, `if`, `match`, …) is conservatively assumed
/// to reference `name` — so a term built from an unrecognised shape declines the multi-acc transform
/// (`_ => true`). Under-approximating here (returning `false` for a node that DOES use the
/// accumulator, as a bare `_ => false` did for `let … in acc`) would wrongly enable multi-acc and
/// then panic in codegen (the accumulator is intentionally absent from the partials' `vars`).
pub(crate) fn expr_uses_ident(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Ident { name: n, .. } => n == name,
        Expr::Int(_) | Expr::Float(_) => false,
        Expr::Binary { left, right, .. } => expr_uses_ident(left, name) || expr_uses_ident(right, name),
        Expr::Unary { expr, .. } => expr_uses_ident(expr, name),
        Expr::Index { recv, index, .. } => expr_uses_ident(recv, name) || expr_uses_ident(index, name),
        Expr::Call { args, .. } => args.iter().any(|a| expr_uses_ident(a, name)),
        _ => true,
    }
}

/// The per-element `term` of a **multi-accumulator-eligible i64 SUM reduce**, or `None`. Eligible
/// when the scalar body is `acc + term` (or `term + acc`) — a top-level `+` with the accumulator
/// binder `pa` as EXACTLY one operand and `term` (the other operand) FREE of `pa`. The fold is then
/// a plain associative sum `init + Σ term(pb)`, which K independent partial accumulators compute
/// BIT-IDENTICALLY (integer add is associative + commutative) while breaking the single-accumulator
/// latency-bound dependency chain (~2.3× per core). i64 ONLY — f64 reassociation changes rounding
/// (non-associative), so a float reduce is never eligible.
pub(crate) fn reduce_multiacc_term(rl: &crate::bytecode::ReduceLoop) -> Option<&Expr> {
    if rl.float || rl.bodies.len() != 1 {
        return None;
    }
    let pa = rl.pa.as_str();
    if let Expr::Binary { op: BinOp::Add, left, right, .. } = &rl.bodies[0] {
        let l_acc = matches!(&**left, Expr::Ident { name, .. } if name == pa);
        let r_acc = matches!(&**right, Expr::Ident { name, .. } if name == pa);
        if l_acc && !r_acc && !expr_uses_ident(right, pa) {
            return Some(right);
        }
        if r_acc && !l_acc && !expr_uses_ident(left, pa) {
            return Some(left);
        }
    }
    None
}

/// Bottom-up [`MixT`] of a **scalar f64 reduce body that indexes captured `f64` arrays by the
/// loop counter** (the float dot-product / weighted-sum / SAXPY-sum case): `pa` is the `f64`
/// accumulator, `pb` the `i64` counter, and `arr[index]` for a free array `arr` is an `f64`
/// element → records `arr` as a [`CaptureKind::ArrayF64`] capture (first-appearance order).
///
/// A bare free var is a VALUE SCALAR — the coefficient `c` in `s + c * a[i]`. It rides as `f64`
/// in this kernel (which is monomorphically `f64`: a `Float` init picked it), so unlike the map
/// case there is no representation routing to do. What DOES carry over is the bit-identity rule:
/// the codegen evaluates integer subexpressions in `i64` and promotes at the first float, exactly
/// like the interpreter, so a value scalar — `f64` in the kernel but possibly `Int` at runtime —
/// is admitted ONLY where a genuine float ([`MixT::GFloat`]: the accumulator, an array load, a
/// float literal) promotes it. `c * a[i]` is safe; `c * pb` or `c + d` would be `i64` in the
/// interpreter and `f64` here, diverging past 2^53, so they are rejected. See [`MixT`].
///
/// The VM pre-checks each array's bounds before the kernel does raw `f64` loads. `None` outside
/// the eligible shape.
/// The three parallel OUTPUTS of an indexed analysis. They are always constructed together,
/// passed together, and consumed together — bundling them keeps the walker's signature at a
/// readable width now that it also needs the eligible-function set.
#[derive(Default)]
struct IndexedOut {
    caps: Vec<Capture>,
    synth: Vec<(String, Expr)>,
    bounds: Vec<IndexBound>,
}

fn infer_f64_indexed<'e>(
    e: &'e Expr,
    pa: &str,
    pb: &str,
    locals: &HashMap<&'e str, MixT>,
    out: &mut IndexedOut,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
) -> Option<MixT> {
    match e {
        Expr::Int(_) => Some(MixT::Int),
        // A USER function with an `i64` specialization, typed exactly as the mixed map's twin
        // arm does (Stage 3p): `int_eligible` means "i64-closed for all-`Int` arguments", so
        // such a call takes `Int` args and returns `Int`, and `mix_combine` then promotes the
        // result at the first genuine float precisely where the interpreter does. Tried BEFORE
        // the builtin arm so a user function shadowing `abs`/`min`/`max` dispatches to the
        // user's function. An `SFloat` argument is refused for the same reason `abs` refuses
        // one — its runtime type is not pinned, and the callee would read it directly.
        Expr::Call { name, args, .. }
            if fns.contains(name.as_str()) && user_fns.contains(name.as_str()) =>
        {
            if !jit_builtin_arity_ok(name, args.len()) {
                return None;
            }
            for a in args {
                if infer_f64_indexed(a, pa, pb, locals, out, fns, user_fns)? != MixT::Int {
                    return None;
                }
            }
            Some(MixT::Int)
        }
        Expr::Float(_) => Some(MixT::GFloat),
        Expr::Ident { name, .. } => {
            if name == pa {
                Some(MixT::GFloat) // the f64 accumulator register — a genuine float
            } else if name == pb {
                Some(MixT::Int)
            } else if let Some(k) = locals.get(name.as_str()) {
                Some(*k) // a `let` local: the MixT its init inferred
            } else {
                // A free VALUE scalar, loaded `f64` by the kernel. Recorded `Scalar` here and
                // relabeled to `ScalarValue` by the caller once the bounds show it is not an
                // index (an index scalar must stay `i64`).
                record_cap(&mut out.caps, name, CaptureKind::Scalar).then_some(MixT::SFloat)
            }
        }
        // A free `f64` array read at an index that is AFFINE in the counter → an `f64` element.
        // `arr[pb]` (v1b) keeps its cheap `Counter` bound; any other affine index
        // (`a[i*n+k]`, `b[k*n+j]`, `a[k+1]`) records an `Affine` bound instead.
        Expr::Index { recv, index, .. } => {
            // Inside a `let` scope, an index that mentions ANY local declines: the
            // bounds machinery pre-evaluates base/coef caps in the ENCLOSING scope,
            // where no local exists — the same argument as the i64 path's guard.
            // (`expr_uses_ident` is conservative: unknown shapes report "uses".)
            if !locals.is_empty()
                && locals
                    .keys()
                    .any(|l| expr_uses_ident(recv, l) || expr_uses_ident(index, l))
            {
                return None;
            }
            let arr = match &**recv {
                Expr::Ident { name, .. } if name != pa && name != pb => name,
                _ => return None,
            };
            let ap = record_cap_pos(&mut out.caps, arr, CaptureKind::ArrayF64)?;
            match &**index {
                // The bare counter: exactly v1b's shape, exactly v1b's obligation.
                Expr::Ident { name: idx, .. } if idx == pb => {
                    push_bound(&mut out.bounds, IndexBound::Counter { array: ap as u32 });
                }
                _ => {
                    // Validate the WHOLE index first — a pure `i64` expression over the counter,
                    // free scalars (recorded as caps) and `Int` literals. This is what codegen
                    // lowers verbatim, so it must be checked verbatim; it also makes every leaf
                    // effect-free and non-trapping, which is what licenses `affine_split`'s
                    // algebraic folding to DISCARD subterms (`0 * x → 0`) without losing a raise.
                    index_scalars_eligible(index, pa, pb, &mut out.caps)?;
                    let (base, coef) = affine_split(index, pb)?;
                    let bp = record_index_term(&mut out.caps, &mut out.synth, base)?;
                    let cp = record_index_term(&mut out.caps, &mut out.synth, coef)?;
                    push_bound(
                        &mut out.bounds,
                        IndexBound::Affine { array: ap as u32, base: bp as u32, coef: cp as u32 },
                    );
                }
            }
            Some(MixT::GFloat) // an f64 array load is a genuine float
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_f64_indexed(left, pa, pb, locals, out, fns, user_fns)?;
            let rk = infer_f64_indexed(right, pa, pb, locals, out, fns, user_fns)?;
            mix_combine(lk, rk)
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                // `sqrt` promotes its argument in BOTH engines → an `SFloat` arg is safe.
                ("sqrt", 1) => {
                    infer_f64_indexed(&args[0], pa, pb, locals, out, fns, user_fns)?;
                    Some(MixT::GFloat)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_f64_indexed(&args[0], pa, pb, locals, out, fns, user_fns)?;
                    Some(MixT::GFloat)
                }
                // `abs`/`min`/`max` do NOT promote (interp `abs(Int)` is `iabs`), so an `SFloat`
                // argument would diverge; admit only genuine floats or ints, preserving the kind.
                                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                // An unpromoted value scalar is refused for the same reason `abs` refuses one:
                // its runtime type is not yet pinned, and `to_int`/`sign` read it directly.
                ("to_int" | "sign", 1) => match infer_f64_indexed(&args[0], pa, pb, locals, out, fns, user_fns)? {
                    MixT::SFloat => None,
                    _ => Some(MixT::Int),
                },
("abs", 1) => match infer_f64_indexed(&args[0], pa, pb, locals, out, fns, user_fns)? {
                    MixT::SFloat => None,
                    k => Some(k),
                },
                ("min" | "max", 2) => {
                    let ka = infer_f64_indexed(&args[0], pa, pb, locals, out, fns, user_fns)?;
                    let kb = infer_f64_indexed(&args[1], pa, pb, locals, out, fns, user_fns)?;
                    if ka == kb && ka != MixT::SFloat { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        // A `let` scope — sequential bindings, each carrying its init's MixT (an
        // SFloat-typed local keeps SFloat's refusal rules at its uses); rebinding
        // `pa`/`pb` declines. Locals never record captures: the `Ident` arm checks
        // them first, and the `Index` arm refuses any index that mentions one.
        Expr::Let { bindings, body, .. } => {
            let mut inner = locals.clone();
            for (n, v) in bindings {
                if n == pa || n == pb {
                    return None;
                }
                let k = infer_f64_indexed(v, pa, pb, &inner, out, fns, user_fns)?;
                inner.insert(n.as_str(), k);
            }
            infer_f64_indexed(body, pa, pb, &inner, out, fns, user_fns)
        }
        _ => None,
    }
}

/// Split an index expression into `(base, coef)` with `index ≡ base + coef*pb` and both parts
/// FREE of the counter `pb` — the algebraic core of [`IndexBound::Affine`]. Only shapes whose
/// linearity is provable by construction are admitted: a counter-free subtree is a pure base;
/// the counter itself is `0 + 1*pb`; `+`/`-` combine componentwise; `*` is linear only when at
/// least one side is counter-free (`k*n` is affine, `k*k` is NOT — quadratic, so `None`).
/// Everything else (`%`, `/`, calls, `if`, indexes) → `None`. Distributing a counter-free factor
/// over both components is exact under wrapping i64: `c*(b + a*pb) = c*b + (c*a)*pb` holds mod
/// 2^64 because multiplication distributes over addition in the ring Z/2^64.
fn affine_split(e: &Expr, pb: &str) -> Option<(Expr, Expr)> {
    fn zero() -> Expr {
        Expr::Int(0)
    }
    fn one() -> Expr {
        Expr::Int(1)
    }
    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary { op, left: Box::new(l), right: Box::new(r), line: 0, col: 0 }
    }
    fn lit(e: &Expr, v: i64) -> bool {
        matches!(e, Expr::Int(k) if *k == v)
    }
    // Identity/constant folding, in the ring Z/2^64 that Helix's `Int` arithmetic already is —
    // so these rewrites are exact, not approximations. This is not cosmetic: splitting `k*n+j`
    // yields `0*n+j` and `1*n+0`, and only folding turns those back into the bare `j` and `n`
    // that [`record_index_term`] can map onto the caps the body ALREADY holds. Without it every
    // affine index mints two fresh synthetic caps and a two-array body blows MAX_CAPTURES.
    // Discarding a factor under `0 * x` is safe because every leaf here is an ident or literal
    // (see the caller's `index_scalars_eligible` pre-check) — nothing to trap or observe.
    fn mk_add(l: Expr, r: Expr) -> Expr {
        match (&l, &r) {
            (Expr::Int(a), Expr::Int(b)) => Expr::Int(a.wrapping_add(*b)),
            _ if lit(&l, 0) => r,
            _ if lit(&r, 0) => l,
            _ => bin(BinOp::Add, l, r),
        }
    }
    fn mk_sub(l: Expr, r: Expr) -> Expr {
        match (&l, &r) {
            (Expr::Int(a), Expr::Int(b)) => Expr::Int(a.wrapping_sub(*b)),
            // `0 - x` is NEGATION, not `x` — only the right identity folds.
            _ if lit(&r, 0) => l,
            _ => bin(BinOp::Sub, l, r),
        }
    }
    fn mk_mul(l: Expr, r: Expr) -> Expr {
        match (&l, &r) {
            (Expr::Int(a), Expr::Int(b)) => Expr::Int(a.wrapping_mul(*b)),
            _ if lit(&l, 0) || lit(&r, 0) => Expr::Int(0),
            _ if lit(&l, 1) => r,
            _ if lit(&r, 1) => l,
            _ => bin(BinOp::Mul, l, r),
        }
    }
    if !expr_uses_ident(e, pb) {
        // Counter-free ⇒ a pure base. (`expr_uses_ident` is conservative: an unrecognised node
        // shape reports "uses", so it can never mis-classify an unknown node as invariant.)
        return Some((e.clone(), zero()));
    }
    match e {
        Expr::Ident { name, .. } if name == pb => Some((zero(), one())),
        Expr::Binary { op: BinOp::Add, left, right, .. } => {
            let (lb, lc) = affine_split(left, pb)?;
            let (rb, rc) = affine_split(right, pb)?;
            Some((mk_add(lb, rb), mk_add(lc, rc)))
        }
        Expr::Binary { op: BinOp::Sub, left, right, .. } => {
            let (lb, lc) = affine_split(left, pb)?;
            let (rb, rc) = affine_split(right, pb)?;
            Some((mk_sub(lb, rb), mk_sub(lc, rc)))
        }
        Expr::Binary { op: BinOp::Mul, left, right, .. } => {
            let l_free = !expr_uses_ident(left, pb);
            let r_free = !expr_uses_ident(right, pb);
            if l_free {
                let (rb, rc) = affine_split(right, pb)?;
                Some((mk_mul((**left).clone(), rb), mk_mul((**left).clone(), rc)))
            } else if r_free {
                let (lb, lc) = affine_split(left, pb)?;
                Some((mk_mul(lb, (**right).clone()), mk_mul(lc, (**right).clone())))
            } else {
                None // both sides vary with the counter → non-linear
            }
        }
        _ => None,
    }
}

/// Validate an index expression as a pure `i64` expression over the counter `pb`, free scalars,
/// and `Int` literals — recording each free scalar as a [`CaptureKind::Scalar`] cap, since codegen
/// lowers this very expression and needs every name it mentions bound. The accumulator `pa` is
/// `f64`, so an index reading it is rejected, as is any `Float` literal or operator outside
/// `+ - *` (the VM marshals a `Scalar` cap only from a `Value::Int`, so a non-integer capture
/// falls back at dispatch anyway).
fn index_scalars_eligible(e: &Expr, pa: &str, pb: &str, caps: &mut Vec<Capture>) -> Option<()> {
    match e {
        Expr::Int(_) => Some(()),
        Expr::Ident { name, .. } => {
            if name == pa {
                None
            } else if name == pb {
                Some(()) // the counter is a binder, not a capture
            } else {
                record_cap(caps, name, CaptureKind::Scalar).then_some(())
            }
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            index_scalars_eligible(left, pa, pb, caps)?;
            index_scalars_eligible(right, pa, pb, caps)
        }
        _ => None,
    }
}

/// Give an affine `base`/`coef` term a cap slot holding its VALUE, so the VM can range-check the
/// index arithmetically without interpreting an AST. A bare free ident already has one (reuse it
/// — `b[k*n+j]`'s base `j` and coef `n` are just the scalar caps the body already captured); any
/// compound term (`i*n`) gets a synthetic `$aff{k}` cap whose expression the compiler evaluates
/// once, in the enclosing scope, before dispatch. Synthetic terms are deduped by their printed
/// form, so the naming is a deterministic function of the body alone — which is what lets the
/// build re-gate re-derive an identical capture list from the same body.
fn record_index_term(
    caps: &mut Vec<Capture>,
    synth: &mut Vec<(String, Expr)>,
    term: Expr,
) -> Option<usize> {
    if let Expr::Ident { name, .. } = &term {
        return record_cap_pos(caps, name, CaptureKind::Scalar);
    }
    let key = format!("{term:?}");
    if let Some((name, _)) = synth.iter().find(|(_, e)| format!("{e:?}") == key) {
        let name = name.clone();
        return record_cap_pos(caps, &name, CaptureKind::Scalar);
    }
    let name = format!("$aff{}", synth.len());
    synth.push((name.clone(), term));
    record_cap_pos(caps, &name, CaptureKind::Scalar)
}

/// Decide whether `range(..).reduce(0.0, (pa, pb) => body)` can JIT as a **scalar `f64` fold
/// that indexes captured `f64` arrays by the counter** — the float dot-product. A `Float`-
/// literal init, a body whose root infers `Float`, and **at least one** `ArrayF64` capture
/// (so this never competes with the capture-free [`reduce_jit_f64_range_body`]). Returns the
/// body + the ordered captures, or `None`. (The VM confirms each capture is a `Floats` array
/// and pre-checks its bounds at dispatch, falling back otherwise.)
pub struct F64RangeCaptures {
    /// The body to lower.
    pub body: Expr,
    /// The ordered captures the VM marshals into the kernel's argument block.
    pub caps: Vec<Capture>,
    /// Index bounds the VM pre-checks at dispatch (empty on the f64 path, which
    /// range-checks its array caps inline).
    pub bounds: Vec<IndexBound>,
    /// Synthetic `$aff{k}` terms the compiler evaluates once in the enclosing scope.
    pub synth: Vec<(String, Expr)>,
}

pub fn reduce_jit_f64_range_captures(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
) -> Option<F64RangeCaptures> {
    if !init_admits_scalar_f64(init) {
        return None;
    }
    let mut out = IndexedOut::default();
    if infer_f64_indexed(body, pa, pb, &HashMap::new(), &mut out, fns, user_fns)
        == Some(MixT::GFloat)
        // Non-empty, NOT "contains an array". Requiring an array capture kept this path from
        // competing with the capture-free `reduce_jit_f64_range_body` — but it also meant a body
        // whose only capture is a SCALAR matched neither: `s + to_float(i) * c` fell to the
        // bytecode loop while `s + to_float(i) * 0.5` ran natively, 0.78s against 0.01s over 10M
        // elements. An empty list still falls through to the capture-free path exactly as before,
        // so this only admits shapes that previously had no kernel at all.
        && !out.caps.is_empty()
        && out.caps.len() <= MAX_CAPTURES
    {
        // Value scalars (`c` in `s + c*a[i]`) become `ScalarValue`, loaded `f64` by the kernel;
        // INDEX scalars (an `a[k]` index, an affine `base`/`coef`, incl. names inside a
        // synthetic `$aff` term) stay `Scalar` — `i64`, since an index is an integer.
        relabel_value_scalars(&mut out.caps, &out.bounds, &out.synth);
        Some(F64RangeCaptures {
            body: body.clone(),
            caps: out.caps,
            bounds: out.bounds,
            synth: out.synth,
        })
    } else {
        None
    }
}

/// Bottom-up kind of a node in a **multi-binder f64** body, given each binder's kind in
/// `binders` (the `f64` accumulator slots `$acc0…` plus the element/counter `pb`), or `None`
/// if it falls outside the eligible shape. The N-binder generalization of
/// [`infer_reduce_f64_kind`] — same promotion rules; used for f64 tuple/record accumulators.
pub(crate) fn infer_f64_typed(e: &Expr, binders: &HashMap<&str, NumKind>, user_fns: &HashSet<&str>) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Ident { name, .. } => binders.get(name.as_str()).copied(), // None = unknown var (no captures)
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_f64_typed(left, binders, user_fns)?;
            let rk = infer_f64_typed(right, binders, user_fns)?;
            Some(if lk == NumKind::Float || rk == NumKind::Float {
                NumKind::Float
            } else {
                NumKind::Int
            })
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_f64_typed(&args[0], binders, user_fns)?;
                    Some(NumKind::Float)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_f64_typed(&args[0], binders, user_fns)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_f64_typed(&args[0], binders, user_fns),
                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                ("to_int" | "sign", 1) => {
                    infer_f64_typed(&args[0], binders, user_fns)?;
                    Some(NumKind::Int)
                }
                ("min" | "max", 2) => {
                    let ka = infer_f64_typed(&args[0], binders, user_fns)?;
                    let kb = infer_f64_typed(&args[1], binders, user_fns)?;
                    if ka == kb { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        _ => None,
    }
}


/// `true` if `init` makes the reduce accumulator `f64`: a `Float` literal (scalar), or a
/// `Tuple`/`Record` of all-`Float` literals (multi-slot). The compiler routes these to the
/// f64 reduce paths instead of the i64 ones (which key only off body shape).
pub fn is_float_acc_init(init: &Expr) -> bool {
    match init {
        Expr::Float(_) => true,
        Expr::Tuple(items) if items.len() >= 2 => items.iter().all(|e| matches!(e, Expr::Float(_))),
        Expr::Record(fields) if fields.len() >= 2 => {
            fields.iter().all(|(_, e)| matches!(e, Expr::Float(_)))
        }
        // A NON-LITERAL init routes to the float family too — see
        // [`init_admits_scalar_f64`]. If the body then fails the f64 analyses, no
        // kernel is stored, exactly as before; if it passes but the runtime init is
        // not a `Float`, the dispatch falls back — so an unknown init can only ever
        // GAIN a kernel, never lose one (it had none: every gate required a literal).
        other => init_admits_scalar_f64(other),
    }
}

/// Whether a reduce INIT may inhabit the scalar `f64` accumulator ABI. A `Float`
/// literal proves it; a NON-LITERAL init — a parameter, a call, an ident, i.e. the
/// natural ODE-integrator spelling `reduce(a0, …)` — is admitted too, because the kind
/// check lives where it belongs: the DISPATCH reads the runtime init value and takes
/// the f64 kernel only for a `Value::Float`, falling back to the bytecode loop
/// otherwise (vm.rs, "a `Float` init confirms the f64 ABI"). The old literal-match was
/// a static type oracle standing in for that existing runtime check, and it silently
/// cost 21–53× on the natural spelling (the llm field report's finding: identical
/// body, identical answer, 59 ms vs 3,117 ms at 100M). Literals of another kind and
/// composite literals stay excluded so the int and tuple families keep their paths.
pub fn init_admits_scalar_f64(init: &Expr) -> bool {
    !matches!(
        init,
        Expr::Int(_) | Expr::Bool(_) | Expr::Str(_) | Expr::Tuple(_) | Expr::Record(_)
    )
}

/// A tuple accumulator may have at most this many `i64` slots; a wider one runs on the
/// bytecode loop. The reduce kernel keeps every slot in a register.
pub const MAX_ACC_SLOTS: usize = 4;

/// The synthetic slot identifiers (`$acc0…`), as `'static` strings so the codegen's
/// `vars` map (keyed by the kernel lifetime) can hold them without lifetime juggling.
/// `$` can't appear in user source, so they never collide. Length == `MAX_ACC_SLOTS`.
pub(crate) const ACC_IDENTS: [&str; MAX_ACC_SLOTS] = ["$acc0", "$acc1", "$acc2", "$acc3"];

/// The identifier bound to accumulator slot `k`. A tuple body's `pa[k]` is rewritten to
/// this so the existing `i64` codegen handles it unchanged.
pub fn acc_ident(k: usize) -> String {
    ACC_IDENTS[k].to_string()
}

/// Rewrite an accumulator slot access — `pa[k]` (tuple) or `pa.field` (record, mapped to
/// its position in `fields`) — to the slot ident `$acc{k}` throughout `e`. Only the
/// `i64`-eligible forms are recursed into; any other form is cloned as-is (so an
/// unsubstituted `pa[..]`/`pa.x` stays and fails eligibility — a safe fallback, never a
/// miscompile).
fn subst_acc(e: &Expr, pa: &str, n: usize, fields: &[String]) -> Expr {
    if let Expr::Index { recv, index, line, col } = e
        && let Expr::Ident { name, .. } = recv.as_ref()
        && name == pa
        && let Expr::Int(k) = index.as_ref()
        && *k >= 0
        && (*k as usize) < n
    {
        return Expr::Ident { name: acc_ident(*k as usize), line: *line, col: *col };
    }
    if let Expr::Field { recv, name, line, col } = e
        && let Expr::Ident { name: rn, .. } = recv.as_ref()
        && rn == pa
        && let Some(k) = fields.iter().position(|f| f == name)
    {
        return Expr::Ident { name: acc_ident(k), line: *line, col: *col };
    }
    let s = |c: &Expr| Box::new(subst_acc(c, pa, n, fields));
    match e {
        Expr::Binary { op, left, right, line, col } => Expr::Binary {
            op: *op,
            left: s(left),
            right: s(right),
            line: *line,
            col: *col,
        },
        Expr::Unary { op, expr, line, col } => Expr::Unary {
            op: *op,
            expr: s(expr),
            line: *line,
            col: *col,
        },
        Expr::Call { name, args, line, col } => Expr::Call {
            name: name.clone(),
            args: args.iter().map(|a| subst_acc(a, pa, n, fields)).collect(),
            line: *line,
            col: *col,
        },
        Expr::If { cond, then_branch, else_branch, line, col } => Expr::If {
            cond: s(cond),
            then_branch: s(then_branch),
            else_branch: s(else_branch),
            line: *line,
            col: *col,
        },
        Expr::Let { bindings, body, from_do } => Expr::Let {
            bindings: bindings
                .iter()
                .map(|(nm, v)| (nm.clone(), subst_acc(v, pa, n, fields)))
                .collect(),
            body: s(body),
            from_do: *from_do,
        },
        other => other.clone(),
    }
}

/// Replace every occurrence of the identifier `name` in `e` with `repl` — the substitution
/// behind map→reduce fusion (`g_body[pb := f_body[fb := $counter]]`, the classical
/// `map(f).reduce(init,g) ≡ reduce(init, (acc,i) => g(acc, f(i)))` identity).
///
/// DELIBERATELY CONSERVATIVE: only the pure-arithmetic node set that the f64 indexed reduce can
/// actually lower is handled, and **anything else returns `None`** so the caller declines the
/// whole fusion instead of emitting a body whose meaning it has not reasoned about. That rules
/// out every binding form (`let`, lambda, `match`) by construction, so there is no shadowing
/// case to get subtly wrong: a rebound `name` inside the substituted region cannot occur because
/// no construct that could rebind it is admitted here. `repl` is substituted structurally, so a
/// `name` occurring more than once duplicates it — safe, because every admitted node is pure and
/// deterministic (the same reason the reduce kernel may re-evaluate an index expression).
pub fn subst_ident(e: &Expr, name: &str, repl: &Expr) -> Option<Expr> {
    Some(match e {
        Expr::Int(_) | Expr::Float(_) => e.clone(),
        Expr::Ident { name: n, .. } => {
            if n == name {
                repl.clone()
            } else {
                e.clone()
            }
        }
        Expr::Unary { op, expr, line, col } => Expr::Unary {
            op: *op,
            expr: Box::new(subst_ident(expr, name, repl)?),
            line: *line,
            col: *col,
        },
        Expr::Binary { op, left, right, line, col } => Expr::Binary {
            op: *op,
            left: Box::new(subst_ident(left, name, repl)?),
            right: Box::new(subst_ident(right, name, repl)?),
            line: *line,
            col: *col,
        },
        Expr::Index { recv, index, line, col } => Expr::Index {
            recv: Box::new(subst_ident(recv, name, repl)?),
            index: Box::new(subst_ident(index, name, repl)?),
            line: *line,
            col: *col,
        },
        Expr::Call { name: f, args, line, col } => Expr::Call {
            name: f.clone(),
            args: args.iter().map(|a| subst_ident(a, name, repl)).collect::<Option<Vec<_>>>()?,
            line: *line,
            col: *col,
        },
        // Every other node — binding forms, strings, records, method calls, … — declines.
        _ => return None,
    })
}

/// Substitute the slot accesses in each component (already in slot order) and keep them
/// only if every one is `i64`-eligible over `{$acc0.., pb}`.
fn check_slot_bodies(
    comps: &[&Expr],
    pa: &str,
    pb: &str,
    fields: &[String],
    fns: &HashSet<&str>,
) -> Option<Vec<Expr>> {
    let n = comps.len();
    let names: Vec<String> = (0..n).map(acc_ident).collect();
    let mut locals: HashSet<&str> = HashSet::new();
    for nm in &names {
        locals.insert(nm.as_str());
    }
    locals.insert(pb);
    let bodies: Vec<Expr> = comps.iter().map(|c| subst_acc(c, pa, n, fields)).collect();
    bodies
        .iter()
        .all(|c| value_eligible(c, fns, &locals, NumKind::Int))
        .then_some(bodies)
}

/// Like [`check_slot_bodies`], but for an **all-`f64`** tuple/record accumulator: each
/// component is substituted (`pa[k]`/`pa.field` → `$acc{k}`) and kept only if it is
/// `f64`-eligible over `{$acc0…(Float), pb}` with root `Float`. `pb_kind` is `Int` for a
/// range counter, `Float` for a `Float`-array element.
fn check_slot_bodies_f64(
    comps: &[&Expr],
    pa: &str,
    pb: &str,
    pb_kind: NumKind,
    fields: &[String],
    user_fns: &HashSet<&str>,
) -> Option<Vec<Expr>> {
    let n = comps.len();
    let bodies: Vec<Expr> = comps.iter().map(|c| subst_acc(c, pa, n, fields)).collect();
    let mut binders: HashMap<&str, NumKind> = HashMap::new();
    for &slot in ACC_IDENTS.iter().take(n) {
        binders.insert(slot, NumKind::Float);
    }
    binders.insert(pb, pb_kind);
    bodies
        .iter()
        .all(|c| infer_f64_typed(c, &binders, user_fns) == Some(NumKind::Float))
        .then_some(bodies)
}

/// Decide whether a `reduce(init, (pa, pb) => body)` with an **all-`Float`** tuple/record
/// init can JIT as a multi-slot `f64` fold, returning the substituted component bodies
/// (`$acc0…`). `pb_is_int` is `true` for a range counter, `false` for a `Float`-array
/// element. `None` → not eligible (run the bytecode loop). Mirrors [`reduce_jit_bodies`]'s
/// tuple/record branches, but every slot is `f64` and the components are typed per-node.
pub fn reduce_jit_f64_tuple_bodies(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    pb_is_int: bool,
    user_fns: &HashSet<&str>,
) -> Option<Vec<Expr>> {
    let pb_kind = if pb_is_int { NumKind::Int } else { NumKind::Float };
    if let (Expr::Tuple(inits), Expr::Tuple(comps)) = (init, body) {
        let n = comps.len();
        if n != inits.len()
            || !(2..=MAX_ACC_SLOTS).contains(&n)
            || !inits.iter().all(|e| matches!(e, Expr::Float(_)))
        {
            return None;
        }
        let refs: Vec<&Expr> = comps.iter().collect();
        return check_slot_bodies_f64(&refs, pa, pb, pb_kind, &[], user_fns);
    }
    if let (Expr::Record(inits), Expr::Record(comps)) = (init, body) {
        let n = inits.len();
        if comps.len() != n
            || !(2..=MAX_ACC_SLOTS).contains(&n)
            || !inits.iter().all(|(_, e)| matches!(e, Expr::Float(_)))
        {
            return None;
        }
        // Same field-order requirement as the i64 path: components map to the init's order.
        let fields: Vec<String> = inits.iter().map(|(k, _)| k.clone()).collect();
        if comps.iter().map(|(k, _)| k).ne(fields.iter()) {
            return None;
        }
        let ordered: Vec<&Expr> = comps.iter().map(|(_, e)| e).collect();
        return check_slot_bodies_f64(&ordered, pa, pb, pb_kind, &fields, user_fns);
    }
    None
}

/// Decide whether a `reduce(init, (pa, pb) => body)` can JIT, and if so return its
/// component bodies (slot accesses already substituted to `$acc0…`). `Some([body])` for a
/// scalar `i64` accumulator; `Some([e0, e1, …])` for a 2..=MAX_ACC_SLOTS **tuple** (`a[k]`)
/// or **record** (`a.field`) accumulator whose every component is `i64`-eligible. A record
/// body's components are reordered to the init record's field order (so component `k` is
/// always slot `k`). `None` → run the bytecode loop.
pub fn reduce_jit_bodies(
    init: &Expr,
    body: &Expr,
    pa: &str,
    pb: &str,
    fns: &HashSet<&str>,
) -> Option<Vec<Expr>> {
    if reduce_loop_eligible(body, pa, pb, fns) {
        return Some(vec![body.clone()]);
    }
    if let (Expr::Tuple(inits), Expr::Tuple(comps)) = (init, body) {
        let n = comps.len();
        if n != inits.len() || !(2..=MAX_ACC_SLOTS).contains(&n) {
            return None;
        }
        let refs: Vec<&Expr> = comps.iter().collect();
        return check_slot_bodies(&refs, pa, pb, &[], fns);
    }
    if let (Expr::Record(inits), Expr::Record(comps)) = (init, body) {
        let n = inits.len();
        if comps.len() != n || !(2..=MAX_ACC_SLOTS).contains(&n) {
            return None;
        }
        // Require the body's fields to be in the SAME order as the init's: the slots map
        // to that order, and the tree-walker's result record carries the body's field
        // order — matching them keeps the JIT result byte-identical (a reordered body
        // would still be value-equal but display its fields in a different order, so it
        // falls back to the bytecode loop instead).
        let fields: Vec<String> = inits.iter().map(|(k, _)| k.clone()).collect();
        if comps.iter().map(|(k, _)| k).ne(fields.iter()) {
            return None;
        }
        let ordered: Vec<&Expr> = comps.iter().map(|(_, e)| e).collect();
        return check_slot_bodies(&ordered, pa, pb, &fields, fns);
    }
    None
}

/// Re-check (at JIT-compile time) that already-substituted reduce bodies are `i64`-eligible
/// — a scalar (1 body) over `{pa, pb}`, or a tuple (2..=MAX_ACC_SLOTS bodies) over the slots
/// `{$acc0.., pb}`. Shared by the range reduce loop and the fused reduce sink.
pub(crate) fn bodies_eligible(pa: &str, pb: &str, bodies: &[Expr], fns: &HashSet<&str>) -> bool {
    if bodies.len() == 1 {
        return reduce_loop_eligible(&bodies[0], pa, pb, fns);
    }
    let n = bodies.len();
    if !(2..=MAX_ACC_SLOTS).contains(&n) {
        return false;
    }
    let names: Vec<String> = (0..n).map(acc_ident).collect();
    let mut locals: HashSet<&str> = HashSet::new();
    for nm in &names {
        locals.insert(nm.as_str());
    }
    locals.insert(pb);
    bodies.iter().all(|c| value_eligible(c, fns, &locals, NumKind::Int))
}

pub(crate) fn reduce_bodies_eligible(
    rl: &crate::bytecode::ReduceLoop,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> bool {
    // An f64 accumulator over the i64 counter: capture-free, every component's root `Float`,
    // exactly what `define_reduce_loop`'s float path lowers (via `gen_f64_typed`). Scalar (1
    // body over `{pa, pb}`) or tuple (N>1 substituted bodies over `{$acc0…, pb}`).
    if rl.float {
        // v1b: a float SCALAR body indexing captured `f64` arrays by the counter (the float
        // dot-product). Re-run the same indexed collector and require it reproduce `rl.captures`
        // exactly — all `ArrayF64`, body root `Float` — so the build gate matches the compile
        // gate (`define_reduce_loop` binds exactly these caps into its `arrays` map).
        if !rl.captures.is_empty() {
            if rl.bodies.len() != 1 {
                return false;
            }
            let mut out = IndexedOut::default();
            let root = infer_f64_indexed(
                &rl.bodies[0],
                &rl.pa,
                &rl.pb,
                &HashMap::new(),
                &mut out,
                fns,
                user_fns,
            );
            // Mirror the compile gate exactly, INCLUDING the value-scalar relabel — otherwise the
            // re-derived list differs from the stored one by kind alone and every such kernel
            // silently declines.
            if root == Some(MixT::GFloat) {
                relabel_value_scalars(&mut out.caps, &out.bounds, &out.synth);
            }
            let (caps, bounds) = (out.caps, out.bounds);
            // Reproduce BOTH the capture set and the bounds obligations exactly (the i64 path's
            // rule, now that an f64 kernel can carry `Scalar`/`ScalarValue` caps and affine
            // bounds): any drift would run unchecked native loads behind a pre-check that doesn't
            // describe them.
            // `!caps.is_empty()`, matching the compile gate exactly — see
            // `reduce_jit_f64_range_captures`. These two must relax together or the build
            // declines a kernel the compiler emitted.
            return root == Some(MixT::GFloat)
                && caps == rl.captures
                && bounds == rl.index_bounds
                && !caps.is_empty()
                && caps.iter().all(|c| {
                    matches!(
                        c.kind,
                        CaptureKind::ArrayF64 | CaptureKind::Scalar | CaptureKind::ScalarValue
                    )
                })
                && caps.len() <= MAX_CAPTURES;
        }
        let n = rl.bodies.len();
        if n == 1 {
            // Identical gate to the compiler's `reduce_jit_f64_range_body` (root `Float`, and the
            // division/min-max soundness rule) so the build never lowers a body the compiler
            // rejected — or vice versa.
            return f64_range_body_eligible(&rl.bodies[0], &rl.pa, &rl.pb, fns, user_fns, msigs);
        }
        if !(2..=MAX_ACC_SLOTS).contains(&n) {
            return false;
        }
        let mut binders: HashMap<&str, NumKind> = HashMap::new();
        for &slot in ACC_IDENTS.iter().take(n) {
            binders.insert(slot, NumKind::Float);
        }
        binders.insert(rl.pb.as_str(), NumKind::Int);
        return rl.bodies.iter().all(|c| infer_f64_typed(c, &binders, user_fns) == Some(NumKind::Float));
    }
    // A scalar captured body: re-run the SAME indexed collector the compiler used and
    // require it reproduce `rl.captures` exactly — same names, kinds, and order. This keeps
    // the build gate identical to the compile gate: `define_reduce_loop` binds exactly these
    // captures (scalar values and array bases loaded from the `caps` pointer), so any drift
    // (a body eligibility accepted but the build can't lower, or a different capture set)
    // is caught here and the whole loop falls back to the VM. v1a lowers `Scalar` +
    // `ArrayI64`; an `ArrayF64` cap belongs to the f64 variant, not yet lowered → reject.
    if rl.bodies.len() == 1 && !rl.captures.is_empty() {
        if rl.captures.iter().any(|c| c.kind == CaptureKind::ArrayF64) {
            return false;
        }
        let mut locals: HashSet<&str> = HashSet::new();
        locals.insert(rl.pa.as_str());
        locals.insert(rl.pb.as_str());
        let mut caps: Vec<Capture> = Vec::new();
        let mut bounds: Vec<IndexBound> = Vec::new();
        // `$aff` naming is a deterministic function of the body, so the re-derived caps
        // (which include any synthetic slots) match the stored list iff nothing drifted —
        // the synth expressions themselves were consumed at the compile site's push loop.
        let mut synth: Vec<(String, Expr)> = Vec::new();
        let ok = value_eligible_cap_indexed(
            &rl.bodies[0],
            fns,
            &locals,
            rl.pb.as_str(),
            &mut caps,
            &mut bounds,
            &mut synth,
        );
        // The build gate must reproduce BOTH the capture set and the bounds obligations the VM
        // will check — a drift in either would run the kernel with a pre-check that doesn't match
        // its actual `arr[…]` accesses (an out-of-bounds hazard), so require an exact match.
        return ok && caps == rl.captures && bounds == rl.index_bounds && caps.len() <= MAX_CAPTURES;
    }
    bodies_eligible(&rl.pa, &rl.pb, &rl.bodies, fns)
}

/// True if a `map` body is a pure `i64` value expression over its single binder (calling
/// only `fns`) — the same shape as a reduce body, lowered to a per-element kernel.
pub fn map_kernel_eligible(body: &Expr, binder: &str, fns: &HashSet<&str>) -> bool {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    value_eligible(body, fns, &locals, NumKind::Int)
}

/// At most this many captured variables per kernel (bounds the `caps` slice).
pub const MAX_CAPTURES: usize = 8;

/// Like [`map_kernel_eligible`] but a body referencing **free (captured) variables** is
/// still eligible — each free `i64` variable is recorded (in first-appearance order) and
/// passed to the kernel as a loop-invariant `caps[i]`. Returns the ordered capture names,
/// or `None` if the body is ineligible (a float literal, `/`, a non-eligible call, …) or
/// captures more than [`MAX_CAPTURES`]. Same i64-closed rules as `value_eligible(Int)`.
pub fn map_kernel_captures(body: &Expr, binder: &str, fns: &HashSet<&str>) -> Option<Vec<String>> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    let mut caps: Vec<String> = Vec::new();
    if value_eligible_cap(body, fns, &locals, &mut caps) && caps.len() <= MAX_CAPTURES {
        Some(caps)
    } else {
        None
    }
}

/// Like [`map_kernel_captures`] but the body may additionally read a captured array —
/// `a[it]` (the binder) or `a[i]` (a loop-invariant scalar cap). Returns the ordered
/// captures plus the bounds the VM must discharge before the kernel's unchecked loads,
/// or `None` if ineligible. Shares [`value_eligible_cap_indexed`] with the reduce path:
/// a reduce passes its counter as `pb`, a map passes its binder, and the index shapes
/// the analysis accepts are the same.
///
/// The two paths differ in what `pb` MEANS, and that difference is a soundness cliff,
/// not a detail. A reduce's `pb` is the loop counter, so an [`IndexBound::Counter`] is
/// discharged by the range's endpoints. A map's binder is an ELEMENT VALUE: for
/// `xs.map(x => a[x])` the index is arbitrary data — and possibly negative, which the
/// interpreter Python-WRAPS rather than rejecting, so no cheap scan can discharge it.
/// The VM therefore takes this kernel ONLY when the receiver is a lazy `Range` (whose
/// elements ARE the counter), and only checks that BEFORE materializing it. See
/// [`crate::bytecode::ArrayKernel::index_bounds`] — the obligation is stated there
/// because the VM, not this analysis, is what discharges it.
pub fn map_kernel_captures_indexed(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
) -> Option<IndexedCaptures> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    let mut caps: Vec<Capture> = Vec::new();
    let mut bounds: Vec<IndexBound> = Vec::new();
    let mut synth: Vec<(String, Expr)> = Vec::new();
    if value_eligible_cap_indexed(body, fns, &locals, binder, &mut caps, &mut bounds, &mut synth)
        && caps.len() <= MAX_CAPTURES
    {
        // Relabel purely-value scalars to `ScalarValue` — same as the mixed twin, so the two
        // derivations of one body produce identical lists (the i64 kernel loads a `ScalarValue`
        // as `i64` exactly as it did a `Scalar`, so its behavior is unchanged; the relabel only
        // lets the mixed kernel recognize the same cap as `f64`). The reduce path does NOT
        // relabel, so its captures are untouched. `synth` carries any affine `$aff` terms —
        // the map compile site's push loop evaluates them, exactly as it does the mixed twin's.
        relabel_value_scalars(&mut caps, &bounds, &synth);
        Some((caps, bounds, synth))
    } else {
        None
    }
}

/// f64 `map` eligibility (over a **Floats** source array). The body must use only
/// `+ - *` over the binder, int/float literals, and captured variables, and it must
/// **reference the binder** — so the result is provably `Float` (the binder is `f64`
/// and float-ness propagates through `+ - *`), matching the interpreter. A constant or
/// capture-only body (whose type could be `Int`) is excluded, as are `/` (the
/// interpreter raises on /0 where native fdiv yields ±inf), `%`, `if`, comparisons, and
/// calls — the safe subset that can't introduce a JIT↔interpreter divergence. Returns
/// the ordered captures (passed to the kernel as `f64`), or `None`.
pub fn map_kernel_captures_f64(
    body: &Expr,
    binder: &str,
    user_fns: &HashSet<&str>,
) -> Option<Vec<String>> {
    let mut caps: Vec<String> = Vec::new();
    let mut uses_binder = false;
    // The ROOT must be proven `Float` — with `uses_binder` required that is implied (a
    // Promotable root is a lone leaf), but asserting it directly is what the soundness
    // argument actually says.
    if f64_body_eligible(body, binder, &mut caps, &mut uses_binder, user_fns)
        == Some(F64Proof::Float)
        && uses_binder
        && caps.len() <= MAX_CAPTURES
    {
        Some(caps)
    } else {
        None
    }
}

/// How a node of an f64-source body relates to the INTERPRETER's arithmetic — the type
/// that keeps the monomorphic f64 kernel honest about integers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum F64Proof {
    /// Provably computed in `f64` by the interpreter: the binder (a `Floats` element), a
    /// float literal, a float builtin, or any operation with such an operand — the
    /// interpreter promotes the other side on the spot, so the kernel's f64 arithmetic
    /// matches bit-for-bit.
    Float,
    /// An int literal or a capture: EXACT to convert (`as f64`, the very conversion the
    /// dispatch marshal performs), but only at a node where a `Float` operand forces the
    /// interpreter to promote it. Anywhere else the interpreter computes in i64.
    Promotable,
}

/// The typed eligibility for the f64 (Floats-source) kernel. `None` = ineligible.
///
/// The rule that matters is `(Promotable, Promotable) => None`, and it exists because its
/// absence was a WRONG-VALUE, JIT-vs-interpreter divergence — the oracle-breaking kind:
///
///     k = 4611686018427387904            # 2^62, an Int
///     ys = (0..100000).map(it * 1.0)
///     ys.map(it + (k + k)).first()       # JIT:  9223372036854775808.0
///                                        # VM/tw: -9223372036854775808.0
///
/// The interpreter computes the `Int + Int` subexpression in i64 — WRAPPING — and only
/// then promotes; this kernel is monomorphic f64 and computes `f64(k) + f64(k)`, which
/// does not wrap. Same for `k * k`, for pure literal arithmetic
/// (`it + (9223372036854775807 + 1)`), and for `-k` (interpreter `wrapping_neg`, kernel
/// `fneg` — a sign flip, not a wrap, divergent at exactly `i64::MIN`; that one arrived
/// with the unary-minus admission and is fixed by the same rule). The mixed kernels are
/// immune by construction — `gen_value_typed` types per node and emits Int subtrees as
/// wrapping i64 ops — and probes confirmed the reduce/fused/scan families agree; this
/// monomorphic family was the only unsound one.
///
/// A `Promotable` under a `Float` operand is exact: the leaf is converted by `as f64`
/// once, which is bit-identical to what the interpreter's promotion does at that node,
/// and to what the dispatch marshal does to an `Int` capture.
fn f64_body_eligible(
    e: &Expr,
    binder: &str,
    caps: &mut Vec<String>,
    uses_binder: &mut bool,
    user_fns: &HashSet<&str>,
) -> Option<F64Proof> {
    match e {
        Expr::Float(_) => Some(F64Proof::Float),
        Expr::Int(_) => Some(F64Proof::Promotable),
        Expr::Ident { name, .. } => {
            if name == binder {
                *uses_binder = true;
                Some(F64Proof::Float)
            } else {
                if !caps.iter().any(|c| c == name) {
                    caps.push(name.clone());
                }
                Some(F64Proof::Promotable)
            }
        }
        Expr::Binary { op, left, right, .. } => {
            if !matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) {
                return None;
            }
            let l = f64_body_eligible(left, binder, caps, uses_binder, user_fns)?;
            let r = f64_body_eligible(right, binder, caps, uses_binder, user_fns)?;
            match (l, r) {
                // Int OP Int is the interpreter's i64 (wrapping) arithmetic — this kernel
                // cannot reproduce it, so the body declines to the VM, which is the
                // semantics. Everything else has a Float operand, so the interpreter
                // promotes here too.
                (F64Proof::Promotable, F64Proof::Promotable) => None,
                _ => Some(F64Proof::Float),
            }
        }
        // Negation of a PROVEN f64 is `fneg`, the interpreter's exact IEEE sign flip. A
        // Promotable operand must decline: the interpreter negates an Int with
        // `wrapping_neg`, which differs from a sign flip at exactly `i64::MIN`.
        Expr::Unary { op: UnOp::Neg, expr, .. } => {
            (f64_body_eligible(expr, binder, caps, uses_binder, user_fns)? == F64Proof::Float)
                .then_some(F64Proof::Float)
        }
        // `sqrt`/`abs`/`min`/`max` (emitted inline by `gen_builtin_f64`) — only the real
        // builtin, never a user function of the same name (which the f64 kernel can't
        // call). Arguments must be PROVEN Float: the interpreter's `abs(Int)` stays Int
        // (and wraps at `i64::MIN`), and a mixed-type `min`/`max` returns whichever
        // original operand wins, so its type is runtime-dependent — the same reasons
        // `infer_mixed_kind` rejects them.
        Expr::Call { name, args, .. } => {
            (jit_float_builtin_arity(name) == Some(args.len())
                && !user_fns.contains(name.as_str())
                && args.iter().all(|a| {
                    f64_body_eligible(a, binder, caps, uses_binder, user_fns)
                        == Some(F64Proof::Float)
                }))
            .then_some(F64Proof::Float)
        }
        _ => None,
    }
}

/// Is `body` a **mixed** `Int`-source → `Float` map: an `f64`-producing expression over
/// an `i64` element? Eligible when it uses the binder, is built only from `+ - *` over the
/// binder / int / float literals / free scalars, and its inferred root type is `Float`
/// (else it's a pure `i64` map). Returns the ordered captures, or `None` if ineligible.
/// The kernel ([`define_array_kernel`] with `mixed`) types every node bottom-up by
/// the interpreter's promotion rule — `Int OP Int` stays `i64` (wrapping `iadd/isub/imul`),
/// and the *first* `Float` operand promotes via `fcvt_from_sint` — so it matches the
/// interpreter bit-for-bit, including any `i64` wrap in an integer subexpression.
///
/// A free scalar rides as a plain `i64` [`CaptureKind::Scalar`] (loaded as `elem_ty`, which
/// is `I64` for a mixed kernel, and typed `Int` by [`gen_value_typed`]'s `Ident` arm). Captures
/// were once excluded here outright, because "a capture's runtime type is unknown at compile
/// time, and an `Int` capture in an `Int` subexpression must wrap as `i64`, which we couldn't
/// guarantee". We CAN guarantee it — just not statically: both dispatch sites (`try_map_range`
/// and `Op::TryJitMap` in `vm.rs`) require every capture to be a `Value::Int` at run time and
/// decline to the bytecode loop otherwise, which is the identical runtime proof the plain i64
/// map path has always relied on. A `Float` in that slot would promote EARLIER in the kernel
/// than in the interpreter, so declining is not a missed optimization but the correctness rule.
///
/// Excluding them cost a lot: capture-free `((7 * j) % 100) * 0.5` ran native while the same
/// body with `7` replaced by a variable fell to the VM — 0.01s vs 0.37s over 4M elements. That
/// is the shape every nested array build has (the inner map captures the outer binder), and
/// `map(i => i * dt)` besides.
/// `fns` is the `i64`-eligible set (`int_eligible` at build time, `jit_fn_set()` at compile
/// time — the same set by contract). A user function in it may be CALLED from a mixed body:
/// it takes `Int` arguments and returns `Int` by construction, which is precisely the
/// contract its `i64` specialization was compiled under, so the call types with no extra
/// information. Without this, factoring a loop body into a named function dropped the whole
/// map to the bytecode loop — measured 1.50s against 0.02s inline over 20M elements.
pub fn mixed_map_eligible(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<Vec<Capture>> {
    let mut uses_binder = false;
    let mut caps: Vec<Capture> = Vec::new();
    let root = infer_mixed_kind(body, binder, &mut uses_binder, &mut caps, fns, user_fns, msigs)?;
    (root == NumKind::Float && uses_binder && caps.len() <= MAX_CAPTURES).then_some(caps)
}

/// The **Int-rooted** mixed map: an `i64` source and an `i64` RESULT, through `Float`
/// intermediates — `map(i => to_int(to_float(i) * 1.5))`, the shape that previously had no
/// kernel at all (measured 4.05s JIT against 4.01s VM: silently interpreted). The same
/// node-by-node typing as [`mixed_map_eligible`], but the root must be `Int` — the kernel
/// reads `i64` and writes `i64`, so its ABI is exactly the plain i64 kernel's and it rides
/// the same FFI wrappers, dispatch arm, and in-place reuse.
///
/// It must never COMPETE with the plain i64 kernel: the compile site tries the i64 analysis
/// first, and the build re-gate requires `map_kernel_captures` to have REJECTED the body
/// (a float literal or float-producing call somewhere is what makes this shape this shape).
/// The four rounding builtins that RAISE when their result leaves the i64 range. Arity 1
/// only — `round(x, digits)` stays a `Float` and is a different (non-raising) operation the
/// analyses do not admit.
const RAISING_ROUNDERS: &[&str] = &["floor", "ceil", "round", "trunc"];

/// Whether a kernel body can RAISE where native code would silently produce inf/NaN or a
/// wrapped integer — i.e. whether its kernel needs the poison out-param and its dispatch the
/// poison call wrapper. A user function SHADOWING one of these names is not the raising
/// builtin (the call dispatches to the user's function), so it does not count.
/// Over-approximates on an `Int`-typed argument (where the builtin is the identity and cannot
/// raise): the kernel then carries a poison slot it never sets, which costs one dead store
/// and nothing else.
///
/// Shared by MAP and REDUCE bodies — the question and the expression forms are identical, and
/// one predicate is what keeps the two from drifting apart as either side widens. Its answer
/// is stored on the kernel ([`crate::bytecode::ArrayKernel::raises`],
/// [`crate::bytecode::ReduceLoop::raises`]) rather than recomputed by the VM, because the
/// answer decides an ABI and the VM cannot reach the user functions a call would need.
pub fn body_raises(e: &Expr, user_fns: &HashSet<&str>, msigs: &MixedSigTable) -> bool {
    match e {
        Expr::Call { name, args, .. } => {
            // A call to a MIXED specialization always counts: its ABI carries a poison
            // pointer precisely because it can bail — a NaN comparison anywhere in its body,
            // or a `/0`. The kernel must therefore be built with the poison signature so the
            // callee's flag has somewhere to land, even when the map body itself contains no
            // rounder and no division. (Without this the kernel is built poison-free, the VM
            // calls the non-poison wrapper, and a raising callee is silently swallowed.)
            (msigs.contains_key(name.as_str()) && user_fns.contains(name.as_str()))
                || (RAISING_ROUNDERS.contains(&name.as_str())
                    && args.len() == 1
                    && !user_fns.contains(name.as_str()))
                || args.iter().any(|a| body_raises(a, user_fns, msigs))
        }
        // Any `/`: the interpreter raises on a zero divisor. Over-approximates on a nonzero
        // literal divisor (which cannot raise) — that costs a dead poison slot, nothing else.
        Expr::Binary { op: BinOp::Div, .. } => true,
        Expr::Binary { left, right, .. } => {
            body_raises(left, user_fns, msigs) || body_raises(right, user_fns, msigs)
        }
        Expr::Unary { expr, .. } => body_raises(expr, user_fns, msigs),
        Expr::Index { recv, index, .. } => {
            body_raises(recv, user_fns, msigs) || body_raises(index, user_fns, msigs)
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            body_raises(cond, user_fns, msigs)
                || body_raises(then_branch, user_fns, msigs)
                || body_raises(else_branch, user_fns, msigs)
        }
        // EVERY shape an eligibility analysis admits MUST have an arm here. This
        // predicate decides whether the kernel is built WITH its poison cell, and a
        // missing arm under-reports: the v0.2.6 `let` widening admitted `Let` bodies
        // into the f64 analyses and codegen but left this fn's `_ => false` to answer
        // for them — so `let d = sq(i * 1.0) in a + d` built a poison-free kernel and
        // hit the mixed-call codegen's unreachable! (SIGABRT, rc 134, uncatchable),
        // and `let inv = 1.0 / e in a + inv` silently printed `inf` at rc 0 where both
        // interpreters raise. Found by the v0.2.6 stabilization sweep (p51/p08).
        Expr::Let { bindings, body, .. } => {
            bindings.iter().any(|(_, v)| body_raises(v, user_fns, msigs))
                || body_raises(body, user_fns, msigs)
        }
        _ => false,
    }
}

pub fn mixed_map_int_root_eligible(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<Vec<Capture>> {
    let mut uses_binder = false;
    let mut caps: Vec<Capture> = Vec::new();
    let root = infer_mixed_kind(body, binder, &mut uses_binder, &mut caps, fns, user_fns, msigs)?;
    (root == NumKind::Int && uses_binder && caps.len() <= MAX_CAPTURES).then_some(caps)
}

/// Bottom-up type of a mixed-map node, or `None` if it contains anything outside the
/// eligible shape (a non-binder ident, a non-`{+,-,*}` operator, a non-eligible call, …).
/// Mirrors the codegen in [`gen_value_typed`] exactly. The pure builtins `sqrt`/`abs`/
/// `min`/`max` are typed like the interpreter: `sqrt` is always `Float`; `abs` preserves
/// its arg kind; `min`/`max` need both args the **same** kind (a mixed `min(int, float)`
/// returns whichever original operand wins, so its type is runtime-dependent — rejected).
fn infer_mixed_kind(
    e: &Expr,
    binder: &str,
    uses_binder: &mut bool,
    caps: &mut Vec<Capture>,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        // Negation PRESERVES its operand's kind, so it needs no promotion rule of its own.
        // Emitted by `gen_value_typed`'s twin arm as `ineg`/`fneg` — wrapping exactly like
        // the interpreter's `wrapping_neg`, and the exact IEEE sign flip, respectively.
        //
        // Admitted here and emitted there in the SAME commit, deliberately: `e30f9fe` fixed
        // the i64 kernel by adding eligibility alone, because `gen_value` already lowered
        // `Neg`; this path had NEITHER, and admitting a shape the codegen cannot emit is
        // how this area was reverted three times before.
        Expr::Unary { op: UnOp::Neg, expr, .. } => {
            infer_mixed_kind(expr, binder, uses_binder, caps, fns, user_fns, msigs)
        }
        // A USER function with an `i64` specialization. Tried BEFORE the builtin arm, so a
        // user function shadowing `abs`/`min`/`max` dispatches to the user's function — the
        // precedence `gen_value` already establishes via its `fn_ids` lookup, and mirrored
        // by `gen_value_typed`'s twin arm.
        //
        // Every argument must type `Int`, which is exactly the contract the callee's i64
        // specialization was compiled under (`int_eligible` means "i64-closed for all-`Int`
        // arguments"), so the result is an `i64` and types `Int` here. The enclosing
        // expression then promotes it at the first `Float` precisely where the interpreter
        // does. A FLOAT argument is rejected rather than converted: the callee has no f64
        // form to call, and silently truncating or promoting would not be the interpreter's
        // answer.
        //
        // That `Int` check is defence in depth, not the only line: relaxing it does not
        // produce a wrong answer, because the f64 value would reach an `i64` call signature
        // and Cranelift rejects the function, so the kernel simply declines (verified by
        // removing the check — the three float-argument cases still agree on all engines).
        // It is kept because the alternative is CONSTRUCTING ill-typed IR and relying on the
        // builder to refuse it, and a builder that panics instead of erroring would breach
        // ADR-0024's never-abort guarantee. Cheaper to never build it.
        // ONE ARM FOR EVERY USER CALL, because two of them could not both be tried.
        //
        // This used to be two arms: an i64 one guarded by `fns.contains(name)`, and a mixed
        // one guarded by `msigs.contains_key(name)` below it, with a comment asserting that
        // "an all-`Int` function has no mixed form, so the two never compete". THAT WAS
        // FALSE, and it cost 66x on the shape this JIT exists for:
        //
        //     fn f(x: Float) -> Float = x * x            (0..20M).map(i => f(to_float(i)))
        //     fn f(x: Float) -> Float = x * x * 1.0       the SAME call site
        //
        // 1.85s and 0.028s. `fns` means "i64-closed BODY", not "Int parameters" — and
        // `x * x` is i64-closed, so the first `f` is in BOTH sets. The i64 arm claimed the
        // call site by name, typed `to_float(i)` as Float, and returned None. Rust match arms
        // cannot fall through, so the mixed arm twenty lines below was unreachable for
        // exactly the callee that needed it. Adding a redundant `* 1.0` to the CALLEE — which
        // does nothing to the call site — pushed `f` out of `fns` and let the mixed arm see
        // it. That is a two-character difference with a 66x cost and no feedback.
        //
        // Merged, the priority is unchanged where it used to apply and defined where it did
        // not: all-Int arguments to an i64-closed function still take the i64 path first;
        // anything else gets the mixed specialization if the argument kinds EQUAL the
        // callee's parameter kinds. That equality is strict on purpose — the specialization
        // was compiled for exactly those kinds and there is no promoting at the boundary,
        // the same rule `infer_typed_env` uses for a mixed sibling call.
        //
        // Every argument is typed EXACTLY ONCE, into a `Vec`, before anything is decided.
        // Walking them twice would be fine for `record_cap` (which dedupes by name) but is
        // not a property worth relying on for `uses_binder` or for whatever capture-order
        // logic arrives next.
        Expr::Call { name, args, .. } if user_fns.contains(name.as_str()) => {
            let mut kinds = Vec::with_capacity(args.len());
            for a in args {
                kinds.push(infer_mixed_kind(a, binder, uses_binder, caps, fns, user_fns, msigs)?);
            }
            let all_int = kinds.iter().all(|k| *k == NumKind::Int);
            if all_int && fns.contains(name.as_str()) {
                if !jit_builtin_arity_ok(name, args.len()) {
                    return None;
                }
                return Some(NumKind::Int);
            }
            // The MIXED specialization — the `Float`-parameter callee. The kernel marshals
            // to the bits ABI and shares its poison cell, so a NaN-compare or `/0` inside
            // the callee bails the whole map exactly as it bails a mixed function.
            let (params, ret) = msigs.get(name.as_str())?;
            if kinds.len() != params.len() || kinds.iter().zip(params).any(|(k, w)| k != w) {
                return None;
            }
            Some(*ret)
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    Some(NumKind::Float) // sqrt always returns Float
                }
                // `to_float` is the explicit Int->Float conversion: always Float, and the typed
                // codegen emits the same `fcvt_from_sint` promotion it already emits for `sqrt`.
                ("to_float", 1) => {
                    infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs), // preserves kind
                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                ("to_int" | "sign", 1) => {
                    infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    Some(NumKind::Int)
                }
                // The RAISING rounders: `Float` in, `Int` out, and an out-of-i64-range result
                // raises where the never-raising `to_int` saturates. Admissible only because
                // the kernel carries a poison out-param (`ArrayKernel::raises`, set by
                // `body_raises` from this same name list): on any raising condition the
                // codegen sets poison, the VM discards the whole output, and the bytecode loop
                // re-runs to raise the exact interpreter error. An `Int` argument makes the
                // builtin the identity (`floor(2) == 2`) and is admitted as such. (This plain
                // analysis types every operand `Int` or genuine `Float` — value scalars ride
                // as `i64` here — so there is no unpromoted-scalar case to refuse.)
                ("floor" | "ceil" | "round" | "trunc", 1) => {
                    match infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)? {
                        NumKind::Int => Some(NumKind::Int), // identity on Int
                        NumKind::Float => Some(NumKind::Int),
                    }
                }
                ("min" | "max", 2) => {
                    let ka = infer_mixed_kind(&args[0], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    let kb = infer_mixed_kind(&args[1], binder, uses_binder, caps, fns, user_fns, msigs)?;
                    if ka == kb { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        Expr::Ident { name, .. } => {
            if name == binder {
                *uses_binder = true;
                Some(NumKind::Int) // the `i64` element
            } else {
                // A free scalar, typed `Int` and loaded `i64` — sound ONLY because the VM
                // proves the value really is a `Value::Int` before dispatch (see
                // [`mixed_map_eligible`]); a `Float` there declines to the bytecode loop.
                // Typing it `Int` is what keeps an integer subexpression containing it
                // wrapping exactly like the interpreter's.
                record_cap(caps, name, CaptureKind::Scalar).then_some(NumKind::Int)
            }
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_mixed_kind(left, binder, uses_binder, caps, fns, user_fns, msigs)?;
            let rk = infer_mixed_kind(right, binder, uses_binder, caps, fns, user_fns, msigs)?;
            Some(if lk == NumKind::Float || rk == NumKind::Float {
                NumKind::Float
            } else {
                NumKind::Int
            })
        }
        // `/` is always float division and always yields Float, for ANY eligible divisor —
        // admissible because `body_raises` counts every `/`, so the kernel carries the
        // poison accumulator `gen_value_typed`'s Div arm ORs `divisor == 0.0` into (the
        // interpreter raises on `/0` where native `fdiv` yields inf). This is what lets
        // `ceil(to_float(i) / 4.0)` compile instead of forcing the `* 0.25` spelling.
        Expr::Binary { op: BinOp::Div, left, right, .. } => {
            infer_mixed_kind(left, binder, uses_binder, caps, fns, user_fns, msigs)?;
            infer_mixed_kind(right, binder, uses_binder, caps, fns, user_fns, msigs)?;
            Some(NumKind::Float)
        }
        // The i64-closed integer ops (`%`, `//`, bitwise, shifts) — the SAME safe subset as
        // `value_eligible`, so an integer subexpression like `j % 97` in a float-producing map
        // body (`(j % 97) * 1.0`) stays `i64` and promotes at the first float operand, instead
        // of forcing the whole map onto the VM. BOTH operands must be `Int` (these ops are
        // meaningless on `f64`); the result is `Int`. Same const-restrictions as `value_eligible`.
        Expr::Binary {
            op: op @ (BinOp::Mod | BinOp::FloorDiv | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr),
            left,
            right,
            ..
        } => {
            let op_ok = match op {
                BinOp::Mod | BinOp::FloorDiv => matches!(**right, Expr::Int(n) if n > 0),
                BinOp::Shl | BinOp::Shr => matches!(**right, Expr::Int(n) if (0..=63).contains(&n)),
                _ => true, // bitwise: unconditionally i64-closed
            };
            if !op_ok {
                return None;
            }
            let lk = infer_mixed_kind(left, binder, uses_binder, caps, fns, user_fns, msigs)?;
            let rk = infer_mixed_kind(right, binder, uses_binder, caps, fns, user_fns, msigs)?;
            if lk == NumKind::Int && rk == NumKind::Int {
                Some(NumKind::Int)
            } else {
                None // an i64-only op with a Float operand is not a valid Helix expression
            }
        }
        _ => None,
    }
}

/// The **indexed** mixed-map analysis: an i64 range source, an `f64` result, and a body that
/// reads captured **`f64` arrays** by the binder (`a[it]`) or by a loop-invariant scalar
/// (`a[k]`) — the vector-add / AXPY / gather-transform shape `(0..n).map(i => a[i] + b[i])`.
/// Returns the ordered captures plus the bounds the VM must discharge, or `None`.
///
/// This types `a[…]` as **`Float`** where [`map_kernel_captures_indexed`] (the i64 twin over
/// the same body shapes) types it `Int`. Both analyses record the same names, kinds, and
/// bounds in the same first-appearance order, so ONE stored kernel can carry BOTH
/// specializations and the VM dispatches on the runtime capture type: all-`Ints` caps run the
/// i64 kernel, all-`Floats` caps run this mixed kernel, and a mismatch falls back to the
/// bytecode loop. The `ArrayI64` capture kind therefore means "array indexed by the counter",
/// not "an array of i64" — which marshal it gets is the dispatch's decision, and the marshal
/// itself is the type guard (an `Ints` buffer never reaches this kernel's F64 loads).
///
/// INDEX scalars (`a[k]`, affine `base`/`coef`) stay `Scalar` (`i64`, an index is an integer);
/// VALUE scalars (`a` in `a * x[i]`) become [`CaptureKind::ScalarValue`] via
/// [`relabel_value_scalars`], loaded `f64` here and `i64` in the i64 twin. A value scalar is
/// admitted only where a genuine float promotes it (SAXPY `a * x[i]`), not `a * i` — see
/// [`MixT`]. The bounds story is IDENTICAL to the i64 path — same [`IndexBound`]s, same
/// lazy-range-only discharge (`map_index_caps` in `vm.rs`) — because bounds depend on the
/// index arithmetic, which is `i64` in both.
pub type MapIndexAnalysis = (Vec<Capture>, Vec<IndexBound>, Vec<(String, Expr)>);

pub fn mixed_map_captures_indexed(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<MapIndexAnalysis> {
    let mut acc = IndexedOut::default();
    let root = infer_mixed_kind_indexed(body, binder, &mut acc, fns, user_fns, msigs)?;
    let IndexedOut { mut caps, bounds, synth } = acc;
    // GFloat root: a genuine `f64` the kernel writes to the output buffer (a bare-`SFloat` root
    // — an un-promoted value scalar — is rejected, matching the interpreter). Non-empty bounds:
    // an unindexed body belongs to the plain i64/f64/mixed analyses, which run first at the
    // compile site.
    if root == MixT::GFloat && !bounds.is_empty() && caps.len() <= MAX_CAPTURES {
        relabel_value_scalars(&mut caps, &bounds, &synth);
        Some((caps, bounds, synth))
    } else {
        None
    }
}

/// The VALUE-SCALAR variant of the plain mixed map: an unindexed `Int`→`Float` body whose
/// free scalars ride as **f64 bits** (`ScalarValue`) instead of proven-`Int` `i64`s. This is
/// the second specialization of the same stored kernel: the plain analysis
/// ([`mixed_map_eligible`]) types captures `Int` and its dispatch declines when one is a
/// runtime `Float` — so `d = 4.0; map(i => to_float(i) / d)` ran on the VM (3.48s against
/// 0.24s for `d = 4` at 20M) while producing identical values. Here the [`MixT`] analysis
/// admits a capture only where a genuine float promotes it (`mix_combine`'s sabotage-proven
/// rule), which is exactly when riding as f64 matches the interpreter bit for bit.
///
/// Returns the ordered captures (all relabeled `ScalarValue`), or `None`. Bounds and synth
/// must be EMPTY — an indexed body belongs to [`mixed_map_captures_indexed`] — and the list
/// must be non-empty, since a capture-free body is already the plain kernel's territory.
/// The build gate compares NAMES AND ORDER against the stored list (the stored kinds are the
/// plain analysis's `Scalar`s; the kinds here are what this specialization loads by).
pub fn mixed_map_value_scalar_eligible(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<Vec<Capture>> {
    let mut acc = IndexedOut::default();
    let root = infer_mixed_kind_indexed(body, binder, &mut acc, fns, user_fns, msigs)?;
    let IndexedOut { mut caps, bounds, synth } = acc;
    if root == MixT::GFloat
        && bounds.is_empty()
        && synth.is_empty()
        && !caps.is_empty()
        && caps.len() <= MAX_CAPTURES
        && caps.iter().all(|c| c.kind == CaptureKind::Scalar)
    {
        relabel_value_scalars(&mut caps, &bounds, &synth);
        Some(caps)
    } else {
        None
    }
}

/// The kernel-vs-interpreter type of an indexed-mixed subexpression. The mixed kernel
/// evaluates integer subexpressions in `i64` and promotes at the first FLOAT, exactly like
/// the interpreter's `arith` — so the two agree bit-for-bit ONLY if they promote at the same
/// point. A value scalar breaks that: it rides as `f64` in the kernel but is possibly-`Int`
/// at runtime, so the interpreter keeps it `i64` until IT hits a float. `MixT` tracks the
/// distinction that makes the promotion points line up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MixT {
    /// Both engines evaluate this in `i64` (binder, int literal, index scalar, i64-closed op).
    Int,
    /// A GENUINE float in both engines — an array load or a float literal — so combining it
    /// with anything promotes identically (the interpreter promotes the `i64` side to `f64`,
    /// which is the same `fcvt` the kernel does). Safe to combine with anything.
    GFloat,
    /// A value SCALAR riding as `f64` but possibly `Int` at runtime. Safe ONLY once a
    /// `GFloat` has promoted it: `a * x[i]` (`SFloat * GFloat`) is fine because the
    /// interpreter also promotes `a` there, but `a * i` / `a + b` (`SFloat` with `Int` or
    /// another bare `SFloat`) would be `i64` in the interpreter and `f64` in the kernel —
    /// diverging once the true product exceeds 2^53. Such a node is REJECTED.
    SFloat,
}

/// Combine two `+`/`-`/`*` operand kinds: a genuine float promotes anything (both engines
/// promote, so it is safe); two `Int`s stay `Int`; a value scalar NOT paired with a genuine
/// float is the divergence case and is rejected. Shared by the mixed MAP analysis
/// ([`infer_mixed_kind_indexed`]) and the f64 indexed REDUCE analysis ([`infer_f64_indexed`])
/// so both sites enforce one rule — the rule proven load-bearing by sabotage (forcing
/// `(SFloat, Int)` to combine makes `(2^53+1) * 3 + x[i]` differ from the interpreter).
fn mix_combine(l: MixT, r: MixT) -> Option<MixT> {
    match (l, r) {
        (MixT::GFloat, _) | (_, MixT::GFloat) => Some(MixT::GFloat),
        (MixT::Int, MixT::Int) => Some(MixT::Int),
        // (SFloat, Int) | (Int, SFloat) | (SFloat, SFloat): the interpreter may do i64.
        _ => None,
    }
}

/// Bottom-up [`MixT`] of an indexed-mixed node, recording captures/bounds as it goes —
/// [`infer_mixed_kind`]'s arm set plus the index shapes and value-scalar captures, mirroring
/// [`gen_value_typed`]'s codegen (a node this admits and that miscompiles is a divergence, so
/// the two stay twins). No `Let` arm — `gen_value_typed` has none, so the counter-shadowing
/// hazard the i64 path guards against cannot arise here: a shadowing body is simply
/// ineligible. The caller requires the root to be [`MixT::GFloat`] (the map writes `f64`).
fn infer_mixed_kind_indexed(
    e: &Expr,
    binder: &str,
    out: &mut IndexedOut,
    fns: &HashSet<&str>,
    user_fns: &HashSet<&str>,
    msigs: &MixedSigTable,
) -> Option<MixT> {
    match e {
        Expr::Int(_) => Some(MixT::Int),
        Expr::Float(_) => Some(MixT::GFloat),
        Expr::Ident { name, .. } => {
            if name == binder {
                Some(MixT::Int) // the i64 range element
            } else {
                // A value scalar — recorded `Scalar` here, relabeled to `ScalarValue` by
                // `relabel_value_scalars` once the bounds show it is not an index. It rides as
                // `f64` in the mixed kernel, so `SFloat`.
                record_cap(&mut out.caps, name, CaptureKind::Scalar).then_some(MixT::SFloat)
            }
        }
        Expr::Index { recv, index, .. } => {
            let arr = match &**recv {
                Expr::Ident { name, .. } if name != binder => name,
                _ => return None,
            };
            let ap = record_cap_pos(&mut out.caps, arr, CaptureKind::ArrayI64)?;
            match &**index {
                // `a[binder]`: read by the counter → a Counter bound.
                Expr::Ident { name: idx, .. } if idx == binder => {
                    push_bound(&mut out.bounds, IndexBound::Counter { array: ap as u32 });
                }
                // `a[k]`: a free loop-invariant scalar → a point bound. The index scalar is
                // recorded `Scalar` and STAYS `Scalar` (an index is `i64`).
                Expr::Ident { name: idx, .. } if idx != arr => {
                    let sp = record_cap_pos(&mut out.caps, idx, CaptureKind::Scalar)?;
                    push_bound(&mut out.bounds, IndexBound::Scalar { array: ap as u32, scalar: sp as u32 });
                }
                // An AFFINE index (`a[2*i]`, `a[i*n + k]` with the map binder as the counter)
                // — the same admission as the f64 reduce's [`infer_f64_indexed`]. The whole
                // index is validated first as a pure `i64` expression over the binder, free
                // scalars, and `Int` literals (codegen lowers it VERBATIM from those caps, so
                // it must be checked verbatim; every leaf effect-free and non-trapping, which
                // licenses `affine_split`'s algebraic folding). `base`/`coef` land as extra
                // Scalar cap slots — bare idents reuse the body's own caps, compound terms
                // (`i*n`) get a synthetic `$aff{k}` slot the compile site evaluates once —
                // and the VM proves the two ENDPOINT indices of the range in bounds, in i128
                // (`map_index_caps`, composed with the range's step). There is no `pa` in a
                // map, so the empty string — never a legal ident — fills that reject-slot.
                _ => {
                    index_scalars_eligible(index, "", binder, &mut out.caps)?;
                    let (base, coef) = affine_split(index, binder)?;
                    let bp = record_index_term(&mut out.caps, &mut out.synth, base)?;
                    let cp = record_index_term(&mut out.caps, &mut out.synth, coef)?;
                    push_bound(
                        &mut out.bounds,
                        IndexBound::Affine { array: ap as u32, base: bp as u32, coef: cp as u32 },
                    );
                }
            }
            Some(MixT::GFloat) // an f64 array load is a genuine float
        }
        Expr::Call { name, args, .. } if !user_fns.contains(name.as_str()) => {
            match (name.as_str(), args.len()) {
                // `sqrt` promotes its argument to `f64` in BOTH engines, so an `SFloat` arg is
                // safe here and the result is a genuine float.
                ("sqrt", 1) => {
                    infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)?;
                    Some(MixT::GFloat)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)?;
                    Some(MixT::GFloat)
                }
                // `abs`/`min`/`max` do NOT promote (interp `abs(Int)` is `iabs`, `min(Int,Int)`
                // an i64 compare) — so an `SFloat` argument would diverge. Admit only genuine
                // floats or ints, and preserve the kind (an `Int` `abs`/`min` stays i64).
                                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                // An unpromoted value scalar is refused for the same reason `abs` refuses one:
                // its runtime type is not yet pinned, and `to_int`/`sign` read it directly.
                ("to_int" | "sign", 1) => match infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)? {
                    MixT::SFloat => None,
                    _ => Some(MixT::Int),
                },
("abs", 1) => match infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)? {
                    MixT::SFloat => None,
                    k => Some(k),
                },
                ("min" | "max", 2) => {
                    let ka = infer_mixed_kind_indexed(&args[0], binder, out, fns, user_fns, msigs)?;
                    let kb = infer_mixed_kind_indexed(&args[1], binder, out, fns, user_fns, msigs)?;
                    if ka == kb && ka != MixT::SFloat { Some(ka) } else { None }
                }
                _ => None,
            }
        }
        // A USER FUNCTION. Without this arm, a body that BOTH captures a float and calls a
        // user function declines — and that is exactly the shape of a numerical derivative,
        // where the captured value is the step size:
        //
        //     h = 0.001
        //     fn f(x: Float) -> Float = x * x
        //     (0..10M).map(i => (f(to_float(i) + h) - f(to_float(i))) / h)     1.783 s
        //     …the same body with `h` written as the literal 0.001              0.021 s
        //
        // 86x for naming a constant. Both halves already worked on their own — a float
        // capture with no call, and a user call with no capture (becf927) — so the gap was
        // only that this walker had no user-call arm at all, and no access to the tables it
        // would need to type one.
        //
        // TWO RULES, AND THE SECOND IS THE SUBTLE ONE:
        //
        // * An i64-closed function with all-`Int` arguments returns `Int`, the same priority
        //   the unindexed walker gives it.
        // * A `Float` parameter must receive a GENUINE float (`GFloat`), never an `SFloat` —
        //   the same rule `abs`, `to_int` and `sign` apply two arms above, for the same
        //   reason. An `SFloat` is a value scalar riding as `f64` that may be an `Int` at
        //   runtime, and an ANNOTATION IS NOT A COERCION: with `c = 2^53+1` and
        //   `fn f(x: Float) -> Float = x * x`, the interpreter computes `f(c)` as a WRAPPING
        //   i64 multiply and answers `18014398509481985` (an Int!), while `f(to_float(c))` is
        //   an f64 multiply answering `8.1e31`. Handing a callee an unpromoted capture would
        //   pick the second while the interpreter picks the first.
        //
        //   DEFENCE IN DEPTH, and stated as such because sabotage would not break it:
        //   relaxing this to admit `SFloat` left every probe — including that 2^53 pair —
        //   byte-identical on all three engines, because the kernel's runtime dispatch
        //   independently declines when a `ScalarValue` capture turns out to be an `Int`. The
        //   guard is kept for the reason the sibling check one function up is kept: the
        //   alternative is CONSTRUCTING ill-typed IR and relying on a later check to refuse
        //   it, and this file would rather never build it.
        //
        //   A capture reaches a callee as a genuine float only once something promoted it,
        //   which is exactly what `to_float(i) + h` does — so the derivative shape qualifies.
        //
        // No codegen work: the value-scalar and indexed variants both lower through
        // `gen_value_typed`, whose merged call arm already emits both forms.
        Expr::Call { name, args, .. } if user_fns.contains(name.as_str()) => {
            let mut kinds = Vec::with_capacity(args.len());
            for a in args {
                kinds.push(infer_mixed_kind_indexed(a, binder, out, fns, user_fns, msigs)?);
            }
            if kinds.iter().all(|k| *k == MixT::Int) && fns.contains(name.as_str()) {
                return jit_builtin_arity_ok(name, args.len()).then_some(MixT::Int);
            }
            let (params, ret) = msigs.get(name.as_str())?;
            if kinds.len() != params.len() {
                return None;
            }
            for (k, want) in kinds.iter().zip(params) {
                let ok = match want {
                    NumKind::Int => *k == MixT::Int,
                    // NEVER `SFloat` — see above.
                    NumKind::Float => *k == MixT::GFloat,
                };
                if !ok {
                    return None;
                }
            }
            Some(match ret {
                NumKind::Int => MixT::Int,
                NumKind::Float => MixT::GFloat,
            })
        }
        Expr::Binary { op: BinOp::Add | BinOp::Sub | BinOp::Mul, left, right, .. } => {
            let lk = infer_mixed_kind_indexed(left, binder, out, fns, user_fns, msigs)?;
            let rk = infer_mixed_kind_indexed(right, binder, out, fns, user_fns, msigs)?;
            mix_combine(lk, rk)
        }
        // `/` promotes BOTH operands in BOTH engines (even `Int / Int` is a float divide,
        // `10 / 2 == 5.0`), so unlike `+ - *` it is safe for ANY operand mix — including an
        // unpromoted value scalar, which is precisely the promotion the interpreter also
        // performs at this node. Result is a genuine float. A zero divisor poisons
        // (`body_raises` counts every `/`, so a dividing kernel always carries the
        // poison accumulator `gen_value_typed`'s Div arm ORs into).
        Expr::Binary { op: BinOp::Div, left, right, .. } => {
            infer_mixed_kind_indexed(left, binder, out, fns, user_fns, msigs)?;
            infer_mixed_kind_indexed(right, binder, out, fns, user_fns, msigs)?;
            Some(MixT::GFloat)
        }
        Expr::Binary {
            op: op @ (BinOp::Mod | BinOp::FloorDiv | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr),
            left,
            right,
            ..
        } => {
            let op_ok = match op {
                BinOp::Mod | BinOp::FloorDiv => matches!(**right, Expr::Int(n) if n > 0),
                BinOp::Shl | BinOp::Shr => matches!(**right, Expr::Int(n) if (0..=63).contains(&n)),
                _ => true,
            };
            if !op_ok {
                return None;
            }
            let lk = infer_mixed_kind_indexed(left, binder, out, fns, user_fns, msigs)?;
            let rk = infer_mixed_kind_indexed(right, binder, out, fns, user_fns, msigs)?;
            // The i64-closed ops require both operands `Int` — a genuine float or a value
            // scalar is not even valid Helix here (`x[i] % 3` on an f64 array is a type error).
            if lk == MixT::Int && rk == MixT::Int {
                Some(MixT::Int)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn value_eligible_cap(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, caps: &mut Vec<String>) -> bool {
    match e {
        Expr::Int(_) => true,
        // Float literals need the (dormant) f64 specialization; not this i64 kernel.
        Expr::Float(_) => false,
        Expr::Ident { name, .. } => {
            if locals.contains(name.as_str()) {
                true
            } else {
                // A free variable → a captured value. Record once, in first-appearance
                // order, so the codegen's `caps[i]` and the VM's load order agree.
                if !caps.iter().any(|c| c == name) {
                    caps.push(name.clone());
                }
                true
            }
        }
        Expr::Binary { op, left, right, .. } => {
            let op_ok = match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => true,
                // `%` only by a positive integer constant (total `rem_euclid`, no `%0`).
                BinOp::Mod => matches!(**right, Expr::Int(n) if n > 0),
                // Bitwise ops are unconditionally i64-closed (this is the i64 kernel).
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => true,
                // `<<`/`>>` only by an in-range constant (0..=63); `//` only by a
                // positive constant — same safe subset as `value_eligible` above.
                BinOp::Shl | BinOp::Shr => matches!(**right, Expr::Int(n) if (0..=63).contains(&n)),
                BinOp::FloorDiv => matches!(**right, Expr::Int(n) if n > 0),
                _ => false, // `/` excluded: not i64-closed; native fdiv diverges on /0
            };
            op_ok
                && value_eligible_cap(left, eligible, locals, caps)
                && value_eligible_cap(right, eligible, locals, caps)
        }
        // Unary negation, admitted for exactly the reason `value_eligible` (the
        // capture-free twin) already admits it: `gen_value`'s `Neg` arm lowers it to
        // `ineg`, which wraps precisely like the interpreter's `wrapping_neg`. Nothing in
        // codegen changes — this gate was simply the only one of the pair that had not
        // been taught the operator.
        //
        // Its absence made the IDIOMATIC spelling lose to the clumsy one, which is the
        // defect signature this project hunts. At 8M elements, bit-identical results:
        //     xs.map(-it)        0.43s   vs   xs.map(0 - it)      0.05s
        //     xs.map(-(it + 1))  0.48s   vs   xs.map((0 - it) - 1) 0.06s
        Expr::Unary { op: UnOp::Neg, expr, .. } => value_eligible_cap(expr, eligible, locals, caps),
        Expr::Call { name, args, .. } => {
            eligible.contains(name.as_str())
                && jit_builtin_arity_ok(name, args.len())
                && args.iter().all(|a| value_eligible_cap(a, eligible, locals, caps))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            cond_eligible_cap(cond, eligible, locals, caps)
                && value_eligible_cap(then_branch, eligible, locals, caps)
                && value_eligible_cap(else_branch, eligible, locals, caps)
        }
        Expr::Let { bindings, body, .. } => {
            let mut locals2 = locals.clone();
            for (n, v) in bindings {
                if !value_eligible_cap(v, eligible, &locals2, caps) {
                    return false;
                }
                locals2.insert(n.as_str());
            }
            value_eligible_cap(body, eligible, &locals2, caps)
        }
        _ => false,
    }
}

fn cond_eligible_cap(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, caps: &mut Vec<String>) -> bool {
    match e {
        // `and`/`or` in condition position (see `cond_eligible`); recurse through the
        // capture-collecting twin so captured names inside the operands are still found.
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible_cap(left, eligible, locals, caps)
                && cond_eligible_cap(right, eligible, locals, caps)
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne)
                && value_eligible_cap(left, eligible, locals, caps)
                && value_eligible_cap(right, eligible, locals, caps)
        }
        _ => false,
    }
}

/// True if a `filter`/`where` predicate is a pure `i64` comparison over its binder
/// (`it > 5`, `it % 2 == 0`, `is_even(it)`, …), calling only `fns`.
/// Returns the ordered captures, or `None` if the predicate is ineligible.
///
/// A `filter` predicate may CAPTURE free `i64` variables, exactly as a `map` body may — each
/// is passed to the kernel as a loop-invariant `caps[i]` and proven `Int` at dispatch. Without
/// this, `xs.filter(it % k == 0)` fell to the bytecode loop while the identical
/// `xs.filter(it % 7 == 0)` ran natively: measured 0.66s against 0.01s over 10M elements, the
/// same "swap a literal for a variable" cliff the map path had.
///
/// The two FUSED call sites require an EMPTY list: a fused pipeline has no caps mechanism, so
/// a capturing predicate must decline there and be handled by this standalone kernel instead.
pub fn filter_kernel_eligible(
    body: &Expr,
    binder: &str,
    fns: &HashSet<&str>,
) -> Option<Vec<Capture>> {
    let mut locals: HashSet<&str> = HashSet::new();
    locals.insert(binder);
    let mut names: Vec<String> = Vec::new();
    if cond_eligible_cap(body, fns, &locals, &mut names) && names.len() <= MAX_CAPTURES {
        Some(names.into_iter().map(|name| Capture { name, kind: CaptureKind::Scalar }).collect())
    } else {
        None
    }
}

/// Like [`filter_kernel_eligible`] but for a **`Floats`-source** predicate: comparisons
/// over the [`F64Proof`] expression subset, combined with `and`/`or`. Each comparison
/// needs at least one PROVEN-Float side — two `Promotable` sides would be the
/// interpreter's exact i64 comparison (`k1 < k2` on Int captures), which f64 cannot
/// reproduce above 2^53.
///
/// NaN is handled at RUN time, deliberately not here. The interpreter RAISES on a NaN
/// operand in an ordering comparison ("cannot compare these values") and is IEEE for
/// `==`/`!=`; the kernel therefore accumulates an `Unordered` flag per ordering
/// comparison (see [`gen_cond`]) and returns -1, and the dispatch falls back to the
/// bytecode loop for the exact error at the exact element. That covers NaN produced
/// INSIDE the predicate too (`it - it < 1.0` over an `inf` element), which no source
/// pre-scan could see — and costs nothing on clean data.
pub fn filter_kernel_eligible_f64(
    body: &Expr,
    binder: &str,
    user_fns: &HashSet<&str>,
) -> Option<Vec<Capture>> {
    let mut names: Vec<String> = Vec::new();
    let mut uses_binder = false;
    if cond_eligible_f64(body, binder, &mut names, &mut uses_binder, user_fns)
        && names.len() <= MAX_CAPTURES
    {
        Some(
            names
                .into_iter()
                .map(|name| Capture { name, kind: CaptureKind::Scalar })
                .collect(),
        )
    } else {
        None
    }
}

fn cond_eligible_f64(
    e: &Expr,
    binder: &str,
    caps: &mut Vec<String>,
    uses_binder: &mut bool,
    user_fns: &HashSet<&str>,
) -> bool {
    match e {
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible_f64(left, binder, caps, uses_binder, user_fns)
                && cond_eligible_f64(right, binder, caps, uses_binder, user_fns)
        }
        Expr::Binary {
            op: BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne,
            left,
            right,
            ..
        } => {
            let l = f64_body_eligible(left, binder, caps, uses_binder, user_fns);
            let r = f64_body_eligible(right, binder, caps, uses_binder, user_fns);
            // At least one side proven Float: the interpreter then promotes the other
            // side AT the comparison, exactly as the marshal's `as_f64` does.
            matches!(
                (l, r),
                (Some(F64Proof::Float), Some(_)) | (Some(_), Some(F64Proof::Float))
            )
        }
        _ => false,
    }
}

pub(crate) fn eligible_set<'a>(funcs: &[FnDef<'a>], kind: NumKind) -> HashSet<&'a str> {
    // Exclude every function on a recursion *cycle* — directly self-recursive OR
    // mutually recursive. A JIT'd function recurses on the native stack with no
    // depth guard, so unbounded recursion (a missing base case) would overflow the
    // native stack and crash the process instead of raising a clean, catchable
    // error. This is a transitive call-graph check, not just a direct self-call
    // test: the JIT's memory safety must NOT silently depend on the front-end's
    // define-before-use rule — a front-end policy that could change (see
    // `recursive_funcs`). It since DID change: two-pass bytecode registration made
    // mutual recursion representable, and this check absorbed it with no edit. The
    // property is pinned by `unbounded_mutual_recursion_raises_instead_of_crashing`.
    // Recursive functions run on the depth-guarded VM (or are memoized) instead —
    // EXCEPT directly tail-self-recursive ones (`tail_loopable_set`), which lower to
    // native LOOPS (parameter rebind + jump, no stack growth), so the native-stack
    // hazard above does not apply to them.
    let recursive = recursive_funcs(funcs);
    let tail_loop = tail_loopable_set(funcs);
    let mut eligible: HashSet<&str> = funcs
        .iter()
        .filter(|f| {
            f.params.len() <= MAX_ARITY
                && (!recursive.contains(f.name) || tail_loop.contains(f.name))
        })
        .map(|f| f.name)
        .collect();
    // Pure scalar builtins the kernel codegen can emit inline (`abs`/`min`/`max`) — usable
    // from a kernel body just like an eligible user function, EXCEPT when a user function
    // of the same name shadows the builtin (then the call must dispatch to the user fn, so
    // the JIT must not treat it as the builtin). Added before the fixpoint so user
    // functions that call them are themselves eligible. This is the single source the
    // compiler (`int_eligible_fns`) and the JIT build both read, so they always agree.
    for (name, _) in JIT_SCALAR_BUILTINS {
        if !funcs.iter().any(|f| f.name == *name) {
            eligible.insert(name);
        }
    }
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
pub(crate) fn recursive_funcs<'a>(funcs: &[FnDef<'a>]) -> HashSet<&'a str> {
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
        Expr::Let { bindings, body, .. } => {
            bindings.iter().any(|(_, v)| body_calls(v, name))
                || body_calls(body, name)
        }
        // A `match` is i64-eligible (`match_eligible`), so a call can hide in its
        // scrutinee, a guard, or an arm body — all must be traversed. Without this arm a
        // self-call inside a match evaded `recursive_funcs`, and the function was JIT'd
        // with unguarded NATIVE recursion (deep input = native stack overflow = process
        // crash, where the VM raises its clean depth error).
        Expr::Match { scrutinee, arms, .. } => {
            body_calls(scrutinee, name)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(|g| body_calls(g, name))
                        || body_calls(&a.body, name)
                })
        }
        _ => false,
    }
}

/// True iff every call to `self_name` in `e` sits in **tail position** — reachable from
/// the function root only through `if` branches and `let` bodies — and passes exactly
/// `arity` arguments. Such a call is a loop back-edge, not real recursion: the JIT lowers
/// it by rebinding the parameters and jumping to the loop header ([`gen_tail`]), growing
/// no native stack — precisely the VM's `TailCallFn` frame-reuse semantics (the tail-call
/// peephole in `bytecode.rs`). Conditions, `let` binding values, and the tail call's own
/// arguments must be self-free: a self-call there needs a real activation record.
fn self_calls_tail_only(e: &Expr, self_name: &str, arity: usize) -> bool {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => {
            !body_calls(cond, self_name)
                && self_calls_tail_only(then_branch, self_name, arity)
                && self_calls_tail_only(else_branch, self_name, arity)
        }
        Expr::Let { bindings, body, .. } => {
            bindings.iter().all(|(_, v)| !body_calls(v, self_name))
                && self_calls_tail_only(body, self_name, arity)
        }
        Expr::Call { name, args, .. } if name == self_name => {
            args.len() == arity && args.iter().all(|a| !body_calls(a, self_name))
        }
        other => !body_calls(other, self_name),
    }
}

/// True iff `funcs[i]` lies on a call cycle of length ≥ 2 — it can reach itself through
/// some *other* function. The direct self-edge is deliberately ignored: a purely
/// tail-self-recursive function lowers to a native loop, but one on a mutual cycle would
/// still recurse natively through its partner, so it must stay excluded. (Mutual
/// recursion is unrepresentable under today's define-before-use rule; like
/// `recursive_funcs`, this check refuses to depend on that front-end policy.)
fn on_mutual_cycle(i: usize, funcs: &[FnDef]) -> bool {
    let n = funcs.len();
    let mut seen = vec![false; n];
    // First hop: every callee EXCEPT the direct self-edge.
    let mut stack: Vec<usize> =
        (0..n).filter(|&j| j != i && body_calls(funcs[i].body, funcs[j].name)).collect();
    while let Some(u) = stack.pop() {
        if u == i {
            return true;
        }
        if !seen[u] {
            seen[u] = true;
            stack.extend((0..n).filter(|&j| body_calls(funcs[u].body, funcs[j].name)));
        }
    }
    false
}

/// The directly tail-self-recursive functions the JIT lowers as native **loops** instead
/// of excluding for recursion: every self-call is in tail position with the right arity
/// (`self_calls_tail_only`) and the function is on no mutual cycle (`on_mutual_cycle`).
/// The back-edge grows no native stack, so the unguarded-recursion hazard that excludes
/// recursive functions does not apply; a missing base case spins exactly like the VM's
/// `TailCallFn` loop would (identical semantics), it does not overflow. Pure and
/// deterministic — `eligible_set` (read by both the bytecode compiler and the JIT build)
/// and `build`'s codegen branch call it identically, so all sites always agree.
/// The free identifiers of `e` — the names it reads that it does not itself bind.
///
/// Correct by construction over exactly the forms [`value_eligible`] ACCEPTS, which is
/// all that is needed: its catch-all is `false`, so for any other expression the body is
/// ineligible and its capture list is never consulted. Within that set, binders occur in
/// only two places — `Let` bindings and `Match` arm patterns — and both are handled here
/// the same way `value_eligible` handles them (a `Let` binding is in scope for the
/// bindings after it and for the body; an arm's pattern names are in scope for its guard
/// and body). First-appearance order, no duplicates: the order IS the parameter order of
/// the compiled specialization, so it must be deterministic.
fn free_idents<'a>(e: &'a Expr, bound: &HashSet<&'a str>, out: &mut Vec<&'a str>) {
    match e {
        Expr::Ident { name, .. } => {
            let n = name.as_str();
            if !bound.contains(n) && !out.contains(&n) {
                out.push(n);
            }
        }
        Expr::Binary { left, right, .. } => {
            free_idents(left, bound, out);
            free_idents(right, bound, out);
        }
        Expr::Unary { expr, .. } => free_idents(expr, bound, out),
        // The callee NAME is a function, not a value — only the arguments are reads.
        Expr::Call { args, .. } => {
            for a in args {
                free_idents(a, bound, out);
            }
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            free_idents(cond, bound, out);
            free_idents(then_branch, bound, out);
            free_idents(else_branch, bound, out);
        }
        Expr::Let { bindings, body, .. } => {
            let mut bound2 = bound.clone();
            for (n, v) in bindings {
                free_idents(v, &bound2, out);
                bound2.insert(n.as_str());
            }
            free_idents(body, &bound2, out);
        }
        Expr::Match { scrutinee, arms, .. } => {
            free_idents(scrutinee, bound, out);
            for arm in arms {
                let mut bound2 = bound.clone();
                // Exactly what `match_eligible` binds: a single `Bind` pattern. Literal,
                // `Or` and wildcard patterns bind no names, and any richer pattern makes
                // the arm ineligible there, so nothing else can be in scope here.
                if let crate::ast::Pattern::Bind(n) = &arm.pattern {
                    bound2.insert(n.as_str());
                }
                if let Some(g) = &arm.guard {
                    free_idents(g, &bound2, out);
                }
                free_idents(&arm.body, &bound2, out);
            }
        }
        // Literals bind and read nothing; anything else makes the body ineligible.
        _ => {}
    }
}

/// Tail-self-recursive functions that would be `i64`-eligible IF the globals they read
/// were parameters, together with those globals in parameter order.
///
/// This is the loop counterpart of the capture work the map/filter/reduce kernels got:
/// `value_eligible`'s `Ident` arm admits only parameters, so ONE global read anywhere in
/// a function — condition or body, it made no difference — dropped the entire loop to the
/// bytecode VM. Measured at 10M iterations: 0.01s compiled against 0.80s interpreted, an
/// 80x penalty for naming a bound instead of passing it.
///
/// Deliberately ADDITIVE. `eligible_set` is untouched, so `int_eligible_fns` — which the
/// bytecode compiler reads to decide whether a kernel may CALL a user function — still
/// describes exactly the functions whose ABI is `params.len()` arguments. A capture-taking
/// function is compiled under its own entry point that only the VM's `CallFn` dispatches
/// to, so no kernel can call it with the wrong signature. Its own calls still resolve
/// against `eligible`, i.e. only to capture-free functions, so there is no transitive
/// capture set to close over.
pub(crate) fn tail_loop_captures<'a>(
    funcs: &[FnDef<'a>],
    eligible: &HashSet<&'a str>,
    kind: NumKind,
) -> Vec<(&'a str, Vec<&'a str>)> {
    let tail_loop = tail_loopable_set(funcs);
    let mut out = Vec::new();
    for f in funcs {
        // Only loops, and only ones the plain analysis already rejected — a function
        // that compiled without captures keeps its existing, cheaper entry point.
        if !tail_loop.contains(f.name) || eligible.contains(f.name) || f.params.len() > MAX_ARITY {
            continue;
        }
        let params: HashSet<&str> = f.params.iter().map(|(n, _)| n.as_str()).collect();
        let mut caps = Vec::new();
        free_idents(f.body, &params, &mut caps);
        // No free names → it was rejected for some other reason (a `/`, a Float, an
        // ineligible callee), and captures cannot rescue it.
        if caps.is_empty() || caps.len() > MAX_CAPTURES {
            continue;
        }
        // A capture that names a user FUNCTION is not a global read — `free_idents` never
        // records a callee, but a function used as a value would be, and it is not an i64.
        if caps.iter().any(|c| funcs.iter().any(|g| g.name == *c)) {
            continue;
        }
        // Now ask the REAL predicate whether the body is eligible once those names are
        // treated as parameters. Same function the capture-free path uses, so the two
        // cannot drift apart on what `i64`-closed means.
        let mut widened = params.clone();
        for c in &caps {
            widened.insert(c);
        }
        // The SELF-call must be treated as eligible while re-checking. `eligible` cannot
        // contain `f` — `f` is here precisely because it was rejected — but `gen_tail`
        // lowers a tail self-call to a parameter rebind and a jump, never to a call
        // instruction, so it needs no entry in `fn_ids` and no compiled callee. Every
        // self-call is in tail position by `tail_loopable_set`, which is what makes that
        // lowering total.
        let mut callable = eligible.clone();
        callable.insert(f.name);
        if value_eligible(f.body, &callable, &widened, kind) {
            out.push((f.name, caps));
        }
    }
    out
}

pub(crate) fn tail_loopable_set<'a>(funcs: &[FnDef<'a>]) -> HashSet<&'a str> {
    funcs
        .iter()
        .enumerate()
        .filter(|(_, f)| body_calls(f.body, f.name))
        .filter(|(i, _)| !on_mutual_cycle(*i, funcs))
        .filter(|(_, f)| self_calls_tail_only(f.body, f.name, f.params.len()))
        .map(|(_, f)| f.name)
        .collect()
}

/// A mixed specialization visible to OTHER mixed bodies (declared before any body is
/// defined, so `escape` can call `step`): its Cranelift id, per-param kinds, and result
/// kind (`Int` placeholder for a body whose every path re-loops).
/// A mixed specialization's numeric SIGNATURE — parameter kinds and return kind, with no
/// codegen identity attached. Deliberately id-free so the inference that produces it
/// ([`mixed_fn_sigs`]) needs no `JITModule` and can therefore run at BYTECODE-COMPILE time,
/// where the decision to emit a kernel guard is made. `build` keeps the `FuncId`s in a
/// parallel map (`mixed_ids`), keyed by the same names.
#[derive(Clone)]
pub(crate) struct MixedSig {
    pub(crate)params: Vec<NumKind>,
    pub(crate)ret: NumKind,
}

/// Every user function that gets a MIXED specialization, with its parameter kinds and
/// return kind. Pure over the AST — the twin of [`int_eligible_fns`], and the table the
/// bytecode compiler needs in order to type a call to a `Float`-parameter function inside a
/// map body (it knows only NAMES otherwise). Computed identically here and inside `build`,
/// so the compile-time guard decision matches what the JIT will actually compile.
///
/// Program order matters and is preserved: each accepted signature is visible to LATER
/// functions, so `fn escape(...) = step(...)` sees `step` (the define-before-use rule
/// guarantees callees precede callers).
pub type MixedSigTable = std::collections::HashMap<String, (Vec<NumKind>, NumKind)>;

pub fn mixed_fn_sigs(program: &[Stmt]) -> MixedSigTable {
    let funcs: Vec<FnDef> = program
        .iter()
        .filter_map(|s| match s {
            Stmt::Func { name, params, body, .. } => Some(FnDef { name, params, body }),
            _ => None,
        })
        .collect();
    let int_eligible = eligible_set(&funcs, NumKind::Int);
    let tail_loop = tail_loopable_set(&funcs);
    let recursive = recursive_funcs(&funcs);
    let user_fns: HashSet<&str> = funcs.iter().map(|f| f.name).collect();
    let mut sigs: HashMap<&str, MixedSig> = HashMap::new();
    let mut out = std::collections::HashMap::new();
    for f in &funcs {
        let Some((_, params, ret)) =
            mixed_fn_sig(f, &tail_loop, &recursive, &int_eligible, &sigs, &user_fns)
        else {
            continue;
        };
        // A body whose every path re-loops never returns; `Int` is the same placeholder
        // `build` uses, so the two tables agree.
        let ret = ret.unwrap_or(NumKind::Int);
        sigs.insert(f.name, MixedSig { params: params.clone(), ret });
        out.insert(f.name.to_string(), (params, ret));
    }
    out
}

/// Bottom-up kind of an expression over a **typed environment** (parameter and `let`
/// binder kinds), or `None` if anything falls outside the mixed-eligible shape. The
/// env-generalization of [`infer_mixed_kind`] (same operator/builtin/promotion rules,
/// mirrored EXACTLY by [`gen_value_env`]): `+`/`-`/`*` promote `Int` operands to `f64`
/// when the other side is `Float` (the interpreter's numeric promotion); `%`/`//`/
/// bitwise/const-shifts stay `Int`-only under `value_eligible`'s constant constraints;
/// `sqrt` is always `Float`, `abs` preserves, `min`/`max` need same-kind operands.
/// `/` is admitted ONLY with a nonzero `Float`-literal divisor (both sides promote to
/// f64; the interpreter's `/` always yields Float, and a literal divisor can never
/// raise its /0 error, so native `fdiv` is bit-exact with no poison obligation).
/// Calls dispatch in priority order: a MIXED sibling (`sigs` — arg kinds must EQUAL
/// its param kinds), then any OTHER user function → ineligible (never silently treat a
/// user-shadowed `sqrt` as the builtin — the shadowing hole the map path already
/// guards with `user_fns`), then the inline builtins. No `let`/`if` in VALUE position
/// (tail positions handle those).
fn infer_typed_env(
    e: &Expr,
    env: &HashMap<&str, NumKind>,
    sigs: &HashMap<&str, MixedSig>,
    user_fns: &HashSet<&str>,
) -> Option<NumKind> {
    match e {
        Expr::Int(_) => Some(NumKind::Int),
        Expr::Float(_) => Some(NumKind::Float),
        Expr::Ident { name, .. } => env.get(name.as_str()).copied(),
        Expr::Binary { op, left, right, .. } => {
            let lk = infer_typed_env(left, env, sigs, user_fns)?;
            let rk = infer_typed_env(right, env, sigs, user_fns)?;
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => {
                    Some(if lk == NumKind::Float || rk == NumKind::Float {
                        NumKind::Float
                    } else {
                        NumKind::Int
                    })
                }
                // `/` is always float division and always yields Float, for ANY eligible
                // divisor. This was literal-only (`Expr::Float(d) if d != 0.0`) — and that
                // single restriction was k2's entire 5.3×: `row`'s `2.7 / to_float(g)`
                // declined the whole function to the VM, costing ~250 ns of dispatch per
                // pixel around a native `step` (0.39s against 0.07s with the reciprocal
                // hoisted). A zero divisor now bails IMMEDIATELY to the poison block
                // (`gen_value_env`'s Div arm) — the same rule as the NaN-compare bail, and
                // for the same reason: a tail loop can be infinite, so the interpreter's
                // `/0` error cannot wait for an accumulate-and-store. The VM then discards
                // the result and re-runs on bytecode, raising the exact error.
                BinOp::Div => Some(NumKind::Float),
                // Any `Int` divisor, literal or not. A zero divisor — and the
                // `(i64::MIN, -1)` pair, which does not raise but WRAPS where native
                // `srem`/`sdiv` would trap — bail to the poison block, exactly like the
                // `/` arm below and for the same reason. Naming a modulus used to cost
                // 17-110x: `MOD = 1000000007` then `% MOD` declined the whole enclosing
                // kernel, and a divisor arriving from data had no fast spelling at all.
                BinOp::Mod | BinOp::FloorDiv => {
                    (lk == NumKind::Int && rk == NumKind::Int).then_some(NumKind::Int)
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    (lk == NumKind::Int && rk == NumKind::Int).then_some(NumKind::Int)
                }
                // Any `Int` shift count; one outside `0..=63` bails, since the
                // interpreter raises there and a native shift is undefined.
                BinOp::Shl | BinOp::Shr => {
                    (lk == NumKind::Int && rk == NumKind::Int).then_some(NumKind::Int)
                }
                _ => None,
            }
        }
        Expr::Unary { op: UnOp::Neg, expr, .. } => infer_typed_env(expr, env, sigs, user_fns),
        Expr::Call { name, args, .. } => {
            if let Some(sig) = sigs.get(name.as_str()) {
                // A mixed sibling: strict per-param kind equality (no promotion — the
                // callee's specialization is compiled for exactly these kinds).
                if args.len() != sig.params.len() {
                    return None;
                }
                for (a, &k) in args.iter().zip(&sig.params) {
                    if infer_typed_env(a, env, sigs, user_fns)? != k {
                        return None;
                    }
                }
                return Some(sig.ret);
            }
            if user_fns.contains(name.as_str()) {
                // A user function without a mixed form (or shadowing a builtin name):
                // not lowerable — never treat it as the inline builtin.
                return None;
            }
            match (name.as_str(), args.len()) {
                ("sqrt", 1) => {
                    infer_typed_env(&args[0], env, sigs, user_fns)?;
                    Some(NumKind::Float)
                }
                // `to_float` is the explicit Int->Float conversion. Like `sqrt` it always yields a
                // float, and the typed codegen emits exactly the `fcvt_from_sint` promotion it
                // already emits for `sqrt`'s argument -- so this is `sqrt` with nothing applied after.
                ("to_float", 1) => {
                    infer_typed_env(&args[0], env, sigs, user_fns)?;
                    Some(NumKind::Float)
                }
                ("abs", 1) => infer_typed_env(&args[0], env, sigs, user_fns),
                // `to_int` and `sign` always yield `Int` and NEVER raise, which is what makes them safe
                // to lower with no bail machinery: `to_int` SATURATES (NaN -> 0, +-inf -> i64::MAX/MIN,
                // exactly Rust's `as i64` and Cranelift's `fcvt_to_sint_sat`), and `sign` is two
                // comparisons whose NaN case falls through to 0 -- matching the interpreter, which
                // returns 0 for NaN rather than propagating it. Contrast floor/ceil/round/trunc, which
                // RAISE when the result leaves i64 range and therefore still need a poison path.
                ("to_int" | "sign", 1) => {
                    infer_typed_env(&args[0], env, sigs, user_fns)?;
                    Some(NumKind::Int)
                }
                ("min" | "max", 2) => {
                    let ka = infer_typed_env(&args[0], env, sigs, user_fns)?;
                    let kb = infer_typed_env(&args[1], env, sigs, user_fns)?;
                    (ka == kb).then_some(ka)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// True iff `e` is a mixed-eligible condition: `and`/`or` over comparisons whose two
/// sides infer to the SAME kind (an `Int`-vs-`Float` comparison is rejected — its
/// promotion semantics past 2^53 are not provably identical to the interpreter's).
/// Mirrored exactly by [`gen_cond_env`].
fn cond_typed_ok(
    e: &Expr,
    env: &HashMap<&str, NumKind>,
    sigs: &HashMap<&str, MixedSig>,
    user_fns: &HashSet<&str>,
) -> bool {
    match e {
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_typed_ok(left, env, sigs, user_fns) && cond_typed_ok(right, env, sigs, user_fns)
        }
        Expr::Binary { op, left, right, .. } => {
            matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne)
                && match (
                    infer_typed_env(left, env, sigs, user_fns),
                    infer_typed_env(right, env, sigs, user_fns),
                ) {
                    (Some(lk), Some(rk)) => lk == rk,
                    _ => false,
                }
        }
        _ => false,
    }
}

/// The result kind of a mixed tail-recursive body, walking exactly the tail structure
/// [`self_calls_tail_only`] admitted. Returns `None` = ineligible; `Some(None)` = every
/// path re-loops (the body never returns a value); `Some(Some(k))` = all value positions
/// agree on kind `k`. Each tail self-call's argument kinds must EQUAL the annotated
/// parameter kinds — the loop then preserves every parameter's type by induction, which
/// is what makes one static specialization faithful to the dynamically-typed interpreter.
fn mixed_tail_ret_kind<'a>(
    e: &'a Expr,
    env: &mut HashMap<&'a str, NumKind>,
    self_name: &str,
    param_kinds: &[NumKind],
    sigs: &HashMap<&str, MixedSig>,
    user_fns: &HashSet<&str>,
) -> Option<Option<NumKind>> {
    match e {
        Expr::If { cond, then_branch, else_branch, .. } => {
            if !cond_typed_ok(cond, env, sigs, user_fns) {
                return None;
            }
            let a = mixed_tail_ret_kind(then_branch, env, self_name, param_kinds, sigs, user_fns)?;
            let b = mixed_tail_ret_kind(else_branch, env, self_name, param_kinds, sigs, user_fns)?;
            match (a, b) {
                (None, x) | (x, None) => Some(x),
                (Some(k1), Some(k2)) if k1 == k2 => Some(Some(k1)),
                _ => None,
            }
        }
        Expr::Let { bindings, body, .. } => {
            let mut saved: Vec<(&'a str, Option<NumKind>)> = Vec::new();
            for (n, v) in bindings {
                let k = infer_typed_env(v, env, sigs, user_fns)?;
                saved.push((n.as_str(), env.insert(n.as_str(), k)));
            }
            let r = mixed_tail_ret_kind(body, env, self_name, param_kinds, sigs, user_fns);
            for (n, old) in saved.into_iter().rev() {
                match old {
                    Some(o) => {
                        env.insert(n, o);
                    }
                    None => {
                        env.remove(n);
                    }
                }
            }
            r
        }
        Expr::Call { name, args, .. } if name == self_name => {
            if args.len() != param_kinds.len() {
                return None;
            }
            for (a, &k) in args.iter().zip(param_kinds) {
                if infer_typed_env(a, env, sigs, user_fns)? != k {
                    return None;
                }
            }
            Some(None)
        }
        other => infer_typed_env(other, env, sigs, user_fns).map(Some),
    }
}

/// The mixed-specialization signature of a tail-loopable function, or `None` if it has
/// no such form: every parameter carries an explicit `Int`/`Float` annotation (the
/// contract that makes one static specialization honest — the VM dispatches it only
/// when the actual argument types match), and the body types consistently under those
/// kinds. An ALL-`Int` signature is admitted too, when the plain i64 path did not
/// already claim the function — that is the "Int state, float intermediates" shape
/// (e.g. an xorshift Monte-Carlo loop: i64 RNG state threaded through the tail calls,
/// f64 math inside each iteration, Int result), which `value_eligible` rejects for its
/// float literals. Returns (float bitmask, per-param kinds, result kind — `None` when
/// every path re-loops).
/// Does this subtree force `f64` evaluation? A `Float` literal, a float-returning builtin, a
/// division (never `i64`-closed — see `value_eligible_cap`), or a parameter already known to be
/// `Float`. Used only to PROPOSE kinds in [`infer_param_kinds`]; the proposal is then validated,
/// so a wrong answer here costs a missed specialization, never a wrong one.
fn subtree_forces_float(e: &Expr, float_params: &HashSet<&str>) -> bool {
    match e {
        Expr::Float(_) => true,
        Expr::Int(_) => false,
        Expr::Ident { name, .. } => float_params.contains(name.as_str()),
        Expr::Call { name, args, .. } => {
            matches!(name.as_str(), "sqrt" | "to_float")
                || args.iter().any(|a| subtree_forces_float(a, float_params))
        }
        Expr::Binary { op: BinOp::Div, .. } => true,
        Expr::Binary { left, right, .. } => {
            subtree_forces_float(left, float_params) || subtree_forces_float(right, float_params)
        }
        Expr::Unary { expr, .. } => subtree_forces_float(expr, float_params),
        Expr::If { cond, then_branch, else_branch, .. } => {
            subtree_forces_float(cond, float_params)
                || subtree_forces_float(then_branch, float_params)
                || subtree_forces_float(else_branch, float_params)
        }
        _ => false,
    }
}

/// Collect the parameter names occurring anywhere in `e`.
fn params_in<'a>(e: &'a Expr, params: &HashSet<&'a str>, out: &mut HashSet<&'a str>) {
    match e {
        Expr::Ident { name, .. } => {
            if params.contains(name.as_str()) {
                out.insert(name.as_str());
            }
        }
        Expr::Binary { left, right, .. } => {
            params_in(left, params, out);
            params_in(right, params, out);
        }
        Expr::Unary { expr, .. } => params_in(expr, params, out),
        Expr::Call { args, .. } => args.iter().for_each(|a| params_in(a, params, out)),
        Expr::If { cond, then_branch, else_branch, .. } => {
            params_in(cond, params, out);
            params_in(then_branch, params, out);
            params_in(else_branch, params, out);
        }
        Expr::Index { recv, index, .. } => {
            params_in(recv, params, out);
            params_in(index, params, out);
        }
        _ => {}
    }
}

/// Walk `e` marking parameters `Float` (float taint) and `Int` (used by an `i64`-closed operator
/// or as an index). Returns `false` on a CONTRADICTION — a parameter with both kinds of evidence —
/// so the caller declines rather than guessing.
fn gather_kind_evidence<'a>(
    e: &'a Expr,
    self_name: &str,
    params: &HashSet<&'a str>,
    order: &[&'a str],
    float: &mut HashSet<&'a str>,
    int: &mut HashSet<&'a str>,
) -> bool {
    match e {
        // A self-call ties argument j to parameter j — the strongest signal in a
        // tail-recursive function, and the shape this exists for.
        Expr::Call { name, args, .. } if name == self_name && args.len() == order.len() => {
            for (j, a) in args.iter().enumerate() {
                if subtree_forces_float(a, float) {
                    float.insert(order[j]);
                }
                if !gather_kind_evidence(a, self_name, params, order, float, int) {
                    return false;
                }
            }
        }
        // `%`, `//`, bitwise and shifts are `i64`-closed: their operands are integers.
        Expr::Binary {
            op:
                BinOp::Mod
                | BinOp::FloorDiv
                | BinOp::BitAnd
                | BinOp::BitOr
                | BinOp::BitXor
                | BinOp::Shl
                | BinOp::Shr,
            left,
            right,
            ..
        } => {
            let mut here = HashSet::new();
            params_in(left, params, &mut here);
            params_in(right, params, &mut here);
            int.extend(here);
            if !gather_kind_evidence(left, self_name, params, order, float, int)
                || !gather_kind_evidence(right, self_name, params, order, float, int)
            {
                return false;
            }
        }
        // A COMPARISON ties its two sides to the same kind in practice — the loop-bound idiom
        // `i >= lim` is how a float counter's limit gets its type, and without this the limit
        // infers `Int`, the mask mismatches at dispatch, and the whole function silently falls
        // back (correct, but 60× slower). Same proposal-not-proof status as the rest.
        Expr::Binary {
            op: BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne,
            left,
            right,
            ..
        } => {
            if subtree_forces_float(left, float) || subtree_forces_float(right, float) {
                let mut here = HashSet::new();
                params_in(left, params, &mut here);
                params_in(right, params, &mut here);
                float.extend(here);
            }
            if !gather_kind_evidence(left, self_name, params, order, float, int)
                || !gather_kind_evidence(right, self_name, params, order, float, int)
            {
                return false;
            }
        }
        // `+ - * /` mixing a parameter with anything float-forcing makes that parameter float.
        Expr::Binary {
            op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div, left, right, ..
        } => {
            if subtree_forces_float(e, float) {
                let mut here = HashSet::new();
                params_in(left, params, &mut here);
                params_in(right, params, &mut here);
                float.extend(here);
            }
            if !gather_kind_evidence(left, self_name, params, order, float, int)
                || !gather_kind_evidence(right, self_name, params, order, float, int)
            {
                return false;
            }
        }
        // An index is an integer.
        Expr::Index { recv, index, .. } => {
            let mut here = HashSet::new();
            params_in(index, params, &mut here);
            int.extend(here);
            if !gather_kind_evidence(recv, self_name, params, order, float, int)
                || !gather_kind_evidence(index, self_name, params, order, float, int)
            {
                return false;
            }
        }
        Expr::Binary { left, right, .. } => {
            if !gather_kind_evidence(left, self_name, params, order, float, int)
                || !gather_kind_evidence(right, self_name, params, order, float, int)
            {
                return false;
            }
        }
        Expr::Unary { expr, .. } => {
            if !gather_kind_evidence(expr, self_name, params, order, float, int) {
                return false;
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                if !gather_kind_evidence(a, self_name, params, order, float, int) {
                    return false;
                }
            }
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            for b in [cond, then_branch, else_branch] {
                if !gather_kind_evidence(b, self_name, params, order, float, int) {
                    return false;
                }
            }
        }
        _ => {}
    }
    // A parameter cannot be both an `i64`-closed operand and float-tainted.
    !float.iter().any(|p| int.contains(p))
}

/// Propose a numeric kind per parameter when the source did not annotate them.
///
/// WHY THIS EXISTS. The mixed specialization is the sound successor to the removed blanket-`f64`
/// function spec (see the note above `let kind = NumKind::Int` in [`build`]): it tracks each
/// parameter's kind AND derives the exact return kind, so it cannot diverge from the interpreter
/// on result type the way blanket `f64` codegen did. But it was reachable only through explicit
/// `: Int` / `: Float` annotations — so an ORDINARY numeric loop, whose natural shape is float
/// state plus an integer counter, never reached native code at all. Measured: `fn spin(zr, zi, i,
/// n)` ran 0.72s where the identical annotated body ran 0.01s, a **72×** cliff with `JIT ≈ NOJIT`
/// (i.e. it never compiled), and the same cliff hit all-`Float` recursion too.
///
/// WHY A PROPOSAL IS ENOUGH — this needs to be plausible, not sound, because two independent
/// validators already stand behind it:
/// 1. [`mixed_tail_ret_kind`] re-types the whole body under the proposed kinds and returns `None`
///    if anything fails to check, so a body that does not fit the proposal is never compiled.
/// 2. The VM re-tests every ARGUMENT's runtime type against `float_mask` before dispatching to
///    the specialization (`vm.rs`, `Op::CallFn`), so a specialization built on a wrong guess is
///    simply never called — the ordinary bytecode path runs and the result is unchanged.
///
/// So the cost of a bad proposal is a few microseconds of wasted JIT time, never a wrong answer.
/// A parameter with contradictory evidence (used both as an `i64`-closed operand and float-tainted)
/// declines the whole function rather than picking a side. Unresolved parameters default to `Int`;
/// if that makes the signature all-`Int`, [`mixed_fn_sig`]'s existing `int_eligible` check drops
/// it so the plain `i64` loop keeps the function.
fn infer_param_kinds<'a>(f: &'a FnDef) -> Option<Vec<NumKind>> {
    let order: Vec<&'a str> = f.params.iter().map(|(n, _)| n.as_str()).collect();
    let params: HashSet<&'a str> = order.iter().copied().collect();
    if params.len() != order.len() {
        return None; // duplicate parameter names — not a shape to reason about
    }
    let mut float: HashSet<&'a str> = HashSet::new();
    let mut int: HashSet<&'a str> = HashSet::new();
    // Seed from whatever WAS annotated, so a partly-annotated signature is honoured exactly.
    for (n, ann) in f.params {
        match ann {
            Some(TypeAnn::Float) => {
                float.insert(n.as_str());
            }
            Some(TypeAnn::Int) => {
                int.insert(n.as_str());
            }
            _ => {}
        }
    }
    // Float taint propagates (a param becomes Float, which makes its neighbours Float), so
    // iterate to a fixpoint. Bounded by the parameter count: each round either grows `float` or
    // stops.
    for _ in 0..=order.len() {
        let before = float.len() + int.len();
        if !gather_kind_evidence(f.body, f.name, &params, &order, &mut float, &mut int) {
            return None;
        }
        if float.len() + int.len() == before {
            break;
        }
    }
    Some(
        order
            .iter()
            .map(|n| if float.contains(n) { NumKind::Float } else { NumKind::Int })
            .collect(),
    )
}

pub(crate) fn mixed_fn_sig(
    f: &FnDef,
    tail_loop: &HashSet<&str>,
    recursive: &HashSet<&str>,
    int_eligible: &HashSet<&str>,
    sigs: &HashMap<&str, MixedSig>,
    user_fns: &HashSet<&str>,
) -> Option<(u16, Vec<NumKind>, Option<NumKind>)> {
    // Recursive functions qualify only in the tail-loopable shape; NON-recursive ones
    // compile straight-line with the same walker (no self-call arm ever fires) — the
    // `fn escape(px: Int, py: Int) = step(…)` wrapper shape.
    if recursive.contains(f.name) && !tail_loop.contains(f.name) {
        return None;
    }
    if f.params.is_empty() || f.params.len() > MAX_ARITY {
        return None;
    }
    // Annotations win where present; anything unannotated gets an INFERRED kind, so an ordinary
    // numeric loop (`fn spin(zr, zi, i, n)`) reaches this specialization instead of falling to
    // the per-element VM — a measured 72× cliff. The proposal is validated by
    // `mixed_tail_ret_kind` below and again by the VM's per-argument type test at dispatch, so a
    // wrong inference costs a never-used specialization, not a wrong result. See
    // [`infer_param_kinds`].
    let inferred = if f.params.iter().any(|(_, a)| a.is_none()) {
        Some(infer_param_kinds(f)?)
    } else {
        None
    };
    let mut kinds = Vec::with_capacity(f.params.len());
    let mut mask: u16 = 0;
    for (j, (_, ann)) in f.params.iter().enumerate() {
        let k = match ann {
            Some(TypeAnn::Int) => NumKind::Int,
            Some(TypeAnn::Float) => NumKind::Float,
            Some(_) => return None, // a non-numeric annotation is not this specialization's shape
            None => inferred.as_ref()?[j],
        };
        if matches!(k, NumKind::Float) {
            mask |= 1 << j;
        }
        kinds.push(k);
    }
    if mask == 0 && int_eligible.contains(f.name) {
        // THE GENERIC HELPER. `fn sq(x) = x * x` is the shape every library author writes,
        // and until now it got NO mixed specialization at all — so every FLOAT call site
        // silently declined and took the enclosing map down with it:
        //
        //     fn sq(x) = x * x            , Float call:  0.967s jit / 0.944s nojit — declines
        //     fn sq(x: Float) -> Float    , Float call:  0.019s                      52x
        //     fn sq(x) = x * x * 1.0      , Float call:  0.018s                     132x
        //     fn sq(x) = x * x            , Int   call:  0.026s                      30x
        //
        // The reasoning that used to end here — "the plain i64 loop already covers an
        // all-Int, i64-closed function, so a mixed duplicate would never be dispatched" — is
        // true about calling `sq` DIRECTLY, and false about a call to it from inside a
        // kernel. `infer_param_kinds` reads the function's OWN BODY only, never its call
        // sites, so a kind-agnostic body like `x * x` yields Int by default rather than by
        // evidence, and the Float reading was simply never built.
        //
        // The two specializations do not compete, because they live in DIFFERENT tables: the
        // i64 one in `fn_ids`, this one in `msigs`/`mixed_ids`. Emitting the Float reading
        // fills an empty slot rather than shadowing anything, and dispatch stays exact — the
        // VM type-tests every argument, so an `Int` argument still takes the i64 path and
        // still gets the interpreter's WRAPPING i64 arithmetic. That matters: `x * x` on
        // 2^53+1 is an exact wrapping multiply in the interpreter and a lossy f64 one here,
        // and it is the per-argument test, not this function, that keeps them apart.
        //
        // ONLY when every parameter is UNANNOTATED. A written `Int` is evidence; an absent
        // annotation is not, and promoting a partly-annotated signature would overrule
        // something the author actually said.
        //
        // A NAME THAT SHADOWS A BUILTIN IS NO LONGER EXCLUDED. It used to be, because
        // `mixed_fn_sigs` is derived from the whole AST and has no notion of definition
        // ORDER while the engines resolved in source order — so promoting `fn round(x) = 99`
        // applied the user's function to the call sites ABOVE it and printed
        // `[99, 99, 99, 99]` against `[1, 2, 3, 4]`. ADR 0027 removed the premise: a
        // top-level `fn` is file-scoped, so an order-blind analysis is now simply CORRECT
        // about these names rather than needing to be kept away from them. This is one of
        // the three guards that decision was taken to delete.
        if f.params.iter().all(|(_, a)| a.is_none()) {
            let fkinds = vec![NumKind::Float; f.params.len()];
            let mut fenv: HashMap<&str, NumKind> =
                f.params.iter().map(|(n, _)| (n.as_str(), NumKind::Float)).collect();
            // A body that cannot be read as f64 declines here exactly as it would have
            // before — this adds a reading, it does not weaken one.
            if let Some(fret) =
                mixed_tail_ret_kind(f.body, &mut fenv, f.name, &fkinds, sigs, user_fns)
            {
                let all_float: u16 = ((1u32 << f.params.len()) - 1) as u16;
                return Some((all_float, fkinds, fret));
            }
        }
        return None;
    }
    let mut env: HashMap<&str, NumKind> =
        f.params.iter().zip(&kinds).map(|((n, _), &k)| (n.as_str(), k)).collect();
    let ret = mixed_tail_ret_kind(f.body, &mut env, f.name, &kinds, sigs, user_fns)?;
    Some((mask, kinds, ret))
}

/// Pure scalar builtins the `i64` kernel codegen emits inline, matching the interpreter
/// bit-for-bit: `abs` is `wrapping_abs` (Cranelift `iabs`, which wraps `i64::MIN` to
/// itself); `min`/`max` reproduce the interpreter's `as_f64()`-compare-then-return-the-
/// original-operand semantics (so they agree even past 2^53, where a native integer
/// compare would differ). Added to the JIT-eligible set only when no user function of the
/// same name shadows them (then the call dispatches to the user's function instead).
pub const JIT_SCALAR_BUILTINS: &[(&str, usize)] =
    &[("abs", 1), ("min", 2), ("max", 2), ("to_int", 1), ("sign", 1)];

/// For a recognized JIT builtin, whether the call arity matches; for any other name (a
/// user function) there is no constraint here — its arity is validated by the front end.
fn jit_builtin_arity_ok(name: &str, nargs: usize) -> bool {
    match JIT_SCALAR_BUILTINS.iter().find(|(n, _)| *n == name) {
        Some((_, ar)) => *ar == nargs,
        None => true,
    }
}

/// Pure float builtins the `f64` kernel codegen emits inline, bit-for-bit with the
/// interpreter: `sqrt` → hardware `fsqrt` (IEEE correctly-rounded, NaN on negatives — the
/// interpreter's `f64::sqrt` doesn't raise); `abs` → `fabs`; `min`/`max` → the
/// interpreter's `as_f64()`-compare (identity for floats) then pick the original operand,
/// so NaN propagates identically. The libm transcendentals (`exp`/`sin`/`tanh`/…) are NOT
/// here: they'd need an external-symbol call whose result must match the host libm exactly.
const JIT_FLOAT_BUILTINS: &[(&str, usize)] =
    &[("sqrt", 1), ("abs", 1), ("min", 2), ("max", 2), ("to_float", 1)];

/// The arity of a recognized JIT float builtin, or `None` if `name` is not one.
fn jit_float_builtin_arity(name: &str) -> Option<usize> {
    JIT_FLOAT_BUILTINS.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
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
                // Bitwise ops on two Ints are unconditionally i64-closed — the
                // interpreter returns `Int(a & b)` etc. with no overflow, promotion, or
                // trap — so `band`/`bor`/`bxor` match exactly. Int kind only.
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => kind == NumKind::Int,
                // `<<`/`>>` only by a constant in `0..=63`: the interpreter *raises* for
                // an out-of-range shift, while native `ishl`/`sshr` silently mask the
                // count, so only an in-range constant is provably equivalent.
                BinOp::Shl | BinOp::Shr => {
                    kind == NumKind::Int
                        && matches!(**right, Expr::Int(n) if (0..=63).contains(&n))
                }
                // `//` (euclidean floor division) is i64-closed like `%`; JIT only by a
                // positive constant divisor (rules out `//0` and the `sdiv(i64::MIN,-1)`
                // trap), lowered as `sdiv` adjusted down when the remainder is negative.
                BinOp::FloorDiv => {
                    kind == NumKind::Int && matches!(**right, Expr::Int(n) if n > 0)
                }
                _ => false,
            };
            op_ok
                && value_eligible(left, eligible, locals, kind)
                && value_eligible(right, eligible, locals, kind)
        }
        // Unary negation: the interpreter is `wrapping_neg` on Int / `-f` on Float —
        // exactly native `ineg`/`fneg`. (Without this arm every NEGATIVE LITERAL, which
        // parses as `Neg(lit)`, silently disqualified its whole kernel.)
        Expr::Unary { op: UnOp::Neg, expr, .. } => value_eligible(expr, eligible, locals, kind),
        Expr::Call { name, args, .. } => {
            eligible.contains(name.as_str())
                && jit_builtin_arity_ok(name, args.len())
                && args.iter().all(|a| value_eligible(a, eligible, locals, kind))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            cond_eligible(cond, eligible, locals, kind)
                && value_eligible(then_branch, eligible, locals, kind)
                && value_eligible(else_branch, eligible, locals, kind)
        }
        Expr::Let { bindings, body, .. } => {
            let mut locals2 = locals.clone();
            for (n, v) in bindings {
                if !value_eligible(v, eligible, &locals2, kind) {
                    return false;
                }
                locals2.insert(n.as_str());
            }
            value_eligible(body, eligible, &locals2, kind)
        }
        Expr::Match { scrutinee, arms, .. } => match_eligible(scrutinee, arms, eligible, locals, kind),
        _ => false,
    }
}

/// An `i64`-scrutinee `match` the JIT can lower to an if/else chain ([`gen_match`]): the
/// scrutinee and every arm body are `i64`-eligible; each pattern is an `Int` literal, an
/// `Or` of `Int` literals, `_`, or a binder; each guard is an `i64` condition (seeing a
/// binder if the pattern is one); and the **last** arm is an unguarded catch-all (`_`/
/// binder) so the lowering is total — a non-exhaustive `match` (which the interpreter
/// raises on) falls through to the VM instead. `Float`/`Str`/`Bool`/tuple/record patterns
/// are not `i64`-closed and fall through.
fn match_eligible(
    scrutinee: &Expr,
    arms: &[crate::ast::MatchArm],
    eligible: &HashSet<&str>,
    locals: &HashSet<&str>,
    kind: NumKind,
) -> bool {
    use crate::ast::Pattern;
    if kind != NumKind::Int || arms.is_empty() {
        return false;
    }
    if !value_eligible(scrutinee, eligible, locals, kind) {
        return false;
    }
    let last = arms.last().unwrap();
    let last_total =
        last.guard.is_none() && matches!(last.pattern, Pattern::Wildcard | Pattern::Bind(_));
    if !last_total {
        return false;
    }
    arms.iter().all(|arm| {
        let pat_ok = match &arm.pattern {
            Pattern::Int(_) | Pattern::Wildcard | Pattern::Bind(_) => true,
            Pattern::Or(alts) => alts.iter().all(|p| matches!(p, Pattern::Int(_))),
            _ => false,
        };
        if !pat_ok {
            return false;
        }
        // A binder pattern adds its name (the scrutinee) for the guard + body.
        let mut locals2 = locals.clone();
        if let Pattern::Bind(n) = &arm.pattern {
            locals2.insert(n.as_str());
        }
        let guard_ok =
            arm.guard.as_ref().is_none_or(|g| cond_eligible(g, eligible, &locals2, kind));
        guard_ok && value_eligible(&arm.body, eligible, &locals2, kind)
    })
}

fn cond_eligible(e: &Expr, eligible: &HashSet<&str>, locals: &HashSet<&str>, kind: NumKind) -> bool {
    match e {
        // `and`/`or` are widenable ONLY in condition position (an `if`/filter/guard
        // condition is forced to `Bool`). Each side must itself be a condition, so every
        // leaf is a comparison whose operands are pure and total — a native
        // non-short-circuit `band`/`bor` is then bit-identical to the interpreter's
        // short-circuit `and`/`or` (no operand can be `Missing` or raise). NOT added to
        // `value_eligible`: in *value* position `true or missing` is `Missing`, not i64.
        Expr::Binary { op: BinOp::And | BinOp::Or, left, right, .. } => {
            cond_eligible(left, eligible, locals, kind)
                && cond_eligible(right, eligible, locals, kind)
        }
        Expr::Binary { op, left, right, .. } => matches!(
            op,
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne
        ) && value_eligible(left, eligible, locals, kind)
            && value_eligible(right, eligible, locals, kind),
        _ => false,
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

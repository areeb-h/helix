# Performance roadmap — how Helix surpasses the incumbents

Synthesized from three 2025 research sweeps (fast interpreters, modern JITs, the
data/tensor frontier). The honest thesis up front:

> We will **not** out-tune CPython's 30-year-old C interpreter at scalar
> microbenchmarks by polishing our own interpreter. We win by **leapfrogging** —
> adopting techniques *newer* than the incumbents (copy-and-patch JIT 2021,
> adaptive specialization 2021, fusing array compilers, Arrow, CubeCL) and
> **combining three best-in-class engines** behind one type system, which no
> existing language does. And on Helix's *actual* target workloads — data and
> tensors — Helix already beats Python/R/pandas today via Polars.

## Where we are

`fib(35)` (≈30M calls, pure scalar recursion — the interpreter's worst case),
release, same machine:

| | time | vs C |
|---|--:|--:|
| C (gcc -O2) | 0.01s | 1× |
| Go | 0.02s | ~2× |
| **Helix — Cranelift JIT** | **0.04s** | **~4×** |
| Node/JS (V8 JIT) | 0.08s | ~8× |
| Python 3.12 | 0.69s | ~69× |
| Helix bytecode VM | 1.52s | ~152× |
| Helix tree-walker | 4.65s | ~465× |

**With the Cranelift JIT (Track B, first iteration), Helix on the integer core now
beats Node/V8 and Python, sits ~2× off Go and ~4× off C, and is 38× faster than
its own bytecode VM.** This validates the whole thesis: the type-specialized
native-codegen path is how Helix surpasses the interpreted incumbents. Scope
caveat: the JIT currently compiles the *integer-recursion subset* (`i64` arith,
`if`, `let`, calls, ≤4 params); `Float`/array/general code still falls back to the
VM/tree-walker until later JIT stages widen coverage. (C at 0.01s is near timer
resolution; the JIT↔C ratio is approximate at these tiny times, but the ranking
is firm.)

## Track D — compute less (purity-enabled), the way to beat C

The only honest way a managed language beats hand-written C is by **not running
redundant work** — which purity makes safe and C can't prove.

- [x] **Automatic memoization** of pure, mutable-global-free, overlapping-recursive
      functions (≥2 self-calls), gated on `Int` args, bounded. `fib(35)`: ~30M calls
      → ~35 → **instant, beats C** (`gcc -O2` 0.01s). Observably transparent. See
      [caching-and-memory.md](caching-and-memory.md).
- [ ] Kernel **fusion** (Track C) — the same idea for array/tensor pipelines:
      eliminate intermediates C-libraries materialize. Weld measured 29–31×.

## Track A — interpreter: pass CPython (no JIT)

Ranked by ROI (effort S=days, M=1–2wk, L=multi-wk). Items 1–2 alone are projected
to erase the deficit and pass CPython.

1. **Register bytecode + overlapping register windows** (L). Lua 5's move cut ~46%
   of executed instructions (measured 1.48× on AMD64); overlapping windows let the
   caller write args directly into the callee's parameter slots — **zero per-call
   `Vec`/alloc**, the single biggest win for call-bound code like fib.
   Refs: register-vs-stack survey (arXiv 1611.00467), Lua 5.0 paper.
2. **Quickening / adaptive inline caches (PEP 659, 2021)** (M–L). The exact thing
   CPython 3.12 does and we don't: ops self-rewrite in place to type-specialized
   variants with inline caches after a few hits. Pure interpreter technique, no
   machine code, ~1.25–1.5× whole-interpreter; `LOAD_ATTR`/global lookups become a
   version-check + array index. Excellent Rust fit (mutate a `Vec<Op>` + a cache
   side-array, no `unsafe`). Refs: PEP 659, Brunthaler DLS'10, CPython internals.
3. **8-byte values (NaN-boxing / tagged pointer)** (L). Shrinks `Value` from 24→8
   bytes, makes it `Copy`, cuts stack/clone traffic ~1.3–2×. Rust caveat: NaN-boxed
   words break `Rc`'s automatic `Drop` — needs manual refcounting or a GC arena, so
   it's the highest-risk item. A 3-bit tagged-pointer scheme is a lighter middle
   ground that keeps `Rc`.
4. **`become` tail-call dispatch** (M, nightly). One function per opcode, guaranteed
   tail calls; measured to beat the `match`-loop VM, especially on ARM64. Gate behind
   a nightly feature; keep the stable `match` (LLVM already emits a jump table for a
   dense `#[repr(u8)]` opcode). Defer until 1–3 land.
5. [x] **Superinstructions** (S–M). Fuse hot op pairs into one dispatch.
   **Shipped:** `LoadLocal;LoadLocal;Binary → LoadLocalBinary`,
   `LoadLocal;Const;Binary → LoadLocalConstBinary`, `Const;Binary → ConstBinary`,
   each delegating to the same `binary()` fast-path for identical semantics.
   Measured **0.38s → 0.32s on reduce-10M (~1.18×)**; bit-exact (`994650007`),
   fuzzer clean. *Safety lesson (fuzzer-caught):* a naive emitted-code peephole
   wrongly fused across **jump targets** — a complex operand (`if`/`and`) can end in
   a `Const` that is also a branch-landing point; truncating it corrupts control
   flow. Fix: gate fusion on **AST simplicity** (operand is a literal or bare ident),
   which provably has no inbound jumps. Refs: Ertl & Gregg TOPLAS'05.

**Already shipped:** scalar arithmetic fast-path; `drain` (no arg-`Vec`) on calls;
no per-dispatch instruction clone; superinstructions (item 5).

### [x] Closed the C/Go gap on reduce-10M — native JIT loop (0.32s → 0.03s)

The reduce-10M benchmark now runs in **0.03s — C/Go parity** (C 0.02s, Go 0.02s,
Node 0.05s), bit-exact (`994650007`), down from 0.32s on the VM loop (~10×):

| | reduce 10M |
|---|--:|
| C (gcc -O2) | 0.02s |
| Go | 0.02s |
| **Helix — JIT loop** | **0.03s** |
| Node/JS | 0.05s |
| Python 3 | 0.53s |
| Helix — VM loop | 0.32s |
| Helix — tree-walker | 1.07s |

**How (widen the existing engine, no special-case fold).** When the compiler lowers
`range(s,e).reduce(init, (acc,x) => body)` and `body` is `i64`-eligible over `{acc,x}`,
it registers a `ReduceLoop` and emits a runtime-guarded `TryJitReduce`. The Cranelift
tier compiles each to a native `extern "C" fn(i64,i64,i64)->i64` register loop
(reusing `gen_value`). The VM takes the native path **only when start/end/init are all
`Int` within the 100M cap**; otherwise it falls through to the *unchanged* bytecode
loop — so floats, over-cap ranges (which must error), and non-x86/`HELIX_NOJIT` builds
all keep the oracle-matched path. The compiler is the **single source of truth** (it
decides eligibility, hands the request list to `jit::build`) — no two-pass coupling.

This is the principled solution the philosophy demanded — *not* a bespoke range-fold.
It generalizes (any `i64` reduce body), and `Mod` joined `+ - *` in the JIT because
`Int % Int` is i64-closed (`rem_euclid`); it is admitted **only with a positive
constant divisor**, which makes native `rem_euclid` total and rules out the `%0`
error case. (`Div` stays out: for `Int` the interpreter returns a *Float*, so `/` is
not i64-closed at all.)

**Bug the new fuzzer caught (pre-existing, now fixed):** a reduce `init` that mentions
a binder name — `range(...).reduce(x, (acc, x) => ...)` — was compiled with the
binders already in scope, so `init`'s `x` wrongly resolved to the (unbound) loop slot
instead of the outer `x`. The tree-walker evaluates `init` in the *outer* environment.
Fix: compile `init` before declaring the binders, in both `compile_reduce` and
`compile_reduce_range`. (Only surfaced once a fuzzer put a binder name in `init`
position — the canonical example of why differential fuzzing earns its keep.)

**Still compounding from here:** 8-byte `Copy` values (Track A item 3) shrink the
per-element traffic everywhere; and the same loop-JIT approach extends to `map`/
`filter` bodies and to multi-statement numeric loops once those land.

## Track B — JIT: reach Go/Node, approach C

Helix's edge: the **type checker already supplies inferred types**, the hardest
prerequisite for C-class codegen. Non-negotiable prerequisites for any tier:
**unboxed scalars** (i64/f64 in registers) + **monomorphization** driven by the
inferred types (the Julia recipe: monomorphize → unbox → function barriers).

| Tier | Strategy | Result | Effort |
|---|---|---|---|
| 1 baseline JIT | **Copy-and-patch** (Xu & Kjølstad, OOPSLA'21) — stencils from a build-time LLVM pass, patched at runtime | ~10× over interpreter, ~LLVM-O0 quality, µs compile | L |
| 2 optimizing JIT | **Cranelift** (`cranelift-jit`, powers Wasmtime) on hot monomorphized fns | **Go/Node-class**, ~14% off LLVM, ~10× faster compile, Rust-native | M–L |
| 3 peak (optional) | **LLVM** (`inkwell`) for proven-hot vectorizable kernels only | **C-class** on autovectorizable loops | XL |

Recommendation: **Cranelift first** (lowest-risk Rust-native path to the Go/Node
target; the crate exists today). Add copy-and-patch as a fast baseline tier later
(CPython 3.13/3.14's architecture). Reserve LLVM for the few kernels that need it.
A **method JIT** (per-signature specialization) fits Helix better than a tracing
JIT, because types are already static — tracing's main payoff (runtime type
discovery) is redundant here.

**Honest ceiling:** a JIT lands ~1.1–2× of C -O2 on scalar numeric code; it can
*match or beat* C only where it has runtime knowledge C lacks (sizes, constants,
fusion) or via already-vectorized Polars/Arrow kernels. "Surpass" honestly means:
**beat CPython/Ruby/R comfortably, match Go/Node, approach C** — not dethrone
`-O2 -march=native` on a SAXPY loop.

## Track C — the unifying architecture (the real moat)

The structural edge no incumbent has: **one type system + one deferred expression
graph** that fans out to three best-in-class engines.

1. **Tabular → Polars/Arrow** (today): lazy optimizer, columnar, multicore —
   already **8–11× over pandas**; 50M-row query ~0.2s. Ensure the checker lowers to
   *lazy* frames so pushdown/fusion carry through.
2. **Scalar/control → type-specialized JIT** (Track B): Mojo showed 78–119× over
   CPython from type specialization + native SIMD + no-GC.
3. **Tensor → a Helix-owned typed fusing IR** (Weld/TACO/XLA lineage), lowering to
   CPU (`portable-simd` + `rayon`: ~207 GFLOPS measured vs ~27 scalar) and GPU via
   **CubeCL** (`#[cube]` → CUDA/ROCm/Metal/SPIR-V, autotuned, borrow-checked Rust;
   already powers Burn). Fusion is *multiplicative*: Weld measured **29–31× across
   NumPy+Pandas** by eliminating cross-op intermediates.

Why static types are the keystone: they fix shapes/dtypes and prove fusion legal
at **compile time**, beating JAX's runtime retracing — and let one graph target
Polars, the scalar JIT, or the tensor compiler. Don't adopt MLIR wholesale (too
heavy for a Rust team) — own a small Rust fusion IR (the MLIR *philosophy*, not its
codebase).

**Three highest-leverage next steps** (from the research):
1. The **typed deferred expression graph** as the universal IR — everything
   compounds on it.
2. Make the checker lower tabular ops to **Polars lazy frames** (huge win, low cost).
3. Prototype the **numeric-kernel subset → CubeCL** on one fused workload to de-risk
   the CPU/GPU-portable tensor path.

## Key sources

Copy-and-patch: fredrikbk.com/copy-and-patch.html, arXiv 2011.13127 · CPython JIT:
PEP 659, tonybaloney.github.io, krun.pro/python-jit · Cranelift vs LLVM:
bytecodealliance wasmtime docs · Julia: Vitek OOPSLA'18, arXiv 1808.03370 ·
Register VM: arXiv 1611.00467, Lua 5.0 paper · Quickening: Brunthaler DLS'10 ·
`become` dispatch: mattkeeter.com/blog/2026-04-05-tailcall · Mojo: bswen benchmarks ·
Weld: arXiv 1709.06416 · TACO: fredrikbk.com/publications/taco.pdf · XLA: arXiv
2301.13062 · PyTorch 2: pytorch2-2.pdf · Rust SIMD 2025: shnatsel.medium.com ·
CubeCL: github.com/tracel-ai/cubecl · Polars: pola.rs.

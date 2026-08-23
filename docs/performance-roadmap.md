# Performance roadmap

> **Status dateline 2026-08-24.** Track B is landed: the Cranelift JIT is the
> shipped tier (cargo feature `jit`, on by default since v0.4.0; x86-64 Linux;
> bytecode identical with or without it). Current cross-language numbers live in
> [`bench/kernels/RESULTS.md`](../bench/kernels/RESULTS.md), not in the tables
> below. A second, native DataFrame backend now exists behind the ADR 0012 seam
> (ADR 0033, stages 0–3; polars remains the default and the oracle).

Synthesized from three 2025 research sweeps (fast interpreters, modern JITs, and
the data/tensor frontier). The central thesis:

> Helix does **not** aim to out-tune CPython's 30-year-old C interpreter at scalar
> microbenchmarks by refining its own interpreter. The strategy is to
> **leapfrog** — adopting techniques *newer* than the incumbents (copy-and-patch
> JIT 2021, adaptive specialization 2021, fusing array compilers, Arrow, CubeCL)
> and **combining three best-in-class engines** behind one type system, which no
> existing language does. On Helix's *actual* target workloads — data and
> tensors — Helix is already faster than Python, R, and pandas today via Polars.

## Status at Track B's first landing (historical)

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

**With the Cranelift JIT (Track B, first iteration), Helix on the integer core is
faster than Node/V8 and Python 3.12, approximately 2× slower than Go and 4× slower
than C, and 38× faster than its own bytecode VM.** This supports the thesis that
the type-specialized native-codegen path is how Helix surpasses the interpreted
incumbents. Scope caveat: the JIT currently compiles the *integer-recursion
subset* (`i64` arithmetic, `if`, `let`, calls, ≤4 parameters); `Float`, array, and
general code still falls back to the VM or tree-walker until later JIT stages
widen coverage. (C at 0.01s is near timer resolution; the JIT-to-C ratio is
approximate at these small times, though the ranking is firm.)

*(Historical: coverage has since widened well past this subset, and
auto-memoization later moved `fib(40)` to ~0.006 s — see
[jit-benchmarks.md](jit-benchmarks.md) §5.1 and `bench/kernels/RESULTS.md`.)*

## Track D — compute less (purity-enabled)

The principal way a managed language can outperform hand-written C is by **not
running redundant work**, which purity makes safe and which C cannot prove.

- [x] **Automatic memoization** of pure, mutable-global-free, overlapping-recursive
      functions (≥2 self-calls), gated on `Int` arguments, bounded. `fib(35)`:
      ~30M calls reduced to ~35, executing faster than C (`gcc -O2` 0.01s).
      Observably transparent. See [caching-and-memory.md](caching-and-memory.md).
- [x] *(scalar pipelines)* Kernel **fusion** (Track C) — landed for `Int`
      `map`/`filter`/`reduce` chains: a chain compiles to a single native loop
      with no intermediate array at any stage (see
      [vectorized-kernels.md](vectorized-kernels.md) §Pipeline fusion).
      Tensor-pipeline fusion remains open. Weld measured 29–31×.

## Track A — interpreter: match CPython (no JIT)

Ranked by return on investment (effort S=days, M=1–2 weeks, L=multiple weeks).
Items 1–2 alone are projected to erase the deficit relative to CPython.

1. **Register bytecode with overlapping register windows** (L). Lua 5's transition
   reduced executed instructions by ~46% (measured 1.48× on AMD64); overlapping
   windows allow the caller to write arguments directly into the callee's parameter
   slots — **zero per-call `Vec` allocation**, the largest improvement for
   call-bound code such as fib. Refs: register-vs-stack survey (arXiv 1611.00467),
   Lua 5.0 paper.
2. **Quickening / adaptive inline caches (PEP 659, 2021)** (M–L). The technique
   CPython 3.12 employs and Helix does not: operations self-rewrite in place to
   type-specialized variants with inline caches after a few executions. A pure
   interpreter technique with no machine code, ~1.25–1.5× whole-interpreter;
   `LOAD_ATTR` and global lookups become a version-check plus an array index. A good
   fit for Rust (mutate a `Vec<Op>` plus a cache side-array, no `unsafe`). Refs:
   PEP 659, Brunthaler DLS'10, CPython internals.
3. **8-byte values (NaN-boxing / tagged pointer)** (L). Shrinks `Value` from 24 to 8
   bytes, makes it `Copy`, and reduces stack/clone traffic ~1.3–2×. Rust caveat:
   NaN-boxed words break `Rc`'s automatic `Drop`, requiring manual reference counting
   or a GC arena; this is therefore the highest-risk item. A 3-bit tagged-pointer
   scheme is a lighter alternative that retains `Rc`.
4. **`become` tail-call dispatch** (M, nightly). One function per opcode, with
   guaranteed tail calls; measured to outperform the `match`-loop VM, particularly on
   ARM64. Gate behind a nightly feature and retain the stable `match` (LLVM already
   emits a jump table for a dense `#[repr(u8)]` opcode). Defer until items 1–3 land.
5. [x] **Superinstructions** (S–M). Fuse hot operation pairs into one dispatch.
   **Implemented:** `LoadLocal;LoadLocal;Binary → LoadLocalBinary`,
   `LoadLocal;Const;Binary → LoadLocalConstBinary`, `Const;Binary → ConstBinary`,
   each delegating to the same `binary()` fast-path for identical semantics.
   Measured **0.38s to 0.32s on reduce-10M (~1.18×)**; bit-exact (`994650007`),
   fuzzer clean. *Safety note (fuzzer-detected):* a naive emitted-code peephole
   incorrectly fused across **jump targets** — a complex operand (`if`/`and`) can end
   in a `Const` that is also a branch-landing point, and truncating it corrupts
   control flow. Fix: gate fusion on **AST simplicity** (operand is a literal or bare
   identifier), which provably has no inbound jumps. Refs: Ertl & Gregg TOPLAS'05.

**Already implemented:** scalar arithmetic fast-path; `drain` (no argument `Vec`) on
calls; no per-dispatch instruction clone; superinstructions (item 5).

### [x] Closed the C/Go gap on reduce-10M — native JIT loop (0.32s to 0.03s)

The reduce-10M benchmark runs in **0.03s, at parity with C and Go** (C 0.02s, Go
0.02s, Node 0.05s), bit-exact (`994650007`), reduced from 0.32s on the VM loop
(~10× faster):

| | reduce 10M |
|---|--:|
| C (gcc -O2) | 0.02s |
| Go | 0.02s |
| **Helix — JIT loop** | **0.03s** |
| Node/JS | 0.05s |
| Python 3 | 0.53s |
| Helix — VM loop | 0.32s |
| Helix — tree-walker | 1.07s |

**Method (widen the existing engine, no special-case fold).** When the compiler
lowers `range(s,e).reduce(init, (acc,x) => body)` and `body` is `i64`-eligible over
`{acc,x}`, it registers a `ReduceLoop` and emits a runtime-guarded `TryJitReduce`.
The Cranelift tier compiles each to a native `extern "C" fn(i64,i64,i64)->i64`
register loop (reusing `gen_value`). The VM takes the native path **only when start,
end, and init are all `Int` within the 100M cap**; otherwise it falls through to the
*unchanged* bytecode loop, so floats, over-cap ranges (which must error), and
non-x86 or `HELIX_NOJIT` builds all retain the oracle-matched path. The compiler is
the **single source of truth**: it determines eligibility and hands the request list
to `jit::build`, with no two-pass coupling.

This is a general solution rather than a bespoke range-fold. It generalizes to any
`i64` reduce body, and `Mod` joined `+ - *` in the JIT because `Int % Int` is
i64-closed (`rem_euclid`); it is admitted **only with a positive constant divisor**,
which makes native `rem_euclid` total and rules out the `%0` error case. (`Div`
remains excluded: for `Int` the interpreter returns a *Float*, so `/` is not
i64-closed.)

**Bug detected by the new fuzzer (pre-existing, now fixed):** a reduce `init` that
references a binder name — `range(...).reduce(x, (acc, x) => ...)` — was compiled
with the binders already in scope, so `init`'s `x` incorrectly resolved to the
(unbound) loop slot instead of the outer `x`. The tree-walker evaluates `init` in
the *outer* environment. Fix: compile `init` before declaring the binders, in both
`compile_reduce` and `compile_reduce_range`. This surfaced only once a fuzzer placed
a binder name in `init` position, illustrating the value of differential fuzzing.

**Further compounding gains:** 8-byte `Copy` values (Track A item 3) reduce
per-element traffic throughout; and the same loop-JIT approach extends to `map` and
`filter` bodies and to multi-statement numeric loops once those land.

## Track B — JIT: reach Go/Node, approach C

Helix's advantage: the **type checker already supplies inferred types**, the
hardest prerequisite for C-class codegen. Essential prerequisites for any tier:
**unboxed scalars** (i64/f64 in registers) and **monomorphization** driven by the
inferred types (the Julia approach: monomorphize, unbox, function barriers).

| Tier | Strategy | Result | Effort |
|---|---|---|---|
| 1 baseline JIT | **Copy-and-patch** (Xu & Kjølstad, OOPSLA'21) — stencils from a build-time LLVM pass, patched at runtime | ~10× over interpreter, ~LLVM-O0 quality, µs compile | L |
| 2 optimizing JIT | **Cranelift** (`cranelift-jit`, powers Wasmtime) on hot monomorphized fns | **Go/Node-class**, ~14% off LLVM, ~10× faster compile, Rust-native | M–L |
| 3 peak (optional) | **LLVM** (`inkwell`) for proven-hot vectorizable kernels only | **C-class** on autovectorizable loops | XL |

Recommendation: **Cranelift first** (the lowest-risk Rust-native path to the
Go/Node target; the crate exists today) — **taken and landed**: the Cranelift
tier is the production JIT (cargo feature `jit`, default-on since v0.4.0).
Add copy-and-patch as a fast baseline tier
later (the CPython 3.13/3.14 architecture). Reserve LLVM for the few kernels that
require it. A **method JIT** (per-signature specialization) fits Helix better than a
tracing JIT, because types are already static; tracing's principal benefit (runtime
type discovery) is redundant here.

**Realistic ceiling:** a JIT achieves ~1.1–2× of C -O2 on scalar numeric code; it
can *match or exceed* C only where it has runtime knowledge C lacks (sizes,
constants, fusion) or via already-vectorized Polars/Arrow kernels. "Surpass" here
means: **substantially faster than CPython, Ruby, and R, comparable to Go and Node,
approaching C** — not exceeding `-O2 -march=native` on a SAXPY loop.

## Track C — the unifying architecture

The structural advantage no incumbent has: **one type system and one deferred
expression graph** that fans out to three best-in-class engines.

1. **Tabular → Polars/Arrow** (today): lazy optimizer, columnar, multicore —
   Helix delegates with **no meaningful query overhead**: the 50M-row query runs
   ~0.20 s end-to-end vs 0.28 s for raw Python-Polars, same engine underneath
   (docs/benchmarks.md — which states plainly that pandas and DuckDB have NOT
   been measured; an earlier draft of this line claimed "8–11× faster than raw
   Python-Polars", which no run in this repo supports). The checker must lower
   to *lazy* frames so pushdown and fusion carry through. (Since ADR 0033 a
   native second backend covers the appliance profile; polars stays the default.)
2. **Scalar/control → type-specialized JIT** (Track B): Mojo demonstrated 78–119×
   over CPython from type specialization, native SIMD, and no GC.
3. **Tensor → a Helix-owned typed fusing IR** (Weld/TACO/XLA lineage), lowering to
   CPU (`portable-simd` plus `rayon`: ~207 GFLOPS measured versus ~27 scalar) and
   GPU via **CubeCL** (`#[cube]` → CUDA/ROCm/Metal/SPIR-V, autotuned, borrow-checked
   Rust; already powers Burn). Fusion is *multiplicative*: Weld measured **29–31×
   across NumPy and Pandas** by eliminating cross-operation intermediates.

Static types are the keystone: they fix shapes and dtypes and prove fusion legal at
**compile time**, improving on JAX's runtime retracing, and allow one graph to
target Polars, the scalar JIT, or the tensor compiler. MLIR should not be adopted
wholesale (too heavy for a Rust team); instead Helix owns a small Rust fusion IR
(the MLIR *philosophy*, not its codebase).

**Three highest-leverage next steps** (from the research):
1. The **typed deferred expression graph** as the universal IR; everything else
   compounds on it.
2. Lower tabular operations to **Polars lazy frames** in the checker (substantial
   gain, low cost) — **landed**: the polars backend is lazy end-to-end
   (`src/backend/polars.rs` extends a `LazyFrame` query plan per operation).
3. Prototype the **numeric-kernel subset → CubeCL** on one fused workload to
   de-risk the CPU/GPU-portable tensor path.

## Key sources

Copy-and-patch: fredrikbk.com/copy-and-patch.html, arXiv 2011.13127 · CPython JIT:
PEP 659, tonybaloney.github.io, krun.pro/python-jit · Cranelift vs LLVM:
bytecodealliance wasmtime docs · Julia: Vitek OOPSLA'18, arXiv 1808.03370 ·
Register VM: arXiv 1611.00467, Lua 5.0 paper · Quickening: Brunthaler DLS'10 ·
`become` dispatch: mattkeeter.com/blog/2026-04-05-tailcall · Mojo: bswen benchmarks ·
Weld: arXiv 1709.06416 · TACO: fredrikbk.com/publications/taco.pdf · XLA: arXiv
2301.13062 · PyTorch 2: pytorch2-2.pdf · Rust SIMD 2025: shnatsel.medium.com ·
CubeCL: github.com/tracel-ai/cubecl · Polars: pola.rs.

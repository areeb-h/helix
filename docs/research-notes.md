# Research notes

Helix is built research-first: existing systems inform the design rather than being
copied directly, and the best approach is synthesized from them. This file preserves
the cited research that drove the major decisions, so the reasoning is retained beyond
the conversation. Four areas are recorded: (1) language and runtime design (the ADRs),
(2) fast interpreters, (3) modern JITs, and (4) the data/numeric/GPU frontier.

---

## 1. Language & runtime design → the ADRs

The founding research covered missing-data models, type systems, collection-API
unity, and functions/errors/mutability. It produced the four accepted ADRs in
`docs/adr/`:

- **ADR-0001 — missing data.** Surveyed Hoare's null "billion-dollar mistake",
  Option/Maybe, pandas' inconsistent NaN/None/NaT sentinels and Arrow validity bitmaps,
  SQL three-valued logic, R's `NA`, and Julia's `missing`. Decision: a single `missing`
  bottom value with Julia-style propagation and three-valued logic, unifying scalars and
  (later) Arrow columns.
- **ADR-0002 — type system.** HM vs bidirectional vs gradual; why global HM gives
  poor errors; the runtime-schema-dataframe problem; gradual-typing performance
  cliffs. Decision: permissive bidirectional checking with an `Unknown` top type,
  zero false positives.
- **ADR-0003 — collection API.** pandas' inconsistent indexing versus dplyr's
  consistent verbs, LINQ, and Julia multiple dispatch. Decision: one verb protocol
  across Array/DataFrame/Tensor/Dna.
- **ADR-0004 / 0005 — functions, errors, mutability, syntax.** Brace-free syntax,
  Result/`?` versus exceptions, value semantics and COW. Decision: `fn name(p) = expr`,
  immutable-by-default, words over symbols, dot-chains.

---

## 2. Fast bytecode interpreters (2015–2025)

The performance gap relative to CPython is adaptive specialization, which the Helix VM
lacked. Techniques ranked by return on investment:

1. **8-byte values (NaN-boxing / tagged pointer)** — shrink `Value` from 24 to 8 bytes,
   make it `Copy`, approximately 1.3–2×. Rust caveat: this breaks `Rc`'s automatic
   `Drop` (requiring manual refcounting or a GC arena), with a lighter 3-bit
   tagged-pointer middle ground as an alternative.
   ([rust-hosted-langs guide](https://rust-hosted-langs.github.io/book/chapter-interp-tagged-ptrs.html), [Float Self-Tagging 2024](https://arxiv.org/pdf/2411.16544))
2. **Quickening / adaptive inline caches (PEP 659, 2021)** — ops self-rewrite in
   place to type-specialized variants with inline caches; approximately 1.25–1.5×, a
   good Rust fit (mutate a `Vec<Op>` and a cache side-array, no `unsafe`).
   ([PEP 659](https://peps.python.org/pep-0659/), [Brunthaler DLS'10](https://publications.sba-research.org/publications/dls10.pdf), [CPython internals](https://github.com/python/cpython/blob/main/InternalDocs/interpreter.md))
3. **Register bytecode + overlapping register windows** — Lua 5 reduced executed
   instructions by approximately 46%; measured at 1.48×; windows give zero per-call
   argument allocation (the largest benefit for call-bound code such as fib).
   ([register-vs-stack survey](https://arxiv.org/pdf/1611.00467), [Lua 5.0 paper](https://www.lua.org/doc/jucs05.pdf))
4. **`become` tail-call dispatch** — one function per opcode, guaranteed TCO; exceeds
   the `match`-loop VM, particularly on ARM64; nightly-only.
   ([Keeter, tail-call interpreter](https://www.mattkeeter.com/blog/2026-04-05-tailcall/))
5. **Superinstructions** — fuse hot op pairs; approximately 1.1–1.3×.
   ([Ertl & Gregg TOPLAS'05](https://www.scss.tcd.ie/David.Gregg/papers/toplas05.pdf))

**Techniques newer than the incumbents:** adaptive in-place specialization (2021) and
interpreter generators such as Deegen (2024) that auto-generate the interpreter, a
copy-and-patch baseline JIT, and inline caches from one bytecode specification.
([Deegen](https://arxiv.org/pdf/2411.11469))
Helix has implemented a scalar arithmetic fast-path, `drain`-not-`split_off` calls, and
no per-dispatch clone. Planned next: register VM and windows, then quickening.

---

## 3. Modern JITs

Helix's advantage is that the type checker already supplies inferred types — the
hardest prerequisite for C-class codegen. Required prerequisites: unboxed scalars and
monomorphization (the Julia approach: monomorphize → unbox → function barriers).

| Tier | Strategy | Result | Effort |
|---|---|---|---|
| Baseline JIT | **Copy-and-patch** (Xu & Kjølstad, OOPSLA'21) — stencils from a build-time LLVM pass, patched at runtime | ~10× over interp, ~LLVM-O0 quality, µs compile | L |
| Optimizing JIT | **Cranelift** (`cranelift-jit`, powers Wasmtime) | **Go/Node-class**, ~14% off LLVM, ~10× faster compile, Rust-native | M–L |
| Peak (optional) | **LLVM** (`inkwell`) for proven-hot vectorizable kernels | **C-class** on autovectorizable loops | XL |

- Copy-and-patch: approximately 100× faster compile than LLVM-O0, code approximately
  10× over interpretation; CPython 3.13/3.14 adopted it (3.14: 12–18% median, 18–25%
  numeric).
  ([project](http://fredrikbk.com/copy-and-patch.html), [paper](https://arxiv.org/abs/2011.13127), [CPython JIT](https://tonybaloney.github.io/posts/python-gets-a-jit.html), [3.14 numbers](https://krun.pro/python-jit/))
- Cranelift: approximately 2% slower than V8, approximately 14% slower than LLVM,
  approximately 10× faster to compile, full SIMD; best Rust fit.
  ([Cranelift vs LLVM](https://github.com/bytecodealliance/wasmtime/blob/main/cranelift/docs/compare-llvm.md))
- Julia's performance derives from per-signature monomorphization to unboxed LLVM IR.
  ([Vitek OOPSLA'18](https://janvitek.org/pubs/oopsla18b.pdf), [dispatch paper](https://arxiv.org/pdf/1808.03370))
- A method JIT is preferable to a tracing JIT for Helix: types are already static, so
  tracing's benefit (runtime type discovery) is redundant. ([JIT impls](https://kipp.ly/jits-impls/))

**Performance ceiling:** a JIT reaches approximately 1.1–2× of C -O2 on scalar numeric
code; it matches or exceeds C only with runtime knowledge C lacks (sizes, constants,
fusion) or via already-vectorized kernels. The objective is to exceed CPython and R by a
clear margin, match Go and Node, and approach C — not to surpass `-O2 -march=native` on
a hand-written loop.

**Helix adopted Cranelift first** — implemented (see `docs/execution-engine.md`).

---

## 4. The data / numeric / GPU frontier

Modern systems converge on fusion (keeping intermediates in registers rather than DRAM)
and type specialization.

- **Mojo** (MLIR, AOT, SIMD, ownership, comptime): 78–119× over CPython on
  n-body/spectral-norm. Applicable lessons: type specialization, comptime, and no GC.
  Approaches to avoid: dependence on MLIR and a closed Python-superset.
  ([benchmarks](https://docs.bswen.com/blog/2026-03-10-mojo-python-performance-comparison/), [Modular](https://docs.modular.com/mojo/vision/))
- **Fusing compilers**: XLA/JAX (up to 58× over NumPy on GPU), PyTorch 2 /
  TorchInductor (1.3–2.4×, emits Triton/C++), TACO (sparse), Weld (29–31×
  across NumPy and Pandas by eliminating cross-op intermediates).
  ([XLA](https://openxla.org/xla/gpu_architecture), [PyTorch 2](https://docs.pytorch.org/assets/pytorch2-2.pdf), [TACO](https://fredrikbk.com/publications/taco.pdf), [Weld](https://arxiv.org/pdf/1709.06416))
- **MLIR** is too heavy for a small Rust team; the approach is to maintain a small Rust
  fusion IR, adopting the MLIR philosophy rather than its codebase.
  ([pliron](https://discourse.llvm.org/t/pliron-an-extensible-compiler-ir-framework-inspired-by-mlir/71906))
- **CPU data-parallelism is highly effective**: `portable-simd` and `rayon` reach
  approximately 207 GFLOPS versus approximately 27 scalar; Polars reaches 8–11× over
  pandas. GPU is justified only at high arithmetic intensity.
  ([Rust SIMD 2025](https://shnatsel.medium.com/the-state-of-simd-in-rust-in-2025-32c263e5f53d), [Polars](https://pola.rs/))
- **GPU from Rust**: CubeCL (`#[cube]` → CUDA/ROCm/Metal/SPIR-V, autotuned,
  borrow-checked; powers Burn) is the realistic multi-backend path. ([CubeCL](https://github.com/tracel-ai/cubecl))

**A unified architecture no incumbent provides:** one type system and one deferred
expression graph fanning out to Polars (tabular), the JIT (scalar), and a typed fusing
tensor compiler (CPU and GPU). Static types prove fusion legal at compile time,
exceeding JAX's runtime retracing. Julia has no fusing tensor compiler; Python adds
XLA onto a slow host; R is single-threaded at its core. Helix's three-engine plan is
the structural distinction.

# Helix — Technical Overview

*A scientific programming language designed for the workflows scientific computing requires.*

This document presents an overview of the project: what Helix is, its rationale, its
staged implementation, its measured performance, its current limitations, and its
planned direction. Each section links to the detailed document for that topic.

---

## What Helix is

Helix is a scientific programming language, implemented in safe Rust. It
synthesizes Python's readability, R's data workflow, Rust's safety, Julia's design,
SQL's data operations, and Arrow's zero-copy model, rather than copying any one of them.

**Positioning** (see [research-notes](research-notes.md), [ROADMAP](ROADMAP.md)): Helix
is not a general-purpose Python replacement, but a scientific computing language for the
post-Pandas era, with computational biology as the flagship domain — the
R-for-statistics approach. The tabular, statistical, and array domains (data science,
finance, climate, astronomy) derive nearly for free from the same core; computational
biology is the differentiating focus area.

**Core principles:** simple low-symbol syntax; one obvious way; immutable by default;
strong static typing with extensive inference; educational errors; memory efficiency
(zero-copy, columnar, lazy); leak-free and memory-safe by construction; AI-ready.

---

## Implementation stages

Each stage delivered working, tested code (the test count grew from 44 to the ~540 cases the gate runs today: 412 unit + 127 CLI, as of 2026-08-11).

1. **Core interpreter** — lexer → parser → AST → tree-walking interpreter. Immutable
   bindings and `mut`, Int/Float/String/Bool/Array/Dna/Function, word-based booleans,
   dot-chain method dispatch, caret-and-hint errors, file runner and REPL.
2. **Syntax additions** (drawn from Python and TypeScript) — string interpolation, `??`
   coalescing, records and `.field`, Python-style slicing, tuples and destructuring,
   lambda-param destructuring, `let … in` local bindings, `if/then/else` as an
   expression, comprehension verbs `map`/`filter`/`where`/`reduce` with `it`/`acc`.
3. **`missing` model** ([ADR-0001](adr/0001-missing-data.md)) — a bottom value with
   Julia-style propagation and three-valued logic; `.is_missing()`, `.drop_missing()`.
4. **Static type checker** ([ADR-0002](adr/0002-type-system.md), [types.rs]) —
   permissive bidirectional inference, `Unknown` top type, zero false positives
   (every example type-checks cleanly — a regression-gated guarantee).
5. **DataFrame engine** ([ROADMAP §3](ROADMAP.md), [benchmarks](benchmarks.md)) — lazy
   Polars/Arrow LazyFrames; SQL-style verbs; CSV/Parquet; 50M-row query in approximately 0.2s.
6. **Tensor engine** ([ADR-0007](adr/0007-tensor-backend.md)) — ndarray-backed, NumPy
   broadcasting, axis reductions, matmul, pure-Rust linear algebra.
7. **Leak-freedom and recursion robustness** ([memory-safety](memory-safety.md)) —
   proved leak-free by construction; 2 GiB eval thread and depth guard.
8. **Bytecode VM** ([execution-engine](execution-engine.md)) — slot-resolved variables,
   heap call frames, approximately 3× over the tree-walker; whole-program fallback
   preserves correctness.
9. **Cranelift JIT** ([execution-engine](execution-engine.md),
   [performance-roadmap](performance-roadmap.md)) — native codegen for the numeric core,
   dual-specialized over `i64` and `f64`. Exceeds Node and Python; approaches Go and C.
10. **Numerical accuracy** — every float aggregation uses Neumaier compensated
    summation (accurate to the last ulp; see below).
11. **Genomics flagship** ([ROADMAP §Flagship](ROADMAP.md)) — FASTA via
    `needletail`; `read_fasta`, `gc_content`, `kmers`, `find`, `Array.top`; the
    [genomics example](../examples/bio/genomics.helix) runs end-to-end.
12. **Module system** ([ROADMAP §7](ROADMAP.md)) — `import name` / `import lib.stats
    as st`; a loader resolves the import graph and rewrites every module into one
    namespaced AST that the existing pipeline runs unchanged. Single files remain untouched.
13. **CPython interop, v1** ([ADR-0008](adr/0008-cpython-interop.md),
    [guide](python-interop.md)) — the adoption mechanism. Behind a feature flag (so the
    core binary remains self-contained), Helix embeds CPython: `import python.numpy as
    np`, attribute and method calls forward to Python, scalars convert back, containers
    remain opaque until `to_array`, exceptions become Helix errors. This allows Helix to
    call the existing scientific stack before it has built its own.

---

## Architecture: three engines, one surface

Helix routes each kind of work to a specialized engine, unified by one type system
and one value model:

- **Scalar and control-flow** → tree-walker → bytecode VM → Cranelift JIT (native).
  Tiered: the JIT handles the numeric core; any construct it cannot compile falls back to
  the VM; any construct the VM cannot compile falls back to the tree-walker. The same
  language and the same results, verified by parity tests at every boundary.
- **Bulk tabular** → Polars/Arrow (lazy, columnar, multicore, SIMD).
- **Tensors** → ndarray currently; a typed fusing compiler (CPU `rayon`+SIMD, GPU
  `CubeCL`) is the planned Track C — a structural distinction no incumbent provides.

Delegation is the strategy: Helix provides a consistent, fast, memory-safe surface;
Polars, ndarray, `needletail`/`noodles`, and Cranelift perform the underlying computation.

---

## Benchmarks

**`fib(35)` — pure scalar recursion (~30M calls), release, same machine** (lower = faster):

| | time | note |
|---|--:|---|
| Helix — auto-memoized | 0.00s | exceeds C |
| C (gcc -O2) | 0.01s | baseline |
| Go | 0.02s | AOT native |
| Helix — Cranelift JIT | 0.04s | non-memoizable recursion |
| Node / JS (V8 JIT) | 0.08s | |
| Python 3.12 | 0.69s | |
| Helix — bytecode VM | 1.52s | |
| Helix — tree-walker | 4.65s | |

Two results, depending on the function:

- **For pure overlapping recursion (such as `fib`): Helix exceeds C.** This is achieved
  not through faster codegen but by performing less work: Helix auto-memoizes
  (approximately 35 calls instead of approximately 30M), which it can do safely because
  it proves `fib` is a pure function of its arguments. C cannot auto-memoize, as it
  cannot prove side-effect-freedom, so it runs all 30M calls. The same source, with
  different execution.
- **For non-memoizable recursion: the Cranelift JIT** — 0.04s, 38× over the Helix VM,
  exceeding Node/V8 and Python, approximately 2× off Go. Float recursion reaches the
  same native tier (`fibf(35.0)`: 0.05s versus 1.58s on the VM).

Reproduce with `bash scripts/langbench.sh`.

**DataFrames** ([benchmarks.md](benchmarks.md)): 50M-row filter+group+sort in
approximately 0.2s (Parquet) on all cores; Parquet `count()` is O(1) from metadata;
approximately 8–11× over pandas (Polars engine). For data work, which is Helix's primary
purpose, it already exceeds Python and R.

---

## Numerical accuracy

Scientific results must be accurate to full precision. Two guarantees apply:

- **Output is exact to `f64`.** Floats print via Rust's shortest round-tripping
  representation — for example, `0.5916666666666667` is the exact value, not a truncation.
- **Aggregations use Neumaier compensated summation.** Naive left-to-right `f64`
  summation silently loses low-order bits when magnitudes differ; `sum`/`mean`/`std`/
  `normalize` all route through Neumaier, accurate to the last ulp even for large or
  ill-conditioned data. `std` is two-pass to avoid catastrophic cancellation.
  Regression-tested (`compensated_summation_is_accurate`).

Edge case: integer arithmetic wraps on overflow (matching Rust release builds and most
systems languages — use floats beyond the i64 range); the JIT's float comparisons
follow IEEE-754 (NaN → `false`) rather than the interpreter's NaN-error.

---

## Memory safety

Zero `unsafe` outside one contained function — the JIT's native-call boundary
(`jit::call_i64`/`call_f64`), guarded by a type and arity check, dealing only in scalars
(no heap, no `Rc`). Everything else is leak-free by construction: no interior
mutability implies no `Rc` cycles, which implies an acyclic value graph freed
deterministically. Backed by a `Rc::strong_count` test and flat-RSS measurement. The
full argument is in [memory-safety.md](memory-safety.md). For the computational biology
flagship this property targets C-toolchain speed without C's memory-safety class of
bug. (Deliberately not phrased as a comparison to samtools: that would be an unsourced
claim about a specific, excellent project.)

---

## Current limitations

- **JIT scope:** integer and float numeric recursion only — `+ - * /`, comparisons, `if`,
  `let`, calls, ≤4 params. Arrays, loops, `Mod`, `Pow`, and strings are not yet supported
  in native code; those run on the VM or tree-walker. Mutual recursion compiles and runs
  (two-pass function registration) but is deliberately **not** JIT-compiled: a native frame
  has no depth guard, so every function on a recursion cycle runs on the VM instead, where
  a missing base case raises a catchable error rather than killing the process.
- **VM scope:** arrays, comprehensions, methods, records, tensors, DataFrames, lambdas,
  and interpolation fall back to the tree-walker (correct, but not the fast path).
- **DataFrames:** no cross-statement caching (a file used twice is re-scanned); print
  materializes the whole frame; limited IO (no Arrow IPC, JSON, FASTA-as-frame, or
  `write_csv` yet); no joins or derived columns.
- **Tensors:** no slicing or stacking, single dtype (`f64`), no GPU or autodiff yet.
- **Computational biology:** FASTA only so far; VCF→DataFrame, FASTQ, GFF/BED, and BAM
  are planned next.
- **Types:** no `Maybe`/nullable tracking, no column-level DataFrame typing.
- **Ecosystem:** modules and a v1 Python bridge are implemented ([Phase 7](ROADMAP.md));
  there is not yet a package manager, zero-copy DataFrame/Tensor sharing, or a bundled
  interpreter.
- **No async or threads at the language level** (data-parallelism is implicit via Polars).

These limitations are tracked in [ROADMAP.md](ROADMAP.md).

---

## Staged plan

- **Performance** ([performance-roadmap.md](performance-roadmap.md)): Track A (faster
  interpreter: register VM and quickening), Track B (JIT — implemented, expanding), Track C
  (the unified fusing graph: Polars, JIT, and tensor compiler, CPU to GPU).
- **Flagship computational biology** ([ROADMAP.md](ROADMAP.md)): FASTA (done) →
  VCF→DataFrame → FASTQ/GFF/BED → BAM (mmap/streaming) → RNA/protein → Python interop
  (v1 done).
- **Adoption** ([ROADMAP §7](ROADMAP.md), [adoption.md](adoption.md)): modules (done) →
  CPython interop v1 (done) → zero-copy Arrow/DLPack sharing → package manager → Jupyter.
- **Correctness and safety:** leak-free and parity-tested at every engine boundary; ~540
  tests, zero warnings, maintained as a gate.

---

## Document index

- [ROADMAP.md](ROADMAP.md) — full roadmap, positioning, flagship plan, phase status.
- [research-notes.md](research-notes.md) — all cited research (design, interpreters,
  JITs, data/GPU frontier).
- [execution-engine.md](execution-engine.md) — the tree-walker / VM / JIT tiers.
- [performance-roadmap.md](performance-roadmap.md) — the three perf tracks, cited.
- [memory-safety.md](memory-safety.md) — the leak-freedom argument + tests.
- [benchmarks.md](benchmarks.md) — DataFrame benchmark methodology + numbers.
- [adoption.md](adoption.md) — the adoption gap analysis.
- [python-interop.md](python-interop.md) — using Python from Helix (the v1 bridge).
- [adr/](adr/) — accepted decision records (missing data, types, collections, functions,
  syntax, concurrency, tensors, CPython interop).
- `examples/` — runnable programs, incl. [genomics.helix](../examples/bio/genomics.helix)
  and [python/interop.helix](../examples/python/interop.helix).
- `scripts/` — `langbench.sh`, `vmparity.sh`, benchmark + parity harnesses.

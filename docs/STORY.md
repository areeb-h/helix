# Helix — the whole story

*The language scientists wish they had, instead of the one they learned to tolerate.*

This is the narrative spine of the project: what Helix is, why it exists, how it was
built stage by stage, what's measured, what's honestly still missing, and where it's
going. Every section links to the deep doc for that topic.

---

## What Helix is

Helix is a modern scientific programming language, implemented in safe Rust. It
combines Python's readability, R's data workflow, Rust's safety, Julia's elegance,
SQL's data operations, and Arrow's zero-copy model — *synthesized*, not copied.

**Positioning** (see [research-notes](research-notes.md), [ROADMAP](ROADMAP.md)): not a
general-purpose Python replacement, but **a scientific computing language for the
post-Pandas era**, with **computational biology as the flagship domain** — the
R-for-statistics play. The tabular/stats/array domains (data science, finance,
climate, astronomy) come nearly free off the same core; bio is the differentiated
wedge.

**Core principles:** simple low-symbol syntax; one obvious way; immutable by default;
strong static typing with heavy inference; educational errors; memory efficiency
(zero-copy/columnar/lazy); leak-free and memory-safe by construction; AI-ready.

---

## The journey, stage by stage

Each stage shipped working, tested code (the test count grew 44 → 130+).

1. **Core interpreter** — lexer → parser → AST → tree-walking interpreter. Immutable
   bindings + `mut`, Int/Float/String/Bool/Array/Dna/Function, word-based booleans,
   dot-chain method dispatch, friendly caret+hint errors, file runner + REPL.
2. **Syntax polish** (best of Python + TypeScript) — string interpolation, `??`
   coalescing, records + `.field`, Python-style slicing, tuples + destructuring,
   lambda-param destructuring, `let … in` local bindings, `if/then/else` as an
   expression, comprehension verbs `map`/`filter`/`where`/`reduce` with `it`/`acc`.
3. **`missing` model** ([ADR-0001](adr/0001-missing-data.md)) — a bottom value with
   Julia-style propagation + three-valued logic; `.is_missing()`, `.drop_missing()`.
4. **Static type checker** ([ADR-0002](adr/0002-type-system.md), [types.rs]) —
   permissive bidirectional inference, `Unknown` top type, **zero false positives**
   (every example type-checks clean — a regression-gated guarantee).
5. **DataFrame engine** ([ROADMAP §3](ROADMAP.md), [benchmarks](benchmarks.md)) — lazy
   Polars/Arrow LazyFrames; SQL-intuitive verbs; CSV/Parquet; **50M-row query ~0.2s**.
6. **Tensor engine** ([ADR-0007](adr/0007-tensor-backend.md)) — ndarray-backed, NumPy
   broadcasting, axis reductions, matmul, pure-Rust linear algebra.
7. **Leak-freedom + recursion robustness** ([memory-safety](memory-safety.md)) —
   proved leak-free by construction; 2 GiB eval thread + depth guard.
8. **Bytecode VM** ([execution-engine](execution-engine.md)) — slot-resolved variables,
   heap call frames, ~3× over the tree-walker; whole-program fallback keeps everything
   working.
9. **Cranelift JIT** ([execution-engine](execution-engine.md),
   [performance-roadmap](performance-roadmap.md)) — native codegen for the numeric core,
   now **dual-specialized** over `i64` *and* `f64`. **Beats Node and Python; near Go/C.**
10. **Numerical accuracy** — every float aggregation uses Neumaier compensated
    summation (accurate to the last ulp; see below).
11. **Genomics flagship** ([ROADMAP §Flagship](ROADMAP.md)) — real FASTA via
    `needletail`; `read_fasta`, `gc_content`, `kmers`, `find`, `Array.top`; the
    [genomics demo](../examples/genomics.helix) runs end-to-end.
12. **Module system** ([ROADMAP §7](ROADMAP.md)) — `import name` / `import lib.stats
    as st`; a loader resolves the import graph and rewrites every module into one
    namespaced AST the existing pipeline runs unchanged. Single files stay untouched.
13. **CPython interop, v1** ([ADR-0008](adr/0008-cpython-interop.md),
    [guide](python-interop.md)) — the adoption unlock. Behind a feature flag (so the
    core binary stays self-contained), Helix embeds CPython: `import python.numpy as
    np`, attribute/method calls forward to Python, scalars convert back, containers
    stay opaque until `to_array`, exceptions become Helix errors. So Helix can call
    the real scientific stack *before* it has built its own.

---

## Architecture: three engines, one surface

Helix routes each kind of work to a best-in-class engine, unified by one type system
and one value model:

- **Scalar / control-flow** → tree-walker → **bytecode VM** → **Cranelift JIT** (native).
  Tiered: the JIT handles the numeric core; anything it can't compile falls back to the
  VM; anything the VM can't compile falls back to the tree-walker. Same language, same
  results, verified by parity tests at every boundary.
- **Bulk tabular** → **Polars/Arrow** (lazy, columnar, multicore, SIMD).
- **Tensors** → **ndarray** today; a typed fusing compiler (CPU `rayon`+SIMD, GPU
  `CubeCL`) is the planned Track C — the structural edge no incumbent has.

Delegation is the strategy: Helix is the beautiful, consistent, fast, memory-safe
*surface*; Polars, ndarray, `needletail`/`noodles`, and Cranelift do the heavy lifting.

---

## Benchmarks

**`fib(35)` — pure scalar recursion (~30M calls), release, same machine** (lower = faster):

| | time | note |
|---|--:|---|
| **Helix — auto-memoized** | **0.00s** | instant — beats C |
| C (gcc -O2) | 0.01s | baseline |
| Go | 0.02s | AOT native |
| **Helix — Cranelift JIT** | **0.04s** | non-memoizable recursion |
| Node / JS (V8 JIT) | 0.08s | |
| Python 3.12 | 0.69s | |
| Helix — bytecode VM | 1.52s | |
| Helix — tree-walker | 4.65s | |

Two honest results, depending on the function:

- **For pure overlapping recursion (like `fib`): instant — Helix beats C.** Not with
  faster codegen, but by *doing less work*: it auto-memoizes (~35 calls instead of
  ~30M), which it can do safely because it *proves* `fib` is a pure function of its
  arguments. C can't auto-memoize — it can't prove side-effect-freedom — so it runs
  all 30M calls. Same source, smarter execution.
- **For non-memoizable recursion: the Cranelift JIT** — 0.04s, **38× over our own VM**,
  beating Node/V8 and Python, ~2× off Go. Float recursion hits the same native tier
  (`fibf(35.0)`: 0.05s vs 1.58s on the VM).

Reproduce: `bash scripts/langbench.sh`.

**DataFrames** ([benchmarks.md](benchmarks.md)): 50M-row filter+group+sort ~0.2s
(Parquet) on all cores; Parquet `count()` is O(1) from metadata; ~8–11× over pandas
(Polars engine). For data work — Helix's actual purpose — it already beats Python/R.

---

## Numerical accuracy ("trust your numbers")

Scientific results must be trustworthy to full precision. Two guarantees:

- **Output is exact to `f64`.** Floats print via Rust's shortest-round-tripping
  representation — e.g. `0.5916666666666667` is the exact value, not a truncation.
- **Aggregations use Neumaier compensated summation.** Naive left-to-right `f64`
  summation silently loses low-order bits when magnitudes differ; `sum`/`mean`/`std`/
  `normalize` all route through Neumaier, accurate to the last ulp even for large or
  ill-conditioned data. `std` is two-pass to avoid catastrophic cancellation.
  Regression-tested (`compensated_summation_is_accurate`).

Honest edge: integer arithmetic *wraps* on overflow (matching Rust release / most
systems languages — use floats beyond the i64 range); the JIT's float comparisons
follow IEEE-754 (NaN → `false`) rather than the interpreter's NaN-error.

---

## Memory safety

Zero `unsafe` outside **one** contained function — the JIT's native-call boundary
(`jit::call_i64`/`call_f64`), guarded by a type/arity check, dealing only in scalars
(no heap, no `Rc`). Everything else is leak-free *by construction*: no interior
mutability ⇒ no `Rc` cycles ⇒ an acyclic value graph freed deterministically. Backed
by a `Rc::strong_count` test and flat-RSS measurement. Full argument:
[memory-safety.md](memory-safety.md). For the bio flagship this is a *feature* —
"samtools speed without the segfaults."

---

## Honest limitations (what doesn't work yet)

- **JIT scope:** integer/float numeric recursion only — `+ - * /`, comparisons, `if`,
  `let`, calls, ≤4 params. No arrays/loops/`Mod`/`Pow`/strings in native code yet; those
  run on the VM/tree-walker. Forward-referenced mutual recursion doesn't compile to
  bytecode (single-pass) so it can't be JIT'd yet.
- **VM scope:** arrays, comprehensions, methods, records, tensors, DataFrames, lambdas,
  interpolation fall back to the tree-walker (correct, just not the fast path).
- **DataFrames:** no cross-statement caching (re-scans a file used twice); print
  materializes the whole frame; limited IO (no Arrow IPC/JSON/FASTA-as-frame/`write_csv`
  yet); no joins/derived columns.
- **Tensors:** no slicing/stacking, single dtype (`f64`), no GPU/autodiff yet.
- **Bio:** FASTA only so far — VCF→DataFrame, FASTQ, GFF/BED, BAM are next.
- **Types:** no `Maybe`/nullable tracking, no column-level DataFrame typing.
- **Ecosystem:** modules + a v1 Python bridge shipped ([Phase 7](ROADMAP.md)); still
  no package manager, no zero-copy DataFrame/Tensor sharing, no bundled interpreter.
- **No async/threads at the language level** (data-parallelism is implicit via Polars).

These are tracked, not hidden. See [ROADMAP.md](ROADMAP.md).

---

## How we're tackling it — the staged plan

- **Performance** ([performance-roadmap.md](performance-roadmap.md)): Track A (faster
  interpreter: register VM + quickening), **Track B (JIT — shipped, widening)**, Track C
  (the unified fusing graph: Polars + JIT + tensor compiler, CPU→GPU).
- **Flagship bio** ([ROADMAP.md](ROADMAP.md)): FASTA ✓ → VCF→DataFrame → FASTQ/GFF/BED →
  BAM (mmap/streaming) → RNA/protein → Python interop (v1 ✓).
- **Adoption** ([ROADMAP §7](ROADMAP.md), [adoption.md](adoption.md)): modules ✓ →
  CPython interop v1 ✓ → zero-copy Arrow/DLPack sharing → package manager → Jupyter.
- **Correctness/safety:** leak-free + parity-tested at every engine boundary; 130+ tests,
  zero warnings, maintained as a gate.

---

## Document index

- [ROADMAP.md](ROADMAP.md) — full roadmap, positioning, flagship plan, phase status.
- [research-notes.md](research-notes.md) — all cited research (design, interpreters,
  JITs, data/GPU frontier).
- [execution-engine.md](execution-engine.md) — the tree-walker / VM / JIT tiers.
- [performance-roadmap.md](performance-roadmap.md) — the three perf tracks, cited.
- [memory-safety.md](memory-safety.md) — the leak-freedom argument + tests.
- [benchmarks.md](benchmarks.md) — DataFrame benchmark methodology + numbers.
- [adoption.md](adoption.md) — the honest "would anyone switch?" gap analysis.
- [python-interop.md](python-interop.md) — using Python from Helix (the v1 bridge).
- [adr/](adr/) — accepted decision records (missing data, types, collections, functions,
  syntax, concurrency, tensors, CPython interop).
- `examples/` — runnable programs, incl. [genomics.helix](../examples/genomics.helix)
  and [python/interop.helix](../examples/python/interop.helix).
- `scripts/` — `langbench.sh`, `vmparity.sh`, benchmark + parity harnesses.

# ADR 0006 — Concurrency, parallelism & scale

- **Status:** proposed
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude

## Context

Helix targets data science / ML / comp-bio over **large datasets — up to
billions of records**. The question "what about threading and async?" has a
specific answer for a scientific language that differs from a general-purpose
one: scientists should get parallelism **for free**, not by managing threads.

## Decision

**Performance at scale comes from implicit data-parallelism in the engines, not
from language-level threads or `async`/`await` exposed to the user.**

Three layers:

1. **DataFrames (shipped).** Lazy `LazyFrame` plans execute **multi-threaded**
   across all cores via Polars' engine, and can stream **out-of-core** for data
   larger than RAM. A whole `where → group → sort` chain fuses into one parallel
   pass. The user writes sequential-looking verbs; the engine parallelizes.
   *(Measured: 5M-row filter+group+sort+head in ~1s on a debug build.)*

2. **Tensors (Phase 4).** The same principle — parallel/SIMD kernels, later GPU
   (Phase 6) — with no thread management in the surface language.

3. **Arrays (future).** Big in-memory array combinators (`map`/`filter`/`reduce`)
   can be parallelized with rayon behind the same API, transparently.

**`async`/`await` is deliberately *not* a core language feature.** It solves IO
concurrency, which is peripheral to compute-bound scientific work, and it
"colors" functions — a complexity tax that contradicts *one obvious way* and
*no surprises*. IO concurrency, if needed, belongs in the runtime/stdlib (e.g. a
parallel multi-file reader), not in user-facing function signatures.

## Rationale

- The dominant cost in scientific workloads is **data-parallel compute over
  columns/tensors**, which Polars/Arrow/SIMD already parallelize optimally.
  Hand-rolled threads would almost always be slower and buggier.
- Keeping threads/async out of the surface language preserves readability and
  the immutable-by-default, value-semantics model (ADR 0004) — immutable data is
  trivially safe to share across threads, which is *why* the engine can
  parallelize freely.
- This is the Polars/NumPy/Julia lesson: users express *what*, the kernels decide
  *how many cores*.

## Rejected alternatives

- **`async`/`await` in the core language** — function coloring, surprise, wrong
  problem (IO, not compute).
- **Explicit threads / a `spawn` primitive** — pushes correctness and scheduling
  onto scientists; immutable-data + engine-parallelism dominates in practice.
- **GIL-style single-threaded execution (Python)** — the very bottleneck Helix
  exists to escape; rejected outright.

## Consequences

- The engine layer (Polars now; tensor/GPU later) owns parallelism; the
  interpreter stays simple and single-threaded for control flow.
- Immutability (ADR 0001/0004) is load-bearing for safe sharing — keep it.
- A future parallel-array layer must preserve identical semantics to the serial
  one (same `map`/`filter`/`reduce` results), differing only in speed.

## Open questions

- When to expose the Polars **streaming** engine toggle (out-of-core) — always
  on past a size threshold, or an explicit `.stream()`?
- Do we ever need a structured-parallelism escape hatch (e.g. `parallel_map`)
  for embarrassingly-parallel user code that isn't a DataFrame/tensor op?

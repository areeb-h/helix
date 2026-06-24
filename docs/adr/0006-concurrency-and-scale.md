# ADR 0006 — Concurrency, parallelism & scale

- **Status:** Proposed
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude

## Context

Helix targets data science, ML, and computational biology over **large
datasets — up to billions of records**. The question of threading and async
admits a specific answer for a scientific language that differs from a
general-purpose one: scientists should obtain parallelism **implicitly**, not
by managing threads.

## Decision

**Performance at scale derives from implicit data-parallelism in the engines, not
from language-level threads or `async`/`await` exposed to the user.**

Three layers:

1. **DataFrames (shipped).** Lazy `LazyFrame` plans execute **multi-threaded**
   across all cores via the Polars engine, and can stream **out-of-core** for data
   larger than RAM. A complete `where → group → sort` chain fuses into one parallel
   pass. The user writes sequential-looking verbs; the engine parallelizes.
   *(Measured: 5M-row filter+group+sort+head in approximately 1s on a debug build.)*

2. **Tensors (Phase 4).** The same principle — parallel/SIMD kernels, later GPU
   (Phase 6) — with no thread management in the surface language.

3. **Arrays (future).** Large in-memory array combinators (`map`/`filter`/`reduce`)
   can be parallelized with rayon behind the same API, transparently.

**`async`/`await` is deliberately *not* a core language feature.** It addresses IO
concurrency, which is peripheral to compute-bound scientific work, and it
"colors" functions — a complexity cost that contradicts *one obvious way* and
*no surprises*. IO concurrency, where needed, belongs in the runtime/stdlib (for
example, a parallel multi-file reader), not in user-facing function signatures.

## Rationale

- The dominant cost in scientific workloads is **data-parallel compute over
  columns/tensors**, which Polars/Arrow/SIMD already parallelize optimally.
  Hand-written threads would in most cases be slower and more error-prone.
- Keeping threads and async out of the surface language preserves readability and
  the immutable-by-default, value-semantics model (ADR 0004). Immutable data is
  trivially safe to share across threads, which is why the engine can
  parallelize freely.
- This follows the Polars/NumPy/Julia precedent: users express *what*, and the
  kernels decide *how many cores*.

## Rejected alternatives

- **`async`/`await` in the core language** — function coloring, surprise, and the
  wrong problem (IO, not compute).
- **Explicit threads or a `spawn` primitive** — pushes correctness and scheduling
  onto scientists; immutable data plus engine-parallelism dominates in practice.
- **GIL-style single-threaded execution (Python)** — the bottleneck Helix
  exists to escape; rejected.

## Consequences

- The engine layer (Polars now; tensor/GPU later) owns parallelism; the
  interpreter remains simple and single-threaded for control flow.
- Immutability (ADR 0001/0004) is essential for safe sharing and must be retained.
- A future parallel-array layer must preserve identical semantics to the serial
  one (the same `map`/`filter`/`reduce` results), differing only in speed.

## Open questions

- When to expose the Polars **streaming** engine toggle (out-of-core) — always
  on past a size threshold, or an explicit `.stream()`?
- Whether a structured-parallelism escape hatch (for example, `parallel_map`) is
  needed for embarrassingly-parallel user code that is not a DataFrame/tensor op.

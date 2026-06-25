# ADR 0012 — DataFrame backend seam: decouple the language from the engine

- **Status:** Accepted (Phase 1 implemented)
- **Date:** 2026-06-25
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0003 — Collection API](0003-collection-api.md),
  [ADR 0008 — CPython interop](0008-cpython-interop.md),
  [ADR 0011 — Core/stdlib boundary](0011-core-stdlib-boundary.md)

## Context

DataFrames are a flagship Helix value (ADR 0003: the same `where`/`select`/`sort`/
`group` verbs span arrays and tables). They were implemented directly against
**Polars**: `Value::DataFrame` held an `Rc<LazyFrame>`, and the verb→engine
translation (`to_polars`) plus every operation lived in `src/dataframe.rs`, calling
`polars::` types throughout. The interpreter, VM, type checker, and `Value` all
touched Polars indirectly.

The strategic question: for a **serious** scientific language, is the right data-layer
move to (a) build a homegrown engine, (b) lean harder on Polars, or (c) lean on the
CPython/pandas ecosystem? Two cited research passes plus a measured prototype settled
it, and corrected two earlier in-house conclusions:

- **CPython-glue: rejected, decisively.** Leaning harder on Python contradicts all
  three load-bearing commitments (serious, self-contained, local-first), makes the
  stack three languages (Helix → CPython → C), pushes hot loops across the slow PyO3
  boundary, and turns Helix into "a fourth way to write Python." The serious-language
  consensus (Julia, R, Mojo-by-design) is: **own your compute core; interop, don't be
  parasitic.** This reaffirms ADR 0008 — native-first, CPython an off-by-default escape
  hatch. Lean *less*, not more.
- **Homegrown-now: not yet.** A time-boxed prototype (`experiments/dfbench/`) proved it
  viable — perf-competitive on filter/group/sort, ~140× smaller binary, reuses Helix's
  `stats.rs` kernels and the Cranelift `reduce_loop` scaffold — but building it *now*
  forfeits Polars's maturity, streaming, and Parquet for months.
- **The decisive fact: Polars's Rust API is officially unstable** (0.x, undeprecated
  breaks every 3–6 months, no upgrade guides). Helix pays that churn tax whichever
  engine it uses. Combined with the fact that Helix *already* lowered its verbs through
  an AST (`to_polars`), one move dominates.

## Decision

**Decouple Helix's DataFrame semantics from the engine** behind a thin, object-safe
`DataHandle` trait plus a backend-agnostic, typed column-expression IR (`ColExpr`) —
the Ibis pattern. `Value::DataFrame` now carries an engine-agnostic `Df =
Rc<dyn DataHandle>`; **no `polars::` type escapes `src/backend/polars.rs`.**

- **`src/backend/mod.rs`** — the `DataHandle` trait, the `ColExpr` IR, the verb→engine
  front half (`ast_to_colexpr`, which owns the friendly "no column or variable"
  diagnostics), and shared engine-agnostic validation (`validate_join_keys`) so every
  backend yields identical Helix errors.
- **`src/backend/polars.rs`** — `PolarsFrame: DataHandle`, the default (and today only)
  backend. `to_polars` became its private `ColExpr → polars::Expr` lowering; the
  readers and verbs became trait methods over `LazyFrame`. All operations stay lazy and
  fuse at a single `collect()` materialization point.
- **`src/dataframe.rs`** — reduced to a thin shim re-exporting the seam's names and the
  active backend's readers, so the interpreter/VM keep a stable `dataframe::` surface.

This insulates Polars's API churn (a break touches one file), captures the unused
`LazyFrame` optimizer/streaming headroom, and turns "go homegrown later" into *adding a
backend*, never a language rewrite.

### Rejected alternatives

- **Hard-wire Polars forever** — bets the language on an unstable 0.x Rust API.
- **DuckDB as *the* engine** — C++ FFI breaks pure-Rust single-binary + fast
  cross-compile; keep it as a possible *optional* backend for out-of-core SQL, not the
  default.
- **DataFusion as default** — mandatory async/tokio + ~92 MB fights self-contained /
  fast-start.
- **Homegrown now / CPython-glue** — see Context.
- **A full lazy `Plan` IR materialized only at the sink** (vs. method-per-verb on the
  handle) — rejected for Phase 1: the verbs validate *eagerly* (a bad column or join
  key errors at the call site, not at `collect`), and a lower-at-materialization model
  would have changed that error-timing behavior. The `DataHandle` trait object
  preserves eager validation exactly while still confining the engine.

### Why a backend abstraction is insurance, not over-engineering

The seam mostly existed already (`to_polars` was a verb→engine lowering), so the
marginal cost was low — the condition under which the abstraction pays off. It
collapses two separate risks (Polars churn, future engine swap) into one localized
problem. The leak is mitigated Ibis-style — a future capability probe with a loud error
for backend gaps; we do **not** promise "any engine, identical semantics."

## Binary size — honest status

The motivation for an eventual engine swap includes the ~65 MB binary Polars forces
(see [binary-size.md](../binary-size.md)). Measured on the pinned **Polars 0.54.4**,
the `csv`/`parquet` features transitively force the streaming engine, which pulls the
async/cloud tail (`object_store` → `reqwest`/`hyper`/`tokio`) **unconditionally** — so
slimming features on 0.54.4 cannot shed it without dropping file I/O. Upstream has since
made `object_store` optional and treats the unconditional pull as a bug. The honest path
is therefore: track/contribute the upstream fix, and treat the homegrown-Cranelift
backend (now a *backend swap*, not a rewrite, thanks to this seam) as the real route to
the size win. No `[patch]` hack.

## Consequences

- **Behavior-preserving:** the entire existing DataFrame/stats/example suite passes
  unchanged across both the tree-walker and the VM. No user-visible change.
- `Value` and the interpreter/VM/type checker no longer reference `polars::` at all.
- **Future phases (behind this seam, not this milestone):** a capability matrix +
  `supports()` probe; a homegrown-Cranelift backend promoted to default once it reaches
  oracle parity verb-by-verb; an optional DuckDB backend for out-of-core SQL.

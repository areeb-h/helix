# ADR 0002 — Type system & inference

- **Status:** Accepted — first iteration implemented (`src/types.rs`)
- **Date:** 2026-06-21 (implemented 2026-06-22)
- **Deciders:** Areeb + Claude
- **Research:** [Domain 2](../research/2026-06-21-foundational-design.md#domain-2--type-system--inference) (high confidence on gradual-typing & Frames; 3-0 verified)

## Context

Helix requires *strong static typing* with *heavy inference* (users rarely write
types), *great error messages*, **and** the ability to open an arbitrary
CSV/Parquet whose schema is unknown until runtime. These goals are in tension:
maximal inference (global HM) yields poor errors; full static typing cannot see a
runtime schema; fully dynamic typing forfeits the guarantees.

## Prior approaches and their documented shortcomings

- **Global Hindley-Milner (OCaml/Haskell):** complete inference, but unification
  failures surface *far from their cause*, producing non-local error messages.
- **Localized/bidirectional inference (Rust, TypeScript):** trades a degree of
  inference completeness for errors anchored at the mismatch site.
- **Sound gradual typing (Typed Racket):** Takikawa et al. (POPL 2016) measured
  **non-monotonic slowdowns up to 105x** from per-boundary runtime contracts —
  adding types can make code substantially slower before performance recovers.
  Bauman et al. (OOPSLA 2017) showed a tracing JIT (Pycket) **recovers >90%** of
  that overhead, indicating the regression is an implementation artifact rather
  than a fundamental one.
- **Frames (Haskell):** statically types a DataFrame by inspecting a CSV **at
  compile time** (Template Haskell) for true column-access safety — but only from
  a *sample file*. It cannot type a Parquet file first seen at runtime.

## Decision

**A strong static core with localized/bidirectional inference, plus a deliberate,
coarse boundary where runtime-schema DataFrames cross into checked-dynamic
territory. No fine-grained sound-gradual boundary contracts.**

1. **Inference:** localized/bidirectional (Rust/TS tradition), *not* whole-program
   HM. Prioritize educational, locally-anchored errors over inference
   completeness. This is the same value system as Helix's existing error work.
2. **Known-schema data** (literals, files sampled at build time): fully static; a
   Frames-style generator can give compile-time column safety.
3. **Runtime-schema DataFrames:** the value is statically a `DataFrame`; its
   **column schema is a runtime value** validated at the load boundary and at
   first column use, producing Helix's characteristic errors:

   ```text
   error: column `age` not found in this DataFrame
     --> study.helix:12:18
   help: available columns: age_years, sex, diagnosis
         did you mean `age_years`?
   ```

4. **Keep the boundary coarse:** validate the schema **once** at load, never
   per-value. This is how the 105x regressions are avoided — the Phase 5 JIT
   never has to recover per-crossing contract overhead.

## Rationale

- Localized inference is the only choice consistent with "great error messages";
  the research directly ties global HM to non-local errors.
- A coarse load-time schema check provides most of static typing's value
  (typo-proof column access, typed operations) without the gradual-typing
  performance regression.
- Treating `DataFrame` as statically typed while its columns are
  dynamic-but-validated is the accurate model: the schema genuinely *is* a
  runtime fact for `read_csv("unknown.csv")`.

## Rejected alternatives

- **Whole-program Hindley-Milner** — non-local errors; violates the errors
  constraint.
- **Fine-grained sound gradual typing** — documented 105x non-monotonic
  slowdowns; incompatible with the JIT roadmap.
- **Frames-style compile-time typing as the *only* model** — can't type a runtime
  Parquet file. Adopt it for the static case only.
- **Fully dynamic typing (Python/R)** — forfeits strong static guarantees.

## Consequences

- The type checker (Phase 2) is bidirectional from day one — shapes the AST's
  type-annotation slots and the inference engine.
- DataFrame column access compiles to a checked operation against a runtime
  `Schema`; the "did you mean" machinery from Phase 1 errors is reused at the
  schema boundary.
- A future optional "schema pin" facility (assert a file's schema at build time)
  can layer on for users who want full static column types.

## First iteration as implemented (2026-06-22)

A bidirectional, **permissive** checker in `src/types.rs`, wired into `run_source`
and the REPL (before interpretation). A `Type` lattice with `Num` (numeric
supertype), `Missing` (bottom), and `Unknown` (top); `compatible`/`join` where
`Unknown`/`Missing` never error. It catches undefined names, unknown
functions/methods (with "did you mean"), wrong arity, `String + Int`, non-boolean
`if`/`and`/`or`, non-integer indexing, and return-annotation mismatches — all
before execution, with the existing caret errors. **Zero false positives**: all 8
example programs and the 68 runtime tests type-check cleanly (the example
regression is a unit test). Optional `fn f(x: Int) -> Int` annotations.

### Decisions made during implementation (resolving prior open questions)

- **`missing` maps to a bottom `Missing` type, not `Maybe`/`Int?`.** Nullable
  tracking is deferred; the permissive model cannot *enforce* the missing branch.
  `missing + Int` types as `Missing` (matching the runtime value); arrays with
  holes type by `join` (Missing drops). Full `Maybe` support is future work.
- **Mutability / immutable-reassignment remains a runtime check**, not a type
  error — the runtime error is already effective and tested, and duplicating it
  risks divergence.
- **Rebinding a variable to a new type is allowed** (mirroring the dynamic env);
  single-type-per-binding is a possible future opt-in for immutable bindings only.
- **DataFrame/GroupBy verb arguments are unchecked** — confirming the coarse
  boundary: column names/predicates in `where`/`select`/`group` are never typed
  (they constitute the runtime schema boundary). A `DataFrame` is an opaque type.

## Open questions (remaining)

- Column-level typing *within* a pipeline after the load boundary validates the
  schema (so `.select(age).mean()` knows `age` is numeric), and an optional
  compile-time "schema pin" for known files.
- Bidirectional refinement: flow expected/element types into lambdas; tighten
  Array-arithmetic and `matmul`/`dot` returns beyond `Unknown`.
- If/when to introduce real `Maybe`/`Int?` and exhaustive missing-handling.

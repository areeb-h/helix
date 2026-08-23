# ADR 0034 — Native frame semantics: frames follow the language

- **Status:** **Accepted & Implemented 2026-08-23** — the policy contract ADR 0033's Stage 1
  implements. Written BEFORE the engine so the differential campaign against the
  polars oracle has decided answers, not ad-hoc ones.
- **Date:** 2026-08-23
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0033](0033-native-dataframe-engine.md) (the staged replacement
  this governs), [ADR 0001] (missing propagation), [ADR 0025](0025-ordering.md)
  (ordering doctrine), [ADR 0024](0024-total-runtime.md) (errors, never aborts).

## The principle

**A column expression means exactly what the same expression means on scalars.**
`df.where(x % 2 == 1)` and `x % 2 == 1` are one semantics, not two dialects — the
native engine evaluates cells through `interp::ops::eval_binary`/`eval_unary`, the
interpreter's OWN scalar kernel, so drift between frame-land and scalar-land is
structurally impossible, not merely tested against. (A typed fast path may replace
the kernel per column later — behind the same differential tests, Stage 3.)

## Decided policies

1. **Arithmetic follows scalars** — three deltas vs the polars backend, measured
   on v0.3.0 and visible only at the Stage-4 flip (release-noted then):
   `%` is euclidean everywhere (`7 % -3` is `1`; polars gave `-2`); `/` is true
   division and yields Float (`2 / 2` is `1.0`; polars kept Int); division and
   modulo **by zero are errors** naming the row (`division by zero at row 2`;
   polars gave `missing` — an unknown that was actually a known bug in the data).
2. **Missing propagates per ADR 0001**, elementwise. A `where` predicate that
   evaluates to `missing` keeps the row out — the same observable outcome the
   polars backend has today.
3. **Aggregations keep the armor's hard-won doctrine** (it was Helix policy forced
   onto polars; now it is just code): `count` counts rows INCLUDING missing;
   `mean`/`sum`/`min`/`max`/`std` (sample, ddof 1) **propagate missing** — an
   all-missing group is unknown, not zero. Group output order is first-seen.
   Float `sum`/`mean` are left-to-right in row order for Stage 1 — bit-matching
   the oracle's sequential kernel — and upgrade to Neumaier compensated summation
   AT THE FLIP (more accurate and still deterministic; deferred only so the
   differential campaign stays byte-exact; release-noted at the flip).
4. **Sort**: stable, multi-key, ascending, missing first. **unique()**: whole-row
   distinct keeps the FIRST occurrence; `unique(keys…)` keeps the LAST (upsert —
   newest wins) — both are today's tested behavior, preserved.
5. **Join**: `inner` (default) / `left` / `right` / `outer`; key columns coalesce
   into one; colliding non-key right columns get `_right`; output order is
   left-then-right read order; missing keys never match.
6. **vstack** requires identical column names AND order (today's rule) — and the
   native engine also checks DTYPES eagerly at the call, a strict improvement over
   the polars backend's late materialization error (delta, recorded).
7. **with** replaces an existing column in place or appends a new one; a scalar
   expression broadcasts. **select** projects in the asked order. **head** clamps.
   **cache** is the identity (eager engines are their own cache).
8. **CSV**: header row required (today's rule); dtype inference over the first 100
   records per column — Int ⊂ Float ⊂ Str lattice, `true`/`false` make Bool, an
   empty field is missing, no date parsing (dates stay strings, today's rule);
   RFC 4180 quoting both directions; `write_csv` emits comma-separated, minimal
   quoting, missing as an empty field. Corner cases (leading zeros, `1e5`, `NaN`,
   whitespace, a post-window contradiction) are settled by running the ORACLE and
   pinning the answer as a test — never by guessing.

## What the differential campaign compares

Every policy above is exercised by dual-backend tests (both engines in one dev
binary) plus the corpus against polars-frozen `.expected` files. A divergence is a
bug in the native engine UNLESS it is one of the numbered deltas above — those are
asserted AS deltas (the test proves the divergence is exactly the decided one).

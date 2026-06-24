# ADR 0003 — Collection API unity

- **Status:** Proposed
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude
- **Research:** [Domain 3](../research/2026-06-21-foundational-design.md#domain-3--collection-api-unity--one-obvious-way) (pandas anti-pattern high/3-0; synthesis medium)

## Context

"One obvious way" is most at risk in the collection API, because Helix has four
collection-like types — `Array`, `DataFrame`, `Tensor`, `Dna` — and the natural
temptation is to give each its own bespoke verbs. pandas illustrates the hazard:
`loc`/`iloc`/`at`/chained indexing, view-vs-copy ambiguity, and per-dtype quirks.
The research **explicitly affirms** Helix's already-shipped `where == filter`
decision as the correct governing principle of the entire collection API.

## Prior approaches and their documented shortcomings

- **pandas** — multiple overlapping indexers and sentinels; users cannot predict
  view vs copy or whether an assignment persists, producing silent bugs and a
  lore-heavy learning curve. The archetypal "many obvious ways" failure.
- **dplyr / LINQ / Rust iterators** — consistent, composable verb vocabularies;
  the positive model (medium confidence — synthesis, not independently verified).
- **Julia multiple dispatch** — one generic API across array types via dispatch.
- **Arrow compute kernels** — a uniform columnar operation substrate.

## Decision

**One verb protocol — a Rust-trait-style `Collection` protocol — implemented by
every collection type, so each verb means the same thing everywhere. One verb per
concept; aliases forbidden.**

```text
# identical surface, type-specific zero-copy engine underneath
numbers.where(it > 0).map(it * 2).sum()        # Array  — eager
patients.where(age > 40).select(name).sort(age)# DataFrame — lazy, Arrow kernels
image.where(it > 0.5).map(it * 255)            # Tensor  — SIMD/GPU later
seq.where(it == "G").len()                      # Dna
```

- **One verb per concept, everywhere.** `where == filter` already collapsed to
  one operation; this discipline extends so that `map`, `reduce`, `group`,
  `sort`, and `select` each have exactly one spelling across all four types,
  without `loc`/`iloc` proliferation.
- **Abstraction = static traits.** Each type implements the protocol; verbs
  resolve to type-specific implementations (Arrow kernels for DataFrame, SIMD for
  Tensor) behind one identical surface. Static dispatch suits the JIT/GPU
  roadmap better than dynamic multiple dispatch.
- **Lazy/columnar by default for DataFrame/Tensor.** The same chain that is eager
  on a small `Array` becomes a fused lazy plan on a big `DataFrame` — same syntax,
  different engine.
- **Naming discipline as a written rule.** A verb joins the protocol only if no
  existing verb expresses the concept. This rule is the structural defense
  against API sprawl, enforced in review.

## Rationale

- Directly extends a decision the research validated, and keeps the language's
  most user-facing surface predictable.
- Static traits provide Julia-like "one API across types" while preserving static
  guarantees and JIT specialization.
- Lazy-by-default on large collections is where the zero-copy/lazy principle
  delivers its benefit, without a second syntax.

## Rejected alternatives

- **Per-type bespoke APIs (pandas)** — overlapping inconsistent verbs, view/copy
  ambiguity.
- **Synonymous verbs for convenience** — already rejected by `where == filter`;
  keep the discipline.
- **Untyped duck-typed protocol** — forfeits static guarantees and JIT
  specialization.
- **Dynamic multiple dispatch (Julia-style) as the core mechanism** — capable,
  but static trait dispatch is a better fit for Helix's compile-time-checking and
  JIT goals. To be revisited only if generic-over-types ergonomics require it.

## Consequences

- Phase 1's `Array` methods (`map`/`filter`/`where`/`reduce`/`sort`/…) become the
  *reference implementation* of the `Collection` protocol; Phase 3 `DataFrame`
  and Phase 4 `Tensor` implement the same trait.
- The interpreter's method dispatch generalizes from "match on value type" to
  "resolve protocol verb"; the comprehension methods already special-cased in
  `eval` (`map`/`filter`/`where`/`reduce`) are the seed.
- Verbs that genuinely differ by type (e.g. `gc_content` on `Dna`) stay
  type-specific and are *not* forced into the shared protocol.

## Open questions

- Static trait dispatch vs a hybrid with limited multiple dispatch for
  binary operations across types (e.g. `array + tensor`)?
- Exact lazy/eager boundary — is `Array` always eager, or lazy past a size
  threshold? How is a lazy chain forced (`.collect()`?) and is that the one
  obvious way?
- How do `it`/`acc` element binding and column-name references (`where(age > 40)`)
  unify — is a DataFrame row an implicit scope where column names *are* the `it`
  fields? (Ties to [ADR 0002](0002-type-system.md).)

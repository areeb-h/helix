# ADR 0001 — Missing data & absence

- **Status:** proposed
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude
- **Research:** [Domain 1](../research/2026-06-21-foundational-design.md#domain-1--missing-data--absence) (high confidence, 3-0 verified)

## Context

A scientific language lives or dies on how it represents "no value." This is the
single most consequential and least reversible decision: it touches every scalar,
every column, every aggregation. Helix's constraints — *no surprises*, *one
obvious way*, *strong static typing*, *zero-copy columnar storage* — sharply
narrow the options.

There are two physical worlds that must feel like one to the user:
- **Scalars** want a compile-time-checked tagged absence (Option/Maybe).
- **Columns** *need* a runtime validity bitmap for zero-copy/SIMD efficiency.

## What others did, and what went wrong

| Approach | Who | Documented pain |
|---|---|---|
| Null references | ALGOL W → Java, C, … | Hoare's "billion-dollar mistake": unchecked deref → crashes/CVEs |
| Reuse float `NaN` as missing | pandas (default) | int/bool columns can't hold it → silent **int→float coercion**; `NaN != NaN` breaks equality |
| Multiple per-type sentinels | pandas `NaN`/`None`/`NaT` | three incompatible markers; "which one?" lore |
| Dedicated `missing` value | Julia, R `NA`, SQL `NULL` | battle-tested; propagation is a *policy* the caller must understand |
| Validity bitmap | Apache Arrow | the right columnar substrate (1 = present, 0 = null) |

Hoare's own prescribed fix (1965!): represent absence as a **tagged union with a
discrimination test**, checked at compile time. That is the Option/Maybe pattern.

## Decision

**One `missing` value with one user-visible semantics, two physical
representations chosen by the compiler.**

- **Scalars:** absence is a tagged union (an `Option`-equivalent). Inference
  hides the type; the type checker *forces* you to handle the missing branch.
- **Columns:** absence is physically the **Arrow validity bitmap** — zero-copy,
  SIMD-friendly, ecosystem-interoperable.
- `missing` is its **own value, distinct from float `NaN`**, so an `Int` column
  with a hole stays an `Int` column.

```text
age = 41              # Int
age = missing         # also valid; age is now Int? (maybe-Int), inferred

age.is_missing()      # the ONE way to test — never `== missing`
missing == missing    # -> missing   (NOT true; equality propagates)

missing + 1           # -> missing    (math propagates)
true or missing       # -> true       (3-valued logic, short-circuits)
false or missing      # -> missing

# Aggregations make the missing policy EXPLICIT — no silent dropping:
column.mean()                 # -> missing if any value is missing (safe default)
column.drop_missing().mean()  # opt out, visibly
```

**Semantics (adopting Julia's verified rules = SQL NULL / R NA):**
- Math propagates: any op touching `missing` yields `missing`.
- Equality propagates: `missing == missing` → `missing`. Test with
  `.is_missing()`.
- Booleans: short-circuiting three-valued logic, composing with Helix's existing
  word-booleans and no-truthiness rule.
- Aggregations: **propagate by default** (a hole makes the result `missing`);
  `.drop_missing()` is the explicit, visible opt-out. This makes the "one obvious
  way" the *safe* way and prevents silent data loss.

## Rationale

- Satisfies *no surprises* (Hoare-safe, compile-time-forced handling) **and**
  *zero-copy* (Arrow bitmap) simultaneously — the two-world tension is resolved
  by separating representation from semantics.
- Distinct `missing` ≠ `NaN` kills pandas' int→float coercion class of bugs
  outright.
- Propagate-by-default aggregation is the conservative choice: a missing result
  is loud and correct; silent skipping (R/pandas default) hides data loss.

## Rejected alternatives

- **Null/nil references** — Hoare's mistake; defeats compile-time safety.
- **Float `NaN` as the marker** — type widening, no int/bool missing, conflates
  "not a number" with "no value."
- **Multiple per-type sentinels** — the pandas mess; violates one obvious way.
- **Two unrelated mechanisms for scalars vs columns** — reproduces the split;
  we expose one semantics with two representations instead.

## Consequences

- The scalar `Option`-equivalent and the column bitmap must share **one trait**
  so `.is_missing()`, propagation, and `drop_missing()` are written once.
- `missing` (absent data) must stay **distinct from `Result`-error** (failed
  operation) — see [ADR 0004](0004-functions-errors-mutability.md). A column may
  hold `missing`; it does not hold "errors."
- Phase 1's `Int/Float/...` value enum gains a maybe-wrapper or nullable flag;
  comparison/arith operators (already centralized in `interp.rs`) get a
  `missing`-propagation path.

## Open questions

- Confirm propagate-by-default for aggregations vs an explicit per-call policy
  argument. (Research flagged this as the key undecided policy.)
- Can a DataFrame column be statically `Int` vs `Int?` (known-non-null vs
  nullable), and does the validity bitmap's absence (Arrow allows omitting it
  when null-count is 0) drive that distinction?

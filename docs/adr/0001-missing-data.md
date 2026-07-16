# ADR 0001 — Missing data & absence

- **Status:** Proposed
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude
- **Research:** [Domain 1](../research/2026-06-21-foundational-design.md#domain-1--missing-data--absence) (high confidence, 3-0 verified)

## Context

The representation of "no value" is among the most consequential and least
reversible decisions in a scientific language: it touches every scalar, every
column, and every aggregation. Helix's constraints — *no surprises*, *one obvious
way*, *strong static typing*, *zero-copy columnar storage* — sharply narrow the
options.

Two physical representations must present a single semantics to the user:
- **Scalars** require a compile-time-checked tagged absence (Option/Maybe).
- **Columns** require a runtime validity bitmap for zero-copy/SIMD efficiency.

## Prior approaches and their documented shortcomings

| Approach | Who | Documented pain |
|---|---|---|
| Null references | ALGOL W → Java, C, … | Hoare's "billion-dollar mistake": unchecked deref → crashes/CVEs |
| Reuse float `NaN` as missing | pandas (default) | int/bool columns can't hold it → silent **int→float coercion**; `NaN != NaN` breaks equality |
| Multiple per-type sentinels | pandas `NaN`/`None`/`NaT` | three incompatible markers; "which one?" lore |
| Dedicated `missing` value | Julia, R `NA`, SQL `NULL` | well-established; propagation is a *policy* the caller must understand |
| Validity bitmap | Apache Arrow | the appropriate columnar substrate (1 = present, 0 = null) |

Hoare's prescribed remedy (1965) is to represent absence as a **tagged union with
a discrimination test**, checked at compile time. This is the Option/Maybe
pattern.

## Decision

**One `missing` value with one user-visible semantics, two physical
representations chosen by the compiler.**

- **Scalars:** absence is a tagged union (an `Option`-equivalent). Inference
  hides the type; the type checker *requires* handling of the missing branch.
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

missing.field         # -> missing    (field/index access propagates)
missing.phred().mean()# -> missing    (method calls propagate; `is_missing` excepted)

# Aggregations make the missing policy EXPLICIT — no silent dropping:
column.mean()                 # -> missing if any value is missing (safe default)
column.drop_missing().mean()  # opt out, visibly
```

**Semantics (adopting Julia's verified rules = SQL NULL / R NA):**
- Math propagates: any op touching `missing` yields `missing`.
- Equality propagates: `missing == missing` → `missing`. Test with
  `.is_missing()`.
- Access propagates: field access, indexing, and **method calls** on `missing`
  all yield `missing` (so `read.qual.phred().mean()` on a quality-less read is
  `missing`, not an error). `.is_missing()` is the sole exception — it always
  answers truthfully.
- Booleans: short-circuiting three-valued logic, composing with Helix's existing
  word-booleans and no-truthiness rule.
- Aggregations: **propagate by default** (a hole makes the result `missing`);
  `.drop_missing()` is the explicit, visible opt-out. This makes the "one obvious
  way" the *safe* way and prevents silent data loss.

## Rationale

- Satisfies *no surprises* (Hoare-safe, compile-time-enforced handling) **and**
  *zero-copy* (Arrow bitmap) simultaneously — the tension between the two
  representations is resolved by separating representation from semantics.
- A distinct `missing` (not equal to `NaN`) eliminates pandas' class of
  int→float coercion bugs entirely.
- Propagate-by-default aggregation is the conservative choice: a missing result
  is explicit and correct, whereas silent skipping (the R/pandas default) hides
  data loss.

## Rejected alternatives

- **Null/nil references** — Hoare's mistake; defeats compile-time safety.
- **Float `NaN` as the marker** — type widening, no int/bool missing, conflates
  "not a number" with "no value."
- **Multiple per-type sentinels** — the pandas approach; violates one obvious
  way.
- **Two unrelated mechanisms for scalars vs columns** — reproduces the split.
  Helix exposes one semantics with two representations instead.

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

## Amendment (2026-07-17) — equality is three-valued at any depth; set operations use identity

Two equalities, one rule each (implemented in `ops::eq3` / `ops::values_equal`,
shared by every engine):

- **The `==`/`!=` operators are three-valued at ANY depth.** A `missing`
  compared against anything — including inside an array/tuple/record/dict —
  makes the answer unknown, so the whole comparison yields `missing`, UNLESS a
  definite structural difference decides first (Kleene: `{a: 1, b: missing} ==
  {a: 2, b: missing}` is `false` because `a` differs; swap the 2 for a 1 and
  it is `missing`). Previously a nested `missing` compared as *definitely
  unequal*, so `{a: missing} == {a: missing}` was `false` while the top-level
  `missing == missing` was `missing` — internally inconsistent.
- **Set-like operations use total IDENTITY equality** (Julia's `isequal`
  convention): `unique`, `frequencies`, `contains`, `index_of`, and alignment
  treat every `missing` as one identity (`[missing, missing].unique()` is
  `[missing]`; `xs.contains(missing)` answers whether a missing is present).
  These must stay total — a filter predicate cannot act on a `missing` bool.
- **Floats keep IEEE semantics in both**: `NaN != NaN`, so `unique` does not
  collapse NaNs and `[1.0, NaN] == [1.0, NaN]` stays `false`. (This
  deliberately deviates from Julia's `isequal(NaN, NaN) == true` — one float
  equality everywhere beats a second float-identity rule.)
- **Tuples order lexicographically** (`(1, 2) < (1, 3)`; equal prefix falls to
  length), with the same three-valued rule: a `missing` in the deciding prefix
  yields `missing`; an unorderable pair (Int vs Str, NaN) errors exactly like
  the scalar comparison would.
- **Duplicate record fields are a parse error** (`{a: 1, a: 2}`): equality is
  order-independent and assumes one entry per field — two "equal" records could
  otherwise disagree on `.a`. Derive changed records with `{ ...base, a: v }`
  (whose update list keeps last-wins).

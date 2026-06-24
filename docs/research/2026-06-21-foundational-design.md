# Foundational Design Decisions for Helix — Cited Research Report

> Generated 2026-06-21 by a multi-agent deep-research run: 5 search angles → 25
> sources fetched → 116 claims extracted → **top 25 claims put through 3-vote
> adversarial verification → 23 confirmed, 2 refuted**. Confidence per finding
> reflects vote margin and source quality. The two refuted claims are noted
> inline as a credibility signal.

This report covers four foundational domains; each gives (a) what existing
languages/libraries did, (b) the documented pain, and (c) a recommended approach
for Helix with rejected alternatives. The decisions distilled from it live in
[../adr/](../adr/).

---

## Domain 1 — Missing Data & Absence

### What existing languages/libraries did

**Null references (the original sin).** Tony Hoare invented the null reference in
1965 in ALGOL W, "simply because it was so easy to implement," despite designing
a type system whose goal was that "all use of references should be absolutely
safe" ([InfoQ keynote](https://www.infoq.com/presentations/Null-References-The-Billion-Dollar-Mistake-Tony-Hoare/)).
**Confidence: high** (3-0, primary). He later called it his "billion dollar
mistake" ([same](https://www.infoq.com/presentations/Null-References-The-Billion-Dollar-Mistake-Tony-Hoare/)).
**Confidence: high** (3-0). The core defect: every reference must be null-checked
or the program risks disaster. Hoare himself prescribed the fix: represent
absence with **disjoint (tagged) unions plus discrimination tests**, moving
checking to compile time — the Option/Maybe pattern of Rust, Haskell, Swift.
**Confidence: high** (3-0, primary).

**Julia's `missing`.** Julia represents statistical missingness with `missing`,
the singleton of type `Missing`, explicitly equated to SQL `NULL` and R `NA` — a
dedicated sentinel, not a reused numeric NaN ([Julia manual](https://docs.julialang.org/en/v1/manual/missing/)).
**Confidence: high** (3-0, primary). Semantics:
- **Math propagates:** `missing + 1` → `missing`, `abs(missing)` → `missing`.
  **Confidence: high** (3-0).
- **Equality propagates:** `missing == missing` → `missing` (not `true`), so
  missingness must be tested with `ismissing(x)`, never `==`. `===`/`isequal`
  always return `Bool` and treat `missing` as equal to `missing`; `isless`
  orders `missing` greater than all values. **Confidence: high** (3-0).
- **Three-valued logic, short-circuiting:** `true | missing` → `true`;
  `false | missing` → `missing`. Follows SQL NULL / R NA. **Confidence: high**
  (3-0). *(A competing claim mis-stating the short-circuit case was refuted 0-3.)*

**Pandas' inconsistency (the cautionary tale).** Pandas uses different sentinels
per dtype — `np.nan` (a float) and `None` for object dtype, `pd.NaT` for
datetime ([Van den Bossche, pandas core dev](https://jorisvandenbossche.github.io/blog/2019/11/30/pandas-consistent-missing-values/)).
**Confidence: high** (3-0, primary). Worse, default integer/boolean columns
cannot hold missing values, so a single missing value silently coerces the whole
column to float ([same](https://jorisvandenbossche.github.io/blog/2019/11/30/pandas-consistent-missing-values/)).
**Confidence: high** (3-0). The fix pandas adopted is the **masked-array /
validity-bitmap** approach.

**Arrow's validity bitmap (the columnar substrate).** Apache Arrow encodes
missing values with a **separate dedicated validity bitmap buffer**, one bit per
element across all types except unions; a set bit (1) = present, unset (0) =
null ([Arrow spec](https://arrow.apache.org/docs/format/Columnar.html)).
**Confidence: high** (3-0, primary).

### The documented pain
1. **Null:** unchecked dereferences → runtime crashes and vulnerabilities.
2. **Pandas:** int→float coercion on first NaN, three incompatible sentinels,
   `np.nan != np.nan` breaking equality — bugs pandas devs themselves set out to
   fix with `pd.NA`.
3. **Two-world tension:** scalars want a compile-time-checked `Option`; columns
   *need* a runtime validity bitmap for zero-copy/SIMD. A naive design uses two
   unrelated mechanisms, reproducing the pandas split.

### Recommended approach for Helix
**One dedicated `missing` value, backed by Arrow validity bitmaps for columns and
a tagged `Option`-equivalent for scalars, unified under one user-visible
semantics.**
- **Scalars:** absence is a tagged union (Option/Maybe), checked at compile time,
  never an in-band null. Heavy inference means users rarely write the type, but
  the compiler forces handling the missing branch.
- **Columns:** absence is physically the Arrow validity bitmap — zero-copy,
  SIMD-friendly, ecosystem-interoperable for free.
- **One surface semantics:** `missing` is a single value distinct from float
  `NaN`, so int/bool columns keep their type when missing appears — avoiding
  pandas' coercion.
- **Operator semantics (Julia's battle-tested rules = SQL NULL / R NA):** math
  propagates; equality propagates (test via `is_missing`, the one obvious way);
  booleans use short-circuiting three-valued logic (`true or missing` → `true`),
  composing with Helix's word-booleans + no-truthiness; aggregations require an
  **explicit** missing policy rather than silently dropping.

### Rejected alternatives
- *Null/nil references* — Hoare's own billion-dollar mistake; breaks
  compile-time safety.
- *Reusing float `NaN`* — forces type widening, can't represent missing in
  int/bool, conflates "not a number" with "no value."
- *Multiple per-type sentinels (`NaN`/`None`/`NaT`)* — the pandas mess; violates
  one obvious way.
- *Two unrelated mechanisms for scalars vs columns* — reproduces the split;
  expose one semantics with two physical representations.

---

## Domain 2 — Type System & Inference

### What existing languages/libraries did

**Gradual typing's performance cliff.** Takikawa et al., *"Is Sound Gradual
Typing Dead?"* (POPL 2016), benchmarked all gradual configurations of Typed
Racket and concluded the cost of soundness "is not tolerable" under current
implementation tech ([ACM 10.1145/2837614.2837630](https://dl.acm.org/doi/10.1145/2837614.2837630)).
**Confidence: high** (3-0, primary, POPL). Overhead comes from **run-time checks
at typed/untyped boundaries**, producing **non-monotonic slowdowns up to 105x** —
"valleys" where adding types makes a program slower before it recovers.
**Confidence: high** (3-0).

**But the cliff is implementation, not fundamental.** Bauman et al., *"Sound
Gradual Typing: Only Mostly Dead"* (OOPSLA 2017), showed an optimized tracing JIT
(Pycket) **eliminated >90% of the overhead** ([ACM 10.1145/3133878](https://dl.acm.org/doi/10.1145/3133878)).
**Confidence: high** (3-0). *(A broad claim that sound gradual typing is
inherently slow was refuted 0-3 — overhead is implementation-dependent.)*

**Statically typing runtime-schema DataFrames.** The hardest problem is typing
tables whose schema is known only at runtime. Haskell's **Frames** uses Template
Haskell (`tableTypes`) to inspect a CSV **at compile time**, infer per-row types,
and give **compile-time type-safe column access** indexed by the data's own
column names ([Hackage: Frames](https://hackage.haskell.org/package/Frames)).
**Confidence: high** (3-0, primary). **Critical caveat the verifiers flagged:**
Frames infers at compile time from a *sample file* (~1000 rows) — it is *not*
true arbitrary-runtime schema inference. That boundary is exactly what Helix must
design around.

### The documented pain
1. **Global Hindley-Milner (OCaml/Haskell)** → poor, non-local error messages:
   unification failures surface far from their cause. Localized/bidirectional
   inference (Rust, TypeScript) anchors errors at the mismatch site.
2. **Gradual typing** → catastrophic non-monotonic regressions at boundaries —
   a direct threat to Helix's Phase 5 JIT if soundness uses boundary contracts.
3. **Compile-time DataFrame typing (Frames)** → brittle for genuinely dynamic
   data: schema must exist as a sample at compile time.

### Recommended approach for Helix
**A strong static core with localized/bidirectional inference (rare annotations,
local errors), plus a deliberate, well-marked boundary where runtime-schema
DataFrames cross from static into checked-dynamic territory — explicitly avoiding
sound-gradual boundary contracts in the hot path.**
- **Inference:** localized/bidirectional (Rust/TS tradition), prioritizing
  educational, locally-anchored errors over maximal inference completeness.
- **Known-schema data:** type fully statically; a Frames-style generator can give
  compile-time column safety where a sample file exists at build time.
- **Runtime-schema DataFrames:** schema is a runtime value carried by the
  DataFrame; column access is checked at the load boundary and first use,
  producing signature educational errors ("column `age` not found; available:
  ...") rather than HM noise. The value is statically a `DataFrame`; its
  column-level schema is dynamic-but-validated.
- **Keep the boundary coarse:** validate schema once at the boundary, never
  per-value — that is what produces the 105x valleys. A good JIT can recover
  overhead (Pycket), but the safer design never makes it necessary.

### Rejected alternatives
- *Whole-program Hindley-Milner* — poor non-local errors.
- *Fine-grained sound gradual typing with boundary contracts* — 105x
  non-monotonic slowdowns; incompatible with the JIT goal.
- *Pure compile-time DataFrame typing (Frames as the only model)* — can't type a
  Parquet file first seen at runtime. Adopt it for the static case only.
- *Fully dynamic typing (Python/R)* — forfeits static guarantees.

---

## Domain 3 — Collection API Unity / "One Obvious Way"

### What existing languages/libraries did
The directly-verified evidence is the **pandas indexing mess as negative
exemplar** and **Arrow compute kernels / masked columns as positive substrate**.
Pandas exposes multiple overlapping indexers (`loc`/`iloc`/`at`/chained indexing
with `SettingWithCopyWarning`); its missing-value/dtype inconsistencies
([Van den Bossche](https://jorisvandenbossche.github.io/blog/2019/11/30/pandas-consistent-missing-values/),
**high, 3-0**) are the same pattern: many ways to do one thing, each with subtly
different semantics. The dplyr/LINQ/Rust-iterator/Julia-dispatch comparisons
below are **design synthesis grounded in that verified pain — confidence: medium.**

### The documented pain
Pandas' multiple-indexing surface is the archetypal "many obvious ways" failure:
users can't predict view-vs-copy, whether assignment sticks, or which sentinel a
column uses → silent correctness bugs and a lore-heavy learning curve. Directly
antithetical to Helix's "one obvious way" + "consistency over cleverness."

### Recommended approach for Helix
**One verb protocol — `map`, `filter`/`where`, `reduce`, `group`, `sort` —
meaning the same thing across `Array`, `DataFrame`, `Tensor`, `Dna`, dispatched
through a trait/protocol, with strict naming discipline (one verb per concept).**
This builds directly on Phase 1's shipped `map`/`filter`/`where`/`reduce` with
`it`/`acc`, and the deliberate `where == filter` decision — **the research
explicitly affirms that instinct as the correct governing law.**
- **One verb per concept, everywhere.** Extend `where==filter` discipline so
  group/sort/reduce have exactly one spelling across all four types. No
  `loc`/`iloc`/`at` proliferation.
- **Abstraction:** a Rust-trait-style protocol — each collection implements the
  shared verbs, resolving to type-specific zero-copy impls (Arrow kernels for
  DataFrames, SIMD for Tensors) behind one identical surface. Julia-dispatch-like
  generality with static dispatch suited to the JIT roadmap.
- **Lazy/columnar by default** for DataFrame/Tensor: the same chain that is eager
  on a small Array becomes a fused lazy plan on a big DataFrame.
- **Naming discipline as written rule:** a verb joins the protocol only if no
  existing verb covers the concept; aliases forbidden. The structural defense
  against API sprawl.

### Rejected alternatives
- *Per-type bespoke APIs (pandas)* — overlapping inconsistent verbs, view/copy
  ambiguity.
- *Synonymous verbs for convenience* — already rejected by `where == filter`.
- *Untyped duck-typed protocol* — forfeits static guarantees + JIT
  specialization.

---

## Domain 4 — Functions, Errors & Mutability

### What existing languages/libraries did
Grounded primarily in the null/absence finding. The load-bearing verified result:
absence/error cases should be **tagged unions discriminated at compile time**, not
runtime sentinels ([Hoare](https://www.infoq.com/presentations/Null-References-The-Billion-Dollar-Mistake-Tony-Hoare/),
**high, 3-0**) — the same argument that justifies `Result`/`Either` over
exceptions. Syntax comparisons (ML/F#/Elm `let`, CoffeeScript) are **synthesis
from verified principles — confidence: medium.**

### The documented pain
- **Exceptions** create invisible control flow — a reader can't see which calls
  can fail, mirroring null's "every reference must be checked" problem.
- **CoffeeScript-style brace-free/whitespace syntax** → block-boundary ambiguity.
- **Rust's borrow checker** delivers safety but imposes ownership/lifetime
  learning cost inappropriate for scientists.

### Recommended approach for Helix
- **Function syntax:** brace-free `let`/`def`-style in the ML/F#/Elm tradition,
  expression-oriented (body is an expression), composing with existing `if … then
  … else` and dot-chains. Keep block boundaries explicit to avoid CoffeeScript
  ambiguity.
- **Errors as values:** a `Result`/`Either` tagged union with a lightweight
  Rust-`?`-style propagation operator — Hoare's prescription applied. Makes "what
  can fail" visible and forces handling while keeping the happy path uncluttered.
  Unifies with Domain 1: `missing` (absent data) and `Result`-error (operation
  failed) are **distinct** tagged concepts, never conflated into one null.
- **Value/mutability:** value semantics with copy-on-write + immutable-by-default
  (already shipped, with explicit `mut`). Predictable copy semantics eliminate
  pandas view/copy ambiguity by construction; memory safety + zero-copy come from
  COW and Arrow's immutable buffers **without ever exposing Rust's borrow checker,
  lifetimes, or ownership to the end user.**

### Rejected alternatives
- *Exceptions as primary errors* — invisible control flow.
- *Go-style `if err != nil`* — verbose, clutters the happy path.
- *Exposing the borrow checker* — wrong cognitive load; use COW + value semantics.
- *Mutable-by-default reference semantics (Python/pandas)* — reintroduces
  aliasing/view-copy bugs.
- *Pure significant-whitespace blocks (CoffeeScript)* — block-boundary ambiguity.

---

## Caveats
1. **Domains 3 & 4 rest more on synthesis than independently re-verified claims.**
   The verified corpus is strongest for Domain 1 (missing data) and Domain 2
   (type systems / gradual typing / Frames). Treat the collection-verb-trait,
   brace-free-syntax, and COW recommendations as well-reasoned but lower
   confidence.
2. **Frames does not solve runtime-schema typing** — its inference is
   compile-time from a sample. The runtime Parquet case remains an open problem.
3. **The gradual-typing "death" is conditional** — scoped to current impl tech;
   Pycket recovered >90%. Avoid fine-grained boundary contracts, but the
   literature does not forbid all static/dynamic mixing.
4. **Julia's `missing` is the most battle-tested model, but aggregation
   propagation is a policy choice** Julia leaves to the caller — Helix must pick
   a single default.

## Open questions (carried into the ADRs)
1. Exact aggregation policy for `missing` — propagate, skip, or explicit policy?
2. How to type a Parquet file first seen at runtime — dynamic + boundary
   validation, optional compile-time schema-pinning, or both?
3. Does the unified verb protocol use static trait dispatch, multiple dispatch,
   or a hybrid — and how does it interact with the JIT/GPU phases?
4. Precise relationship between `missing` (absent data) and `Result`-error
   (failed operation) at the API level.
5. How to keep the static/dynamic schema boundary coarse enough to avoid
   per-value overhead while still giving precise column-level errors.

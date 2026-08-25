# ADR 0036 — One semantics: frames, arrays and scalars answer the same question

- **Status:** **Accepted, implementation in progress** — the unification ADR 0034's
  policy-1 delta list was always a promissory note for. Ships in v0.6.0, and
  deliberately BEFORE ADR 0033's Stage-4 native flip, while correcting the native
  engine's NaN order is still free rather than a breaking change to a shipped default.
- **Date:** 2026-08-25
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0034](0034-native-frame-semantics.md) (the principle this
  finishes), [ADR 0025](0025-ordering.md) (ordering doctrine, extended here to
  frames), [ADR 0001] (missing propagation — one clause of whose 2026-07-17
  amendment this knowingly reverses), [ADR 0024](0024-total-runtime.md) (errors,
  never aborts), [ADR 0033](0033-native-dataframe-engine.md) (the flip this precedes).

## The principle

ADR 0034:14 already said it: **a column expression means exactly what the same
expression means on scalars.** It then recorded three deltas against the polars
backend and deferred closing them. This ADR closes them, finds five more that were
recorded nowhere, and states the rule that makes a sixth impossible:

> A divergence between any two of scalar-land, array-land and either frame backend is
> a bug in whichever side disagrees with **the language**. There is no privileged
> engine and no standing delta list.

## Decided policies

1. **Arithmetic follows scalars, on BOTH engines.** As of 0.6.0 this is not a delta
   list; it is one semantics with two implementations. `%` is euclidean (`7 % -3` is
   `1`); `/` is true division and yields Float (`2 / 2` is `1.0`, `41 / 10` is
   `4.1`); `//` is euclidean floor division and stays Int — the polars backend
   previously REFUSED it outright and now lowers it; division and modulo **by zero
   are errors** naming the row, 0-based, as a hint, for Int **and Float** alike. The
   polars backend previously gave three different silent answers to one question:
   `missing` for Int `/0`, `inf` for Float `/0`, `NaN` for `0.0/0.0`. String `+` is
   refused in a query, as it is on scalars; polars previously concatenated.

   The checker already agreed with the language and not with the backend —
   `src/types.rs:157` reads `Div => Type::Float, // division is always Float`. Polars
   was the only dissenter in the binary.

   **The thirteenth divergence, found while implementing this ADR and the reason
   `/` `%` `//` lower to a UDF rather than to polars' own operators:** polars is not
   IEEE-faithful for a scalar divisor. It rewrites division-by-a-constant into
   multiplication by the reciprocal, and `41.0 * 0.1` is not `41.0 / 10.0`. Measured
   on v0.5.1, `@b / 10` over `[41, 38, 55, 29]` answered
   `[4.1000000000000005, 3.8000000000000003, 5.5, 2.9000000000000004]` where the
   scalar kernel answers `[4.1, 3.8, 5.5, 2.9]`. It affects EVERY division by a
   constant in EVERY frame query, it is silent, it is one ULP wide, and — worst — it
   triggers only at **two rows or more**, so the one-row test anyone would write to
   check it reports agreement. No cast avoids it: Int/Int-literal, Int/Float-literal,
   Float/Int-literal and Float/Float-literal all diverge, while column-by-column
   division is exact. That is why the guard computes the arithmetic itself instead of
   merely checking for zeros: correctness here is not separable from the guard.

   The UDF is elementwise (`FunctionOptions::elementwise`), so streaming and
   predicate pushdown survive it, and polars invokes it **once per column** rather
   than once per morsel — measured at 4, 100k and 1M rows and pinned by
   `udf_invocation_shape`. That measurement is load-bearing: it is why the row number
   in `at row N of the frame.` is a global row and is deterministic. If a future
   polars starts chunking, that test fails rather than users silently reading a
   wrong row.

   Two consequences are load-bearing, because they change WHICH ROWS a query returns
   rather than how a number prints:
   - `where(@x / @y == 2)` on `x=[4,5], y=[2,2]` was 2 rows and is now 1 (`5 / 2` is `2.5`).
   - `where(@x / @y > 0)` over a zero divisor was a silently shorter frame (a null
     predicate drops the row) and is now an error naming the row.

2. **Float modulo by zero is an error in scalar-land too.** `1.0 % 0.0` returned `NaN`
   on scalars, on arrays and on both backends — no divergence, but policy 1's own
   sentence was false of Floats. It errors now, matching `1.0 / 0.0`. This is the last
   silent NaN-producing arithmetic channel, and closing it is what makes policy 5's
   compare-error affordable.

3. **NaN is a Float value meaning "this computation failed." `missing` is an absent
   datum. Nothing in the language converts one into the other, in either direction, at
   any depth.** `nan.is_missing()` is `false`; `drop_missing()` does not drop NaN and
   `drop_nan()` does not drop `missing`; `??` does not catch NaN. The laundering in
   `missing_or_nan` is WITHDRAWN.

   The survey is unanimous and not a close call: NumPy, pandas, R, Julia, Postgres,
   polars and IEEE-754-2019 all keep NaN as NaN. **Not one of them launders NaN into
   their missing marker.** And because Helix errors on `x / 0`, the only channels to a
   NaN inside the language are `inf - inf`, `sqrt(-1)`, `ln(-1)`, `0 * inf` and a CSV
   field spelled `NaN` — so inside Helix specifically, a NaN is overwhelmingly a bug,
   not a data-encoding convention. Laundering took the one signal that a computation
   went wrong and filed it under "the data was incomplete", which is the category ADR
   0001 trains users to accept and move past. ADR 0001's own Consequences section
   forbids exactly that merge: `missing` "must stay distinct from `Result`-error".

4. **Every numeric reduction propagates NaN as NaN.** Measured before implementing,
   because the laundering turned out NOT to be uniform — it is four behaviours across
   thirteen reductions, and two of them are worse than laundering:

   | today | reductions |
   |---|---|
   | `missing` (laundered) | `max min sum mean std var median argmin argmax` |
   | `NaN` (already correct) | `product norm` |
   | **`2.0` — a wrong number** | `spread` |
   | skips the NaN | `group().max()`, `group().min()` |
   | propagates | `group().sum()`, `group().mean()` |

   `[1.0, nan, 3.0].spread()` answering `2.0` is the worst of them: not missing, not
   NaN, but a plausible and confidently wrong number in the stats surface of a language
   aimed at scientific work. And `sum` disagrees with `group().sum()` in one binary —
   the ungrouped form launders, the grouped form propagates.
 `max min sum mean product std
   var median quantile summary spread norm normalize`, the `argmin`/`argmax` family,
   and every grouped aggregate on both backends. Never `missing`, never a skip.
   `spread` stops using Rust's `f64::min`/`f64::max` (IEEE-754-2008 `minNum`, REMOVED
   in 754-2019 for being non-associative) — it answered `2.0` for `[3.0, NaN, 1.0]`, a
   wrong number in the flagship's own stats surface. The polars backend's
   `group().max()` stops skipping NaN: that is precisely the pandas `skipna` behavior
   ADR 0025:132 wrote down as a red line and then shipped in the frame world.
   `missing` propagation under ADR 0001 is unchanged and independent; an array holding
   both yields `missing`, because absence is the weaker claim.

   **`.drop_nan()` is the single visible opt-out**, a verb parallel to
   `.drop_missing()` — not a `skipna=` flag, per ADR 0001's visible-verb choice.
   `xs.drop_nan().max()` is the `nanmax` spelling.

   An earlier draft of this policy claimed *no spelling at all* extracted a real
   maximum from a NaN-bearing array. That was **wrong**, and measuring beats
   asserting: `xs.filter(x => not is_nan(x)).max()` returns `3.0` today.
   `drop_missing().max()` is indeed a dead end (still `missing`), which is what the
   claim was generalised from. So `.drop_nan()` is a SHORTHAND for something that
   already works, not a new capability, and the migration is gentler than the
   overstatement implied — which matters, because a policy resting on "there is no
   alternative" is weaker than one resting on "the alternative is verbose".

5. **Ordering comparisons on a NaN are an error; `==`/`!=` stay IEEE.** `< > <= >=`
   raise `cannot compare these values (NaN?)` on scalars, arrays and BOTH frame
   backends — IEEE-754 defines signaling comparison predicates that raise on unordered
   operands, so this is a legitimate 754 option, not an invention, and it keeps ADR
   0024's total-runtime posture. The polars backend previously answered `true` for
   `NaN > 2.0`, silently KEEPING the row: a wrong dataset, not a wrong format, on the
   default backend, with exit 0. `nan == nan` is `false` at any depth; polars
   previously reported NaN as self-equal in expressions and must not.

   **Load-bearing precondition, discharged in the same release:** `is_nan()` and
   `is_finite()` now work INSIDE a DataFrame query. Until 0.6.0 they were a parse-time
   refusal, which made the runtime's own hint — "guard it first with `is_nan(x)`" —
   advice the user could not follow on a column.

6. **One order: NaN sorts LAST, sign-independently, everywhere.** `sort`, `argsort`,
   `sort_by`, and frame `sort` on both backends place every NaN after `+inf`
   ascending. `-0.0 < 0.0` is retained for everything else (ADR 0025) and is EXTENDED
   to the polars frame sort, which previously canonicalized signed zeros to equal.

   The rule this replaces was `f64::total_cmp` — order by sign bit — and it was
   unobservable, undocumented, and self-contradictory: `sqrt(-1.0)` produces a
   NEGATIVE NaN on x86 (the invalid-op default is the real-indefinite QNaN, sign set),
   so `[3.0, sqrt(-1.0), abs(sqrt(-1.0)), 1.0].sort()` printed `[NaN, 1.0, 3.0, NaN]`
   — **the same printed value at both ends of one sorted array, decided by a bit no
   user can see.** NumPy, pandas, polars, Julia and Postgres all place NaN last. The
   polars frame backend was already right on this half and native was wrong;
   correcting native is free today and a breaking change to a shipped default after
   Stage 4, which is why this ADR precedes it.

7. **Keys use one total identity in which all NaNs are one, distinct from `missing`.**
   `unique`, `frequencies`, `contains`, `index_of`, alignment, `group`, `join` and
   dict keys. `[nan, nan].unique()` is `[NaN]`; a NaN join key matches a NaN join key.

   **This reverses one clause of ADR 0001's 2026-07-17 amendment** ("Floats keep IEEE
   semantics in both … so `unique` does not collapse NaNs"). Three reasons, stated
   plainly because a written decision deserves a written reversal. (i) The clause is
   **already not implemented** — arrays obey it, frames do not: frame `unique`
   collapses, frame `group` makes one group, frame `join` matches NaN to NaN, on both
   backends. The amendment describes a language Helix does not have. (ii) A key domain
   must be an equivalence relation or grouping is undefined and a hash join is
   unimplementable — which is why Postgres, polars, pandas, Julia and numpy ≥ 1.21 all
   made NaN self-equal for KEYS while keeping `==` IEEE. (iii) The amendment's stated
   reason — "one float equality everywhere beats a second float-identity rule" — was
   never available: Helix already had two float relations, `ops::compare`'s IEEE `<`
   and `numeric_cmp`'s `total_cmp`, documented as such by ADR 0025. The choice is not
   one relation versus two. It is two relations named and specified, or two relations
   undocumented and drifting.

8. **A `nan` literal is added beside the existing `inf`**, producing a canonical quiet
   NaN. A doctrine whose first sentence is "NaN is an ordinary Float value" cannot
   coherently refuse to let you write one, and the error message already apologized for
   its absence and handed out a workaround. The cost is not zero: `nan` becomes a
   builtin constant, so any program binding a variable of that name now gets ADR 0027's
   shadowing error. **Three** files in THIS repository did —
   `tests/corpus/m5_nan_sort.helix`, `tests/corpus/t9_eq3_tuples.helix`, and a Helix
   program embedded in `tests/cli.rs` — and were rewritten. The third was found by the
   gate rather than by the survey, because a grep over `.helix` files cannot see Helix
   source that lives inside a Rust string. Worth knowing before adding the next
   constant: the blast radius of a new global name is larger than the file extension
   suggests. (It was also the better fixture afterwards: it had bound
   `nan = INF - INF`, which yields a NEGATIVE NaN on x86, so it had been testing one
   sign only.)

## Deltas — asserted AS deltas

Two remain, both narrow, both recorded so a test can prove the divergence is exactly
the decided one:

- **`**` on Int columns that overflows i64 is an ERROR naming the row, on both
  backends, where the SCALAR promotes to Float.** `2 ** 63` is `9223372036854775808.0`
  on scalars and an error in a frame. The reason is structural: a column has one
  dtype, so the scalar's promote-when-it-does-not-fit rule would make a column's TYPE
  data-dependent per row, which is unrepresentable. The frame refuses what a column
  cannot hold. This replaced a three-way split in which polars silently wrapped to
  `-9223372036854775808`, native promoted, and neither appeared in any ADR — the worst
  kind of divergence, because both answers were plausible. **The delta closes when the
  overflow ADR makes the scalar error too.**

- **Div-by-zero's CARET lands at the materialization point on the polars backend**, not
  at the verb. Native is eager and errors at `.with(...)`; the lazy backend's
  elementwise guard fires when the plan runs — at `print(...)`, `.column(...)`,
  `write_parquet`, `write_csv`. The message and the row number are identical; only the
  source position moves. Fixing it requires executing the plan twice, which measured
  3.5x on any query with a limit or a downstream predicate, and unbounded on a large
  source, because eager materializes the whole input regardless of how few rows the
  query wanted. Paying that on every query to move a caret is the wrong trade.

## Integer overflow is NOT decided here

`+ - *` and `group().sum()` wrap identically on both backends. That is a language-wide
doctrine question, not a seam divergence, and it is deferred to its own ADR rather than
smuggled into this one. The single exception is frame `**` above, which was a live
three-way split and therefore in scope.

Explicitly rejected: making `.product()`, `.cumsum()` and `group().sum()` promote to
Float as an interim step. It would change each of those expressions **twice in two
releases** — wrap, then promote, then error. One breaking change per expression beats
two.

## What the differential campaign compares

Every policy above is exercised by dual-backend tests in one dev binary
(`backend::native::tests::against_the_oracle`), by the corpus, by
`tests/ordering_matrix.rs`, and by its new frame sibling — ADR 0025 scoped itself to
arrays and the matrix followed: 247 cells, **zero of them a DataFrame**. That scope was
wrong and the cost was measurable.

`scripts/dfdiff.sh` runs every tracked `.helix` program under both engines. It exists
because the procedural lesson of this release is not technical: **a differential
campaign only covers inputs somebody wrote down.** At the v0.5.1 tag,
`examples/dataframes/dataframes.helix:27` was the ONLY `with({…})` in the tracked tree,
and `/` and `%` appeared inside a frame query nowhere else — so the entire arithmetic
surface of the frame language rested on one line of one example, and five divergences
hid behind it. Its predecessor `scripts/dfcheck.sh` was worse than absent: it ran a
path that had moved, so it diffed three copies of "no such file" and reported them
identical, while ADR 0033:60 cited it as acceptance evidence.

## Consequences

- One release changes which rows some queries return. That is the cost of having
  shipped two dialects, and it is paid once, loudly, with `tests/compat/MIGRATIONS.md`
  naming every pinned artifact that moves.
- `tests/compat/v0.5.1/` records what the two-dialect world computed. Once a program is
  authorized in MIGRATIONS, a *second* accidental drift in that same program is no
  longer detectable — so capturing `tests/compat/v0.6.0/` at release is not optional.
- The differential fuzzer's literal pool still does not generate near-`i64::MAX`
  values, which is why the overflow family survived 40,000 fuzzed programs. That is
  open, and it means the `**` pins are the only coverage for that delta.

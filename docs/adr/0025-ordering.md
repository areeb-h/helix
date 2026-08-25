# ADR 0025 — One order, one domain: unifying `sort`, `argsort`, `min`/`max` and the `_by` family

- **Status:** **Accepted and IMPLEMENTED 2026-08-13** — a1 (`8d51f6c`), d1 (`9262684`),
  b1 (`4b0475d`), c1 (`e073d23`). The signed-zero residue (d2) awaits comparator
  unification and is documented in `examples/language/ordering.helix`. Ships in v0.2.0.
  The evidence artifact exists and is green: `tests/ordering_matrix.rs` pins 247 cells
  (19 shapes × 13 spellings) on all three engines and passes against `e267a25`. It is a
  regression net for whichever direction is chosen, not an endorsement of today.
- **Date:** 2026-08-12
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0001 — Missing data](0001-missing-data.md) (the three-valued
  contract the four spellings apply four different ways), [ADR 0017 — Methods and
  functions](0017-methods-and-functions.md) (why `xs.argmax()` and `argmax(xs)` both
  exist at all), [ADR 0024 — A total runtime](0024-total-runtime-no-host-panics.md)
  (the `total_cmp` decision that gave `sort` its comparator, and the `argmax`/`argmin`
  `missing`-propagation that only reached the *free function*).

## Context

Helix has one concept — "put these in order" — and **four implementations of it**. They
disagree on the element types they accept, on `missing`, on `NaN`, on the empty array,
and on which element wins a signed-zero tie. Every disagreement is between two
*spellings*; none is between two engines.

| domain | spellings | comparator | `missing` element | `NaN` element | empty | code |
|---|---|---|---|---|---|---|
| **A. sort** | `sort` | `numeric_cmp` (`total_cmp`) + `Str` + `Dna` | **error** | *placed* | `[]` | `src/interp/methods.rs:1375` |
| **B. argsort** | `argsort`, `sort_by` | `numeric_cmp` + `Str`, **no `Dna`** | **`missing`** | *placed* | `[]` | `src/interp/methods.rs:1838` |
| **C. reduction** | `min`, `max`, free `argmin`/`argmax` | `numeric_cmp`, **numbers only** | `missing` | `missing` | **error** | `:1316`, `builtins.rs:919` |
| **D. `<`-reduce** | `min_by`, `max_by`, method `argmin`/`argmax` | `ops::compare` — **IEEE**, three-valued, accepts `Tuple`/`Str`/`Dna` | **error** | **error** | **error** | `parser.rs:208` |

Read across a single row of the matrix and the four domains are unmistakable:

```
["b", "a"].sort()        ->  ["a", "b"]        (A)
["b", "a"].argsort()     ->  [1, 0]            (B)
["b", "a"].min()         ->  error: `min` needs an array of numbers, but element 0 is a String   (C)
["b", "a"].min_by(it)    ->  a                 (D)
argmin(["b", "a"])       ->  error: `argmin` expected an array of numbers, found a value of type String
```

`min_by(it)` is `min` with the identity key. One of those two lines is wrong.

The four defects, each verified on the tree-walker, the VM and the JIT (2026-08-12,
`e267a25`, `target/gate/helix`):

1. **`sort` and `argsort` disagree on `missing` and on `Dna`.**

   ```
   [1, missing, 3].sort()             ->  error: cannot sort: the array has missing values
   [1, missing, 3].argsort()          ->  missing
   [dna("GG"), dna("AA")].sort()      ->  [AA, GG]
   [dna("GG"), dna("AA")].argsort()   ->  error: `argsort` needs an array of all numbers or all strings
   ```

   `sort_by` inherits **argsort's** answers, because `desugar_sort_by`
   (`src/parser.rs:76`) rewrites `xs.sort_by(k)` to
   `let $s = xs in $s.map(k).argsort().map($si => $s[$si])`. So `xs.sort()` and
   `xs.sort_by(it)` — two spellings of *the same operation* — disagree on both, and the
   `sort_by` failure names `argsort`, a method the user never wrote.

2. **`min`/`max` are narrower than `sort`, and narrower than `min_by`.** `sort` accepts
   `Str` and `Dna`; `min`/`max` accept neither. `min_by(it)` accepts both, plus `Tuple`.

3. **`<` orders tuples; no sort or reduction spelling does.**

   ```
   (1, 2) < (2, 1)                    ->  true
   [(2, 1), (1, 2)].min_by(it)        ->  (1, 2)
   [(2, 1), (1, 2)].min()             ->  error: `min` needs an array of numbers, but element 0 is a Tuple
   [(2, 1), (1, 2)].sort()            ->  error: `sort` needs an array of all numbers, all strings, or all DNA
   ```

   This is not a corner: `enumerate()` and `zip()` *return* tuples, so the two verbs the
   language hands you for "index alongside value" produce arrays `sort` refuses. It also
   blocks the obvious composite-key spelling — `sort_by(c => (c.w, c.uses))` — which is
   why real code in this repo still hand-encodes composite keys as arithmetic
   (`0 - (c.w * 100000 + c.uses)`).

4. **Signed-zero ties split the families, invisibly to `==`.** `sort`/`argsort`/`min`/
   `max` use `total_cmp`, under which `-0.0 < 0.0`. Family D desugars through IEEE `<`,
   under which the two zeros are *equal* (`0.0 < -0.0` is `false` **and** `-0.0 < 0.0`
   is `false`), so first-wins returns element 0 whatever it is:

   ```
   [0.0, -0.0].min()          ->  -0.0        [0.0, -0.0].min_by(it)   ->  0.0
   [-0.0, 0.0].max()          ->   0.0        [-0.0, 0.0].max_by(it)   -> -0.0
   [0.0, -0.0].argsort()      ->  [1, 0]      [0.0, -0.0].argmin()     ->  0
   [-0.0, 0.0].argsort()      ->  [0, 1]      [-0.0, 0.0].argmin()     ->  0
   [0.0, -0.0].min_by(it)     ->  0.0         [0.0, -0.0].max_by(it)   ->  0.0
   ```

   The last line is the sharpest: on that array the *smallest* element and the *largest*
   element are the same element. And `[0.0, -0.0].min_by(it) == [0.0, -0.0].min()` is
   `true`, because `0.0 == -0.0` — so a test written with `==` is **structurally blind**
   to this whole defect. `tests/ordering_matrix.rs` asserts on the rendered text
   throughout, and `equality_is_blind_to_the_signed_zero_disagreement` pins both the
   blind assertion and the sighted one so nobody re-derives the blind version.

**These are spelling inconsistencies, not engine divergences — stated explicitly
because it changes what the fix costs.** All 247 cells render identically on the
tree-walker, the VM and the JIT. That is structural, not luck: `sort`, `argsort`,
`min`, `max` have exactly one implementation each (`src/interp/methods.rs`; neither
`src/vm.rs` nor the JIT carries a copy), and `sort_by`/`min_by`/`max_by`/`argmin`/
`argmax` are *parse-time desugarings*, so all three engines walk the same AST. Whichever
option is chosen, the change lands in one or two functions and the differential oracle
follows for free.

Two more things the matrix surfaced that were not in the original bug report:

- **The method and the free function of the same name disagree.** `argmax(xs)` and
  `xs.argmax()` are different code (`builtins.rs:919` vs the `desugar_order_by` reduce),
  and they differ on `missing` (propagate vs raise), `NaN` (propagate vs raise), `Str`/
  `Dna`/`Tuple` (refuse vs accept) and empty ("`argmax` of an empty collection" vs
  "index 0 is out of bounds for length 0"). ADR 0024's fix — "`argmax`/`argmin`
  propagate `missing`" — landed on the free function only.
- **ADR 0024's prose about NaN placement does not describe what ships.** It says NaN
  sorts "after `+inf`, numpy-style". `total_cmp` orders by the **sign bit**, and every
  NaN this runtime produces in practice (`sqrt(-1.0)`, `inf - inf`) has that bit *set*,
  so it sorts **first**. The comparator's own doc comment (`methods.rs:628`) was
  corrected to say so; the ADR text was not. numpy places every NaN last regardless of
  sign, so Helix does *not* match numpy here.

## Prior approaches and their documented shortcomings

| System | How it answers "one order or many?" | Documented pain |
|---|---|---|
| **Python** | One `<` for everything; `sorted`, `min`, `max`, `sorted(key=)` all delegate to it. Tuples order lexicographically. | The single most-copied thing about Python's collections API, and the reason `min(("b","a"))` needs no special case. Its cost is the other end: `sorted([3.0, nan, 1.0])` silently mis-sorts, because `<` is a partial order and `sorted` pretends it isn't. |
| **Rust std** | Splits **domain** from **policy**: `Ord`/`PartialOrd` decide *what is comparable*, `sort_by`/`total_cmp`/`min_by_key` decide *how*. `f64` is `PartialOrd` only; `total_cmp` is the opt-in total order. | The split is the lesson: one comparability domain, several deliberate policies on top. `sort_by` *panics* on a non-total comparator rather than mis-sorting — Helix hit exactly that in ADR 0024. |
| **NumPy** | Two families on purpose: `np.sort`/`np.argsort` place NaN last (total order), `np.min`/`np.argmin` propagate NaN, `np.nanmin` skips it. | Precedent that "sorts place, reductions propagate" is a legitimate *policy* split. But numpy's two families share one **domain** (the dtype), so `np.sort` and `np.min` never disagree about *what is sortable* — which is precisely Helix's bug. |
| **Julia** | `sort`/`minimum`/`argmin` all take `lt=`/`by=`, defaulting to `isless`, which is a genuine total order (`-0.0 < 0.0`, NaN last). | Julia already made Helix's question (d) call: `isless` orders the zeros, so `minimum` and `argmin` agree. `min`/`max` keep IEEE, and Julia documents the split rather than hiding it. |
| **pandas** | `sort_values` places NaN (`na_position=`), `min`/`idxmin` skip it (`skipna=True`). Object columns fall back to Python `<`. | The `skipna` default is a famous silent-wrong-answer source — Helix's ADR 0001 `missing` propagation is the deliberate opposite, and this ADR must not accidentally re-introduce skipping. |
| **R** | `sort` drops `NA` by default, `order` keeps it (`na.last`), `min` propagates it. Three verbs, three `NA` policies. | The failure mode Helix has today, in a shipped language, for thirty years. R users memorize it. That is the cost of not deciding. |

The synthesis these point at: **one comparability domain, several explicitly documented
policies layered on it.** Helix currently has the inverse — four domains and four
policies, with no stated relationship between any of them.

---

## The four questions

Each option lists the currently-passing assertions and `.expected` goldens it would
change. Counts are from `git grep` against `e267a25` (not the working tree) and name
every site; comments and doc prose are listed separately because they are edits, not
failures. `tests/ordering_matrix.rs` is excluded from every count — by construction it
changes for all of them, which is its job.

### (a) Do `argsort` and `sort` unify on `sort`'s policy or `argsort`'s?

**Option a1 — `argsort` moves to `sort`: error on `missing`, accept `Dna`.**

- Changes **2 assertions in 1 test function**, both in
  `src/vm/tests.rs::argsort_reads_the_packed_buffer_and_is_lazy_on_ranges`:
  - `:6838` `("[1, missing, 2].argsort()", "missing")` → becomes the `sort` error.
  - `:6856` `("[dna(\"T\"), dna(\"A\")].argsort()", "all numbers or all strings")` →
    becomes `[1, 0]`.
- **0** `.expected` goldens, **0** examples.
- `sort_by` follows for free (it *is* argsort), so `xs.sort()` and `xs.sort_by(it)`
  agree afterwards — including the `Dna` case, which starts working.
- Cost: a caller who relies on `xs.argsort()` propagating `missing` into a three-valued
  pipeline gets an error instead. There is no such caller in the repo.

**Option a2 — `sort` moves to `argsort`: propagate `missing`, drop `Dna`.**

- Changes **3 assertions in 3 test functions**:
  - `src/vm/tests.rs:7749` (`sort_and_reverse_keep_a_packed_array_packed`) — the
    `missing` error.
  - `src/interp/tests.rs:1243` (`dna_is_orderable`) — `[dna("CAT"), …].sort().first()`.
  - `src/vm/tests.rs:4142` (`parity_value_methods_and_destructuring`) — the same DNA
    sort in the parity list.
- Changes **1 golden**: `tests/corpus/t11_diag.expected:5` (pinned by
  `tests/cli.rs::corpus_is_engine_identical_and_pinned`).
- Requires **1 example edit**: `examples/language/missing-data.helix:74-76`, whose whole
  section is titled "Sorting refuses rather than inventing an order" and whose trailing
  comment quotes the error verbatim. It is not machine-pinned (only `##` doctest blocks
  are, and there are none touching ordering), so it would *pass while being false* —
  the worst outcome.
- Cost: it deletes a documented ADR 0001 behaviour ("make dropping visible"), removes
  DNA sorting, which is a bio-first flagship's exact use case (canonical k-mers,
  sort-by-sequence), and rewrites a teaching example to teach the opposite.

**Recommendation: a1.** `sort` is the one with the richer, more deliberate policy — the
`missing` error carries a hint (`drop them explicitly first: xs.drop_missing().sort()`),
which is ADR 0001's "make dropping visible" doing its job, and DNA ordering is a
flagship feature (`ops::compare` already orders `Dna`, so `argsort` refusing it is the
outlier, not `sort` accepting it). a1 also costs a third as much and touches no golden.
The honest counter-argument for a2: `argsort` returns *indices*, so `missing` in, nothing
out is arguably the more composable answer for a three-valued pipeline — if the owner
weighs pipeline composability above the explicit-drop principle, a2 is defensible, and
the ADR 0001 hint could move to `drop_missing` documentation.

### (b) Do `min`/`max` widen to everything `sort` accepts (`Str`, `Dna`, `Tuple`)?

**Option b1 — widen.** `["b", "a"].min()` becomes `a`; `[dna("GG"), dna("AA")].min()`
becomes `AA`; `[(2,1),(1,2)].min()` becomes `(1, 2)` (if (b) is taken together with the
tuple half below).

- Changes **0 currently-passing assertions and 0 goldens.** `git grep -E '(min|max).
  needs an array of numbers'` over `src/`, `tests/`, `examples/`, `bench/` at `e267a25`
  returns **nothing** — the error text is asserted nowhere. Only `docs/ROADMAP.md:2211`
  quotes it, as prose in the backlog entry this ADR closes.
- **Name the real cost, because the test count hides it: this is a semantic change to a
  SHIPPED release.** v0.1.1 is published and installable on six platforms. A program
  written against it that does `r = try xs.min()` / `if r.ok` to *detect* a
  non-numeric column would, after b1, silently succeed and compare strings. There is no
  deprecation channel — no warning, no version gate — so the change is invisible until a
  user's result is wrong. It is an error → value transition, which is the direction that
  cannot be caught by a compiler.
- Implementation constraint that must not be missed: `min`/`max` get their domain from
  `numeric_vec` (`methods.rs:590`), which `min`/`max` share with **fifteen other
  reduction arms in the same file** — `mean`, `std`, `median`, `var`, `quantile`,
  `summary`, `sum`, `normalize`, `standard_error`/`coefficient_of_variation`/`iqr`/
  `spread`/`zscores`, `dot`, `norm`, `cumsum`, `product`, `clamp`, `softmax`. Widening
  `numeric_vec` would make `["a"].sum()` legal. `min`/`max` must be split off it onto an
  order-domain check instead.

**Option b2 — keep `min`/`max` numeric, and narrow `min_by`/`max_by` to match.**
`["b", "a"].min_by(it)` becomes an error.

- Changes **1 assertion**: `src/vm/tests.rs:6767` `("[\"b\", \"a\"].min_by(it)", "a")`,
  plus every `Tuple`/`Dna` case in the same function
  (`:6759`, and `dna/min_by` has no test) — call it **1–2 assertions, 1 test function**.
- This is a value → error transition in the same shipped release, which is *louder*
  (users see a failure, not a wrong answer) but strictly removes capability: after b2,
  `records.min_by(r => r.name)` — "the row with the alphabetically first name" — has no
  spelling at all, even though `records.sort_by(r => r.name)` keeps working (it rides
  `argsort`, not `min_by`, and `argsort` already orders strings; pinned at
  `src/interp/tests.rs:603`). One verb would order strings and its `min` twin would not.
  It also contradicts (a1): `sort` would order strings while `min` refused them.

**Recommendation: b1, but staged.** Widen `min`/`max` to exactly `sort`'s domain, and do
it in the same commit as (a) so there is one order domain rather than three. The
shipped-release cost is real and should be paid deliberately: land it as **v0.2.0, not a
patch**, with a CHANGELOG entry naming `["b","a"].min()` explicitly, because "an error
became a value" is the kind of change a semver minor exists to announce. If the owner
judges that cost too high for the benefit, b2 is coherent — but then (a1)'s "one order
domain" claim weakens to "one *sort* domain and one *reduction* domain", and that split
must be written into the language reference rather than left to be discovered.

### (c) Do `min_by`/`max_by`/`argmin`/`argmax` move onto the reduction's policy?

Today family D raises where family C propagates, on all three of `missing`, `NaN` and
the empty array — and the errors it raises are *leaked internals*:

```
[1, missing, 3].min()      ->  missing
[1, missing, 3].min_by(it) ->  error: `if` condition is `missing` — cannot choose a branch
[].min()                   ->  error: cannot compute `min` of an empty array
[].min_by(it)              ->  error: index 0 is out of bounds for length 0
missing.min()              ->  missing
missing.min_by(it)         ->  error: a value of type Missing cannot be indexed
```

The `if` and the index-0 are the desugared reduce talking (`desugar_order_by`'s
`$ob.reduce($ob[0], ($a, $b) => if $b[k] < $a[k] then $b else $a)`). No user wrote an
`if`, and no user wrote `[0]`.

**Option c1 — move family D onto family C's policy** (`missing`/`NaN` → `missing`;
empty → a named domain error).

- Changes **4 assertions in 1 test function**,
  `src/vm/tests.rs::min_by_and_max_by_return_the_original_element_for_a_destructuring_key`:
  `:6781` `[].min_by(it)`, `:6782` `missing.min_by(...)`, `:6783` `[1, missing, 3]
  .min_by(it)`, `:6784` `[1.0, inf - inf].min_by(it)`.
- Changes **0** goldens, **0** examples.
- **Two structural costs, both real, both verified in the code:**
  1. **It collides with the `argmin` fast path's decline sentinel.** `desugar_order_by`
     emits `xs.$arg_extreme(want_max) ?? <the tuple reduce>`, and `$arg_extreme` returns
     **`Value::Missing` to mean "I declined, run the slow path"** (`parser.rs:344-353`,
     which says so and warns that a change making the method propagate `missing` "must
     confront this collision first"). If `missing` becomes a legal *answer*, the `??`
     cannot distinguish it from a decline: it would fall through to the reduce, which
     raises — the exact error c1 is trying to remove. c1 therefore requires a new decline
     channel (a distinct internal sentinel, or `$arg_extreme` returning a two-valued
     result), not a policy tweak.
  2. **It breaks the identity `a5737ce` deliberately pinned.** That commit re-desugared
     `min_by(k)` to `let $obe = recv in $obe[$obe.map(k).argmin()]`, and its justification
     (`parser.rs:271-281`) is that *min_by's errors have always been argmin's errors
     wearing a different name* — same reduce seed on empty, same "cannot be indexed" on
     `missing`, same NaN text — "verified byte-for-byte on all three engines". Under c1
     that stops holding two ways: the empty-array messages must differ (`min_by` cannot
     honestly say "`argmin` of an empty collection"), and `argmin` returning `missing`
     makes `$obe[missing]` raise "`index` expected an integer, found a value of type
     Missing" — verified — rather than propagating. So c1 forces `min_by` to stop
     composing through `argmin`, undoing the simplification `a5737ce` bought.
- Note the split c1 also closes: the **free** `argmin`/`argmax` already follow family C
  (`argmin([1, missing, 3])` → `missing`, `argmin([])` → "`argmin` of an empty
  collection"), pinned at `src/interp/tests.rs:1458-1460` and `:1685`. Those four
  assertions would *keep* passing under c1 and would finally describe the method too.

**Option c2 — leave the policy, fix only the leaked error text.** `min_by`/`argmin` keep
raising on `missing`/`NaN`/empty, but say so in their own words with their own caret.

- Changes the same **4 assertions** (they assert the leaked text), **0** goldens.
- Keeps the `$arg_extreme` sentinel and the `a5737ce` composition intact — but the
  composition is exactly *why* the text leaks, so c2 still costs a real refactor: either
  `min_by` stops composing through `argmin`, or `argmin`'s own messages are rewritten to
  be honest for both spellings.
- Leaves family C and family D disagreeing about `missing` forever, and leaves
  `xs.argmax()` and `argmax(xs)` giving different answers to the same question.

**Recommendation: c1, but decoupled from (a) and (b) and landed last.** It is the only
option that makes ADR 0024's own sentence — "the three-valued contract covers *every*
aggregation" — true of `xs.argmax()` and not just `argmax(xs)`; today ADR 0024 is
accurate about the free function and false about the method, which is a documentation
defect as much as a code one. But c1 is the one option with genuine structural cost
(a new decline channel plus unwinding `a5737ce`'s composition), so it should be its own
commit with its own measurement, not folded into a domain change. If the owner wants the
smaller step first, c2 is a strict improvement on its own — leaked `if`/`index` errors
are indefensible either way — and c1 remains available on top.

### (d) Is IEEE first-wins on signed-zero ties a bug or a documented consequence?

```
[0.0, -0.0].argmin()   ->  0        [-0.0, 0.0].argmin()   ->  0
[0.0, -0.0].argmax()   ->  0        [-0.0, 0.0].argmax()   ->  0
[0.0, -0.0].argsort()  ->  [1, 0]   [-0.0, 0.0].argsort()  ->  [0, 1]
```

So `xs[xs.argmin()]` and `xs.min()` are different elements of `xs` (`0.0` vs `-0.0`),
and `argmin` disagrees with `argsort` about which index is smallest.

**Option d1 — keep it, and document it.** `packed_arg_extreme`'s comment
(`methods.rs:668-685`) already argues the case: family D mirrors `ops::compare`'s `<`,
which is IEEE, so `argmin` agrees with the operator a user would write by hand, and
first-wins makes the answer permutation-*stable* in the sense that ties never move.

- Changes **0 assertions** — nothing pins `[0.0, -0.0].argmin()` at `e267a25` except
  that source comment. It becomes a language-reference sentence.
- Cost: `xs[xs.argmin()] != xs.min()` stays true, and stays discoverable only by
  reading rendered output — `==` cannot see it.

**Option d2 — fix it: family D orders the zeros too.**

- Changes **0 currently-passing assertions and 0 goldens** directly. It does *not*
  disturb the 13 signed-zero `min`/`max` assertions
  (`src/vm/tests.rs:7083-7092`, `min_and_max_do_not_depend_on_the_arrays_representation`),
  the 2 `argsort` ones (`:6825-6826`) or the 2 `sort` ones (`:7656-7657`) — all four
  already use `total_cmp` and would simply gain agreement from family D.
- **But it cannot be done inside `argmin` alone, and that is the whole decision.**
  Family D's comparator *is* the `<` operator: `desugar_order_by` emits `Expr::Binary {
  op: Lt }`, evaluated by `ops::compare`. Making the reduce order the zeros means one of:
  - changing `ops::compare` for floats — which changes the **language operator**:
    `0.0 < -0.0` would become `true`, breaking IEEE for every user expression, not just
    `argmin`. Rejected on sight.
  - stopping `argmin`/`min_by` from desugaring through `<` and giving them a
    `numeric_cmp`-based implementation — a real rewrite of `desugar_order_by`, and it
    changes the error matrix for every non-numeric shape (`Tuple`, `Str`, `Dna`), i.e.
    it collides head-on with (b) and (c).
  - or: keeping the desugar and changing only the packed kernel — which would make the
    answer depend on the array's **representation**, the exact defect fixed on 2026-08-09
    and pinned by `min_and_max_do_not_depend_on_the_arrays_representation`. Not an option.
- So d2 is not a standalone fix; it is a consequence of deciding (b)+(c) in favour of one
  comparator.

**Recommendation: d1 for now — a documented consequence — with d2 as the natural
byproduct of (b)+(c).** Julia is the precedent that ordering the zeros is the right end
state (`isless` orders them, so `minimum` and `argmin` agree), and Helix's own
`sort`/`min`/`max` already went that way. But the *only* clean route there is via one
shared order relation, and the interim states are worse than either endpoint. So:
document first-wins now (it is already true and now pinned by
`tests/ordering_matrix.rs`), and let it fall out when the comparator unifies. The owner
can disagree in the direction of urgency — `xs[xs.argmin()] != xs.min()` is the kind of
thing a scientific user hits once and never trusts again — in which case (c1) should be
scheduled first, since d2 rides on it.

---

## The tuple claim, verified

An earlier survey reported that tuple ordering is fixed by "a single-site validation
swap in `src/interp/methods.rs`". **The phrase appears nowhere in the repo**
(`grep -rn 'single-site\|validation swap' docs/` returns nothing), so this section
checks the substance against the code rather than the wording.

**Partly true, and misleading in the part that matters.**

- **True for `sort` alone.** Tuples live in `ArrayData::Values` (`src/value.rs:164-166`;
  the packed variants are `Ints`/`Floats`/`Range`/lazy-`enumerate` only), so a tuple
  array cannot reach the packed `"sort" | "reverse"` arm at `methods.rs:889`. The single
  gate is the `else if` chain in `array_method`'s `"sort"` arm at **`methods.rs:1375`**,
  which ends in "`sort` needs an array of all numbers, all strings, or all DNA". Adding
  a `Tuple` branch there is genuinely one edit.
- **False for "tuple ordering".** Two more sites decide it:
  - **`methods.rs:1838`, the `"argsort"` arm.** `sort_by` desugars through `argsort`
    (`parser.rs:76`), so fixing only `sort` leaves `[(2,1),(1,2)].sort()` working while
    `[(2,1),(1,2)].sort_by(it)` still errors — *with a message naming `argsort`*. That
    replaces one inconsistency with a stranger one.
  - **`methods.rs:1316`, the `"min" | "max"` arm**, whose domain check is `numeric_vec`
    (`methods.rs:590`) — shared with 16 other reductions. `min`/`max` must be split off
    it first (see (b)); it cannot be widened in place.

  So: **one site to make `sort` accept tuples; three sites plus a `numeric_vec` split to
  make *tuple ordering* consistent.**

**Can `sort`'s comparator simply reuse the `<` implementation, so the two cannot drift?
No — not as `<` is written today.** `ops::compare` (`src/interp/ops.rs:765`) has the
signature

```rust
fn compare(op: &BinOp, l: &Value, r: &Value, line: usize, col: usize) -> Result<Value, HelixError>
```

Three properties each independently disqualify it as a `sort_by` comparator, which needs
`FnMut(&T, &T) -> Ordering` — total and infallible:

1. **It is fallible.** It returns `Err` for an unorderable pair. `sort_by` has nowhere to
   put an error.
2. **It is three-valued.** Its `Tuple` arm returns `Ok(Value::Missing)` when the deciding
   prefix contains `missing` (ADR 0001). There is no `Ordering` for "unknown".
3. **It is not a total order on floats.** It reaches `partial_cmp` and *raises* on NaN
   ("cannot compare these values (NaN?)"). A comparator derived from it is exactly the
   non-total comparator that made Rust's sort **abort the interpreter** — the process
   kill ADR 0024 fixed by moving to `total_cmp`. Reusing `<` would re-introduce it, or
   force a NaN pre-scan that contradicts `sort`'s NaN-placement semantics.

**The structure that does prevent drift** — and the one this ADR recommends if (b) is
taken — is to split *domain* from *policy*, the way Rust std does:

```
fn order_domain(a: &Value, b: &Value) -> Option<Ordering>
    // total within each comparable domain (Int/Int exact, Float/Float total_cmp,
    // Str, Dna, Tuple lexicographic by recursion); None for a cross-domain pair.
```

Both spellings then *derive* from it and the domain cannot drift, while the policies stay
deliberately different:

- `ops::compare` layers on it: `None` → the "cannot order X and Y" error; NaN operand →
  raise; `missing` in a tuple prefix → `Ok(Missing)`.
- `sort`/`argsort`/`min`/`max` layer on it: `None` → the type error before sorting
  begins; NaN → placed by `total_cmp`; `missing` → whichever answer (a) chooses.

That is the only arrangement in which "`<` orders it" and "`sort` orders it" are the same
sentence by construction rather than by two lists kept in sync by hand — which is how
they drifted in the first place.

## Decision

**ACCEPTED 2026-08-13 — all four recommendations taken (a1, b1, c1, d1 + d2).**

Decided against a stated goal: *a language people will use and build packages and
libraries on*. That criterion settles all four the same way, and it is worth writing down
why, because the four questions look independent and are not.

A library author does not experience `sort`, `argsort`, `min_by` and `argmax` as four
features. They experience them as **one concept with four spellings**, and today those four
spellings disagree about `missing`, about which types they order, and about ties. Every
such disagreement is something the author must learn by being surprised, then encode as a
workaround, then carry forever — and then every *consumer* of their library inherits it.
That is precisely the tax that stops an ecosystem forming. Four spellings of one idea must
have one policy, and if that costs a v0.2.0 breaking change, the cheapest moment to spend
it is now, at 0.1.1, with one published release and a 247-cell matrix to prove what moved.

The counts below are kept exactly as written, including the note that they are the weakest
argument here — (b) changes zero tests and is the riskiest of the four. That remains true
and is the reason (b) ships as a named v0.2.0 CHANGELOG entry rather than quietly.

**Implementation order** is the one the recommendations already imply: (a) and (d1) first
(no behaviour change, or documentation only), then (c) in its own commit, then (b) as the
release-noted widening. `tests/ordering_matrix.rs` is the specification of each step — its
diff is the review.

The original recommendations, restated for the record:

| # | Question | Recommendation | Assertions changed | Goldens |
|---|---|---|---|---|
| (a) | `argsort` vs `sort` | **a1** — `argsort` adopts `sort`'s policy (error on `missing`, accept `Dna`) | 2, in 1 fn | 0 |
| (b) | widen `min`/`max` | **b1** — widen to `sort`'s domain, ship as **v0.2.0** with a named CHANGELOG entry | 0 | 0 |
| (c) | family D's policy | **c1** — adopt the reduction policy, in its own commit, last | 4, in 1 fn | 0 |
| (d) | signed-zero ties | **d1 now** (document), **d2 as a byproduct** of (b)+(c) | 0 | 0 |

Total if all four are taken: **6 assertions across 2 test functions, 0 `.expected`
goldens, 0 example edits** — plus `tests/ordering_matrix.rs`, which is *designed* to
change and whose diff is the reviewable summary of what the release did to ordering.

The counts are deliberately printed next to the recommendations because they are the
weakest argument in this document: (b) changes zero tests and is the riskiest option
here, and (a2) changes four things and is the safest to revert. **Test-change count
measures how well the current behaviour is pinned, not how much users depend on it.**

## Consequences

- `tests/ordering_matrix.rs` exists and passes at `e267a25` — 247 cells × 3 engines,
  asserting on rendered text, never on `==`. Whichever direction is chosen, the diff to
  that file *is* the specification of the change, reviewable cell by cell. It also
  permanently pins the finding that all 247 cells are engine-identical, so a future
  divergence in this family fails loudly and is immediately distinguishable from a
  spelling question.
- Two documentation defects are now on the record and should be fixed regardless of the
  four decisions: **ADR 0024's** "NaN sorts after `+inf`, numpy-style" is false (NaN
  sorts by sign bit, and every NaN this runtime produces sorts *first*), and ADR 0024's
  "the three-valued contract covers every aggregation" is true of `argmax(xs)` and false
  of `xs.argmax()`.
- Whatever is chosen must be written into the language reference as a **table**, not
  prose: which types each spelling orders, and what each does with `missing`, `NaN`, and
  empty. Four spellings times three edge cases is exactly the shape that prose loses and
  a table keeps.
- `docs/ROADMAP.md` carries three open backlog entries this ADR supersedes (the tuple
  entry at `:2205`, the signed-zero paragraph at `:2242`, and the `argsort`/`sort` entry
  at `:2271`). They should be closed with a pointer here rather than left to drift into a
  fourth description of the same defect.
- Nothing here changes behaviour. The engines still agree, the gate is unaffected, and
  `helix` v0.1.1 remains exactly as shipped.

## Open questions

- **Should `Bool` be orderable?** Every spelling refuses it today, with four different
  sentences. `false < true` is conventional in Rust/Python and would make
  `sort_by(r => r.is_valid)` work; refusing it is also defensible. Out of scope here
  because no domain accepts it, so it is not an *inconsistency* — but it becomes one the
  moment `order_domain` is written.
- **What is the empty `min_by` supposed to say?** Under (c1) it needs a message of its
  own, and "cannot compute `min_by` of an empty array" is not obviously better than
  suggesting the caller supply a default. A `min_by(key, default:)` named argument is a
  separate ADR.
- **Does `sort` accepting tuples want a `reverse:` or key-direction argument?** The
  composite-key motivation (`sort_by(c => (c.w, c.uses))`) usually wants *descending* on
  one component, and the current workaround (negating the key) stops working the moment
  the key is a tuple. Worth deciding before tuples land in `sort`, or the first real user
  hits it immediately.

## Addendum (2026-08-24): the refusal wording tells the operator's truth

The unorderable-type refusal changed from "operator `<` needs numbers, but got
a Bool" to "`<` cannot order a Bool — it compares two numbers, two strings,
two DNA sequences, or two tuples of those", at the checker AND the runtime
(they now say the same sentence). NO ordering decision changed — the same
programs refuse — but the old wording predated string/DNA/tuple ordering and
misdescribed the operator (the stabilization sweep found the runtime still
carrying it, plus a mixed-pair variant that omitted tuples while refusing a
tuple). The ordering-matrix pins were updated in this same commit, as this
file requires.


## Addendum (v0.6.0) — frames are in scope, and NaN sorts last

This ADR scoped itself to arrays, and `tests/ordering_matrix.rs` followed: 247 pinned
cells, **zero of them a DataFrame**. The cost was measurable — the two frame backends
disagreed with each other and with array-land about where a NaN sorts, and no cell in
the matrix could see it. [ADR 0036](0036-one-semantics.md) puts frames in scope and
`tests/frame_ordering_matrix.rs` is the sibling this file should always have had.

Two rules change here:

1. **NaN sorts LAST, sign-independently**, in `sort`, `argsort`, `sort_by`, and frame
   sort on both backends. The rule this replaces was `f64::total_cmp` — ordering by
   sign bit — which is unobservable from Helix source and produced
   `[3.0, sqrt(-1.0), abs(sqrt(-1.0)), 1.0].sort()` = `[NaN, 1.0, 3.0, NaN]`: the same
   printed value at both ends of one sorted array, decided by a bit no user can see.

2. **`-0.0 < 0.0` is retained** for everything that is not a NaN, and is now EXTENDED
   to the polars frame sort, which canonicalized signed zeros to equal.

The red line this file drew at :132 — that Helix does not adopt pandas' `skipna`
default — was being crossed in the frame world by both backends, which skipped NaN in
`group().max()`. ADR 0036 policy 4 stops that; `.drop_nan()` is the visible opt-out.

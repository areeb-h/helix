# ADR 0051 — A function is compiled for what its call site knows

- **Status:** **Accepted & implemented 2026-09-07** (0.10.0-to-be). The user's decision, on
  the measurement below; the field build's ORM is the case.
- **Decision:** a call to one of the program's own functions whose arguments carry
  something the callee can use before the values are known — a record literal's KEYS, a
  top-level name the load-time sandbox holds, a scalar literal — is pointed at a clone of
  the callee made once for exactly that, in which every question that knowledge answers is
  answered in place, and the fold (ADR 0050) then runs over the clone as over any
  function. A method through a record the sandbox holds — `M.sql(spec)`, the object API a
  library builds by closing over a model — becomes a direct call of the closure held
  there, so the rule reaches it. Nothing about the runtime values is assumed.

## Context

The field build's render, measured on this box with a runtime value in every spec
(`target/bench/f72`): a `where eq` query costs 5.4 µs, of which 3.6 µs is `sql`'s fixed
cost with an EMPTY spec — a 14-name destructure (0.6), a key check over the spec's keys
(0.3), some twenty guarded `if key.is_missing()` bindings (1.5), the rest interpolation
and records — and 1.9 µs the where clause of one condition. The primitives are not slow: a
call is 10 ns, a field read 25 ns, `is_missing()` 20 ns. The render is slow because it
runs about 300 of them per call, most of them for spec keys the call does not use. A
hand-specialized `sql` for the shape `{where, limit}` — two names destructured, three
bindings, no key check — costs 2.5 µs. The field's own `prepare` + `bind`, which renders
once and binds values per request, costs 0.6 µs, level with GORM's cheapest path; the
point of this decision is that a route author never has to know it exists.

## The design

**Knowledge.** What a call site knows about an argument, as a `Binding`: `Shape` — a record
literal's keys, in order, and recursively what is known of each value; `Seq` — an array
literal of at most eight elements, and recursively what is known of each; `Global` — the
argument IS a top-level immutable name the sandbox holds; `Lit` — a scalar literal; or
`Any`. A name the sandbox does not hold is `Any`: it folds nothing, so it keys nothing.
Knowledge reads through a value's parts: `spec.page` of a shape is the shape of that key,
`xs[1]` of a sequence is what is known of its second element.

**The clone.** Made once per (function, knowledge) and memoized: a copy of the function's
body in which, scope-aware, a parameter known as `Global` or `Lit` is the global or the
literal; a `Shape`d parameter answers `p.k?` (`missing` when absent, a field read when
present), `p.get("k")`, `p.expect("k")`, `p.has("k")`, `p.keys()`, `p.values()`,
`p.items()`, `p.is_missing()` (`false`), `type_of(p)` (`"Record"`) and `p ?? x` (`p`); a
`let`-bound alias of it — the destructuring desugar's own temp — carries the knowledge on;
a call inside the clone that passes knowledge on specializes its callee in turn (four deep at
most). The clone is appended to the program and folded when the walk reaches it — a
constant `if` selects its branch, `["where", "limit"].all(SPEC_KEYS.contains(it))` folds to
`true`, `m.table` on the held model folds to its string — and what remains is the work the
runtime values genuinely need. The generic function stays, for every other caller.

**Through a record.** `M.sql(spec)` where `M` is a top-level name the sandbox holds a
record for, and `sql` a field holding a closure the record's own type does not own as a
method: the closure's body becomes a top-level function `M$sql`, each captured value a
top-level binding `M$sql$m` placed FIRST in the program (with the literal the fold writes
for it — a dict as the pairs that rebuild it), the call a direct `M$sql(spec)`, which the
shape rule then specializes. A capture with no literal (a frame) leaves the call as it is.
Where the constructor call itself was specialized — `M = mk("z")` — the capture is
already baked into the clone and nothing is hoisted.

**Reduction.** A clone is reduced as far as what is known reaches, in one pass over its
body — the pass is a partial evaluator over one function. A sub-expression closed under
the sandbox — `"city".split_once(" ")`, `SEED.s == ""`, `m.columns.contains("city")` on
the held model — is evaluated where it stands; a literal condition selects its branch; a
`map` or a `reduce` over the small literal array a shape produces (`w.items()` on
`{city: …}` is one element; eight at most) is unrolled, and a lambda applied to known
arguments becomes the `let` that binds them; a `let` bound to a tuple or array of safe
elements answers `c[0]`, `c[1]` and `c.count()`; a binding nothing reads whose value
cannot raise is dropped. A frame verb reads its arguments as written (`l.join(r, k)` with
`k` a name is a key column, `l.join(r, "id")` a join kind), so nothing inside such a
method's arguments is rewritten. For the field build's where clause, what remains of
`_where` is one branch on whether the value is `missing` and the parameter list — the
text `city = $1` is a constant, as it is in their hand-written `prepare`.

**Elements.** An array the call site wrote — `{order: ["-age"]}`, `{any_of: [{city: v},
{age: w}]}` — is a sequence inside the clone: `order.count()` is its length, and a `map` or
a `reduce` over it is unrolled over its elements, each written as the scalar the site
wrote or as `order[i]`, so what is known of the element — a literal, a record's shape —
reaches the lambda's body and, through it, the callees it calls (`clause(b, n)` with
`b` a branch's shape): the same idea one level down (the field build's §1.50a). A call of
the program's own function with literal arguments is evaluated where it stands, through
the same candidate rules and futility memo as the fold's, so `[ord1("-age"), ord1("name")]`
is the array of what `ord1` answers; and a method on a name bound to a literal — the
`join` over that array — is evaluated as the method on the literal. Only a comprehension
verb (`BOUND_FN_VERBS`) binds `it`: `cur.get(it)` inside a `map` reads the map's `it`, and
the unrolling replaces it — the first cut shadowed `it` under every method with arguments,
and `_keyset`'s unrolled body reached the engines with an `it` no binder owned. What the
field build's `order by`, `any_of` and `page` clauses render is then a constant.

**Inlining.** A clone that reduces to one of its parameters, or to a literal — a scalar,
or an array or record of literals — is not a function worth calling: the call site becomes
the argument, or the literal, when the arguments it drops have nothing to run; the clone
is not kept. That is what a validating wrapper becomes for a record literal —
`_wants_rec("page", p, eg)` is `p` — so what it wrapped is seen through; and what a clause
builder becomes for a shape — `_clauses(w)` is `["city = $1"]` — so its count and its join
fold at the site. An alias a `let` then binds — `let p = q`, the destructuring desugar's
own `$rec0 = spec` — is the name it aliases, read where the alias was, unless a later
binding or the body rebinds either name. A name is read by a call BY NAME too — the field
build's renderer binds `let f = it` over an array of closures and calls `f(p, s, c)` — and
a call by name of a replaced name is a call through the value, `it(p, s, c)`: the first
cut saw a name read only through identifier nodes, replaced those, dropped the binding and
left the call, and "`f` is not a known function" was the run's answer after a clean
`check` (§1.53).

**Types.** The checker's type map is keyed by node address, and after the fold the
compiler routes a frame verb, and the receiver-directed rewrite a method call, by what it
says of a receiver. So a function's types are snapshotted BY VALUE when the specializer
records it, before any fold frees a node of it, and a clone takes them from the snapshot; a
body the pass copies — for an unrolled `map`, a `let` made of a lambda application, a
devirtualized closure — takes the types of the live body it was copied from; the sandbox's
own copies of the program's lambda bodies (what its closures hold) carry them too and are
forgotten with the sandbox; and every node any pass drops or replaces is forgotten, the
slot it occupied included, as is a statement's root when the program grows around it (a
root is inline in its statement and moves with it — and is never a receiver). Pinned by
`every_type_the_fold_leaves_names_a_live_node`: after the checker and the fold, every key
of the map names a node alive in the program. The first cut kept the original body's
addresses and copied through them when a clone was made: the field build's §1.51 — a fold
had freed one, a typed node was allocated there, the clone's `c` received that node's
type — one under which `count` is not a method — and `c.count()` was rewritten to the
module's three-argument `count`, after a clean `check`, for one program and for no
smaller one.

**What the fold gained for this.** A method on a literal receiver (`missing.is_missing()`,
`["a"].count()`), an operator on literals, a field of a held record, an interpolation of held
names, `type_of` of a literal fold; the branch a literal condition selects replaces the
`if`, `and`, `or`, `??` or `not` — exactly and only where the language's own rule decides
without the other operand (`true and x` is not `x` for every `x`). And a name bound locally
— a parameter, a `let`, a lambda's own — is never the global of that name: the first cut
folded a parameter `M` as the top-level `M`. A name an expression binds ITSELF — a
lambda's parameter, a `let`'s name, the `it` of a method's body — is the sandbox's own to
bind, so `["page"].all(SPEC_KEYS.contains(it))` and `[1, 2].map((x) => x * 2)` are closed
and fold; the first cut counted those as locals too, and no comprehension with a
parameter ever folded.

**Bounds.** A budget of 262 144 nodes for all of a program's clones together — a clone
costs its body's size, so a small helper's clone costs little and a large function's much;
sixty-four of the largest body allowed — and at most 1 024 clones behind it; none for a
body past 4 096 nodes or a record past 32 keys; four levels of transitive specialization;
and a clone in which nothing was reduced — the function passes the record on, or reads
only what the runtime values decide — is not kept, and returns what it took. The first
cut counted clones instead, sixty-four per program and eight per function, and the field
build's harness — thirteen cases in one file — starved its later call sites: `page
offset` went through the generic clone at 3.7 µs while the same call alone rendered in
1.3 µs, and `keyset` and `any_of` never reached theirs (§1.50). Which call site loses to
a count is decided by its position in the file, which is no rule at all; a budget by
size is what the cost actually is. A RECURSION IS SPECIALIZED ONCE PER CHAIN: a call to
a function whose clone is being made carries knowledge that changed along the recursion
— `_tk(st, i + 1, acc.concat([tok]))` inside `_tk`, the index one literal higher, the
accumulator one element longer — and every level would earn a clone until the budget ran
out (the field build's tokenizer: 390 clones of `_tk`, 624 of `_scan_str`, 0.6 s to load
a 100-line file, §1.60 — the checker's §1.48 in the load path). What the recursion passes
down is measured against what the ancestor was made for: a call in which any position
shrinks — a part of the ancestor's shape, `render(p.left, n)`, however the counter beside
it grows — keeps all its knowledge, since a finite structure ends; a call in which nothing
shrinks has its changed positions generalized to `Any`, and the chain reaches a clone that
recurses into itself (the tokenizer is two clones: the entry, and one for its recursion).
`HELIX_NOSPECIALIZE=1` turns the pass off for an A/B;
`HELIX_NOFOLD=1` turns off the fold it rides on; `HELIX_FOLD_DUMP=<name>` prints what the
pass made (`1` for all of it, `all` for the whole program as the compiler sees it).

## Consequences

- The field's rendered queries on this box, min of five trials of 2 000 (their harness), specialization off → on: `where eq` 5.362 → 3.542 µs, `where+limit` 5.531 → 4.831 µs, `keyset` 7.887 → 5.393 µs, `OR two branches` 10.260 → 8.485 µs, the fixed cost of an empty spec 3.304 → 1.118 µs; `helix check` over the corpus's 93 programs 436 → 433 ms (min of 5).
- The budget by size, on the field's thirteen-case harness (four binaries built fresh,
  interleaved, min of three runs of its median of 5 trials of 2 000): `page offset`
  3.570 → 1.318 µs, `keyset cursor` 7.146 → 4.871 µs, `OR two branches` 9.631 → 7.703 µs,
  `prepared bind only` 0.612 → 0.306 µs, `where eq` 2.633 → 2.475 µs; the other cases within
  the noise, which two fresh builds of one source put at ±2%; the statements identical; the
  harness's `helix check` 18.6 → 19.4 ms — the load-time price of the clones it now gets.
- Elements (§1.50a), on the field's harness, the budget commit against this one (fresh builds, interleaved, min of three):
  Measured on the field's harness, the budget commit's binary against this one, both built fresh, interleaved, min of three runs of its median of 5 trials of 2 000: `order+limit+offset` 2.698 → 0.702 µs, `OR two branches` 7.666 → 6.454 µs, `keyset cursor` 4.848 → 1.600 µs, `page offset` 1.326 → 0.682 µs, `where eq` 2.498 → 2.118 µs, `where+limit` 3.170 → 2.684 µs, `update` 3.302 → 1.933 µs; `helix check` of the harness 18.4 → 19.7 ms; the rendered statements identical.
- Every engine runs the one program the pass produced; the differential oracle holds it
  byte-identical with and without the pass (`a_call_site_is_specialized_for_what_it_knows_on_every_engine`).
- A raise inside a literal receiver's body — `[1].map(chk(bad))` at the top level — is
  reported before the program runs, as `[chk(bad)]` always was: the receiver has an element,
  so the program meets it. A receiver the sandbox does not hold may be empty; its raise is
  the run's.
- Loading costs the clones' folding, bounded as above; the corpus check is measured in the
  commit that landed this.

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
literal's keys, in order, and recursively what is known of each value; `Global` — the
argument IS a top-level immutable name the sandbox holds; `Lit` — a scalar literal; or
`Any`. A name the sandbox does not hold is `Any`: it folds nothing, so it keys nothing.

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

**Types.** The checker's type map is keyed by node address, and the compiler routes
receiver-polymorphic verbs by it after the fold. A clone inherits the types of the nodes it
was cloned from, node by node; the sandbox's own copies of the program's lambda bodies —
what its closures hold — carry them too and are forgotten with the sandbox; a node the
pass drops is forgotten, so a reused address never answers for it.

**What the fold gained for this.** A method on a literal receiver (`missing.is_missing()`,
`["a"].count()`), an operator on literals, a field of a held record, an interpolation of held
names, `type_of` of a literal fold; the branch a literal condition selects replaces the
`if`, `and`, `or`, `??` or `not` — exactly and only where the language's own rule decides
without the other operand (`true and x` is not `x` for every `x`). And a name bound locally
— a parameter, a `let`, a lambda's own — is never the global of that name: the first cut
folded a parameter `M` as the top-level `M`.

**Bounds.** Sixty-four clones per program, eight per function, none for a body past 4 096
nodes or a record past 32 keys, four levels of transitive specialization. `HELIX_NOSPECIALIZE=1`
turns the pass off for an A/B; `HELIX_NOFOLD=1` turns off the fold it rides on.

## Consequences

- The field's rendered queries on this box, min of five trials of 2 000 (their harness), specialization off → on: `where eq` 4.967 → 3.532 µs, `where+limit` 5.642 → 4.226 µs, `keyset` 7.949 → 6.021 µs, `OR two branches` 10.571 → 9.143 µs, the fixed cost of an empty spec 3.326 → 2.135 µs; `helix check` over the corpus's 93 programs 427 → 423 ms (min of 5).
- Every engine runs the one program the pass produced; the differential oracle holds it
  byte-identical with and without the pass (`a_call_site_is_specialized_for_what_it_knows_on_every_engine`).
- A raise inside a literal receiver's body — `[1].map(chk(bad))` at the top level — is
  reported before the program runs, as `[chk(bad)]` always was: the receiver has an element,
  so the program meets it. A receiver the sandbox does not hold may be empty; its raise is
  the run's.
- Loading costs the clones' folding, bounded as above; the corpus check is measured in the
  commit that landed this.

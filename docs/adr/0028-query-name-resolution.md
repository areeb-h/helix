# ADR 0028 — In a DataFrame query, does a bare name mean the column or the binding?

- **Status:** **Accepted 2026-08-13 — a binding in scope wins; implemented.**
- **Date:** 2026-08-13
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0012 — DataFrame backend seam](0012-dataframe-backend-seam.md) (the query
  DSL this governs), [ADR 0017 — Methods and functions](0017-methods-and-functions.md),
  [ADR 0026](0026-library-performance-boundary.md) and
  [ADR 0027](0027-builtin-shadowing.md) (decided on the same criterion).

## Context

Inside `where` / `filter` / `with`, a bare identifier used to be tested against the frame's
COLUMNS first and only then against bindings in scope. That makes a library author's
parameter names into reserved words in data they have never seen:

```helix
fn above(frame, cutoff) = frame.where(@value > cutoff).count()
```

| the caller's frame | result |
|---|---|
| `dataframe({value: [1,5,9], other:  [0,0,0]})` | `2` — correct |
| `dataframe({value: [1,5,9], cutoff: [0,0,0]})` | `3` — wrong |

Same function, same argument, same values; only the caller's second column NAME differs.
`cutoff` bound to that column, so the predicate quietly became a column-vs-column comparison
(`[1,5,9] > [0,0,0]`, all true). Exit 0, `helix check` ok, and **all three engines agree,
because all three are equally wrong** — so the differential oracle this project is built on
cannot see it.

DataFrames are the flagship domain (ADR 0012), so this ruled out publishing anything
frame-facing: the author cannot defend against it, because they cannot see the caller's
schema.

## Decision: a binding in scope wins; `@name` still pins the column

Decided on the criterion the previous three ADRs used — *a language people build packages and
libraries on*.

**The hazard does not disappear, it MOVES, and that is the whole argument.** Under the new
rule a query author whose local shadows a column gets the local. That is a real behaviour
change, and it is the better one:

- A query author can SEE both names — the local and the column are in one scope, in front of
  them. The collision is local and visible.
- A library author cannot see the caller's schema AT ALL. The collision is non-local,
  invisible, and undefendable.

Trading an invisible, undefendable capture for a visible, local one is a straight improvement,
and it is the only direction in which a library can be written correctly without knowing what
the caller's data is called.

It also matches what the author already said. Someone writing `frame.where(@value > cutoff)`
has used the sigil on the name they meant as a column; the bare one is plainly the parameter.
`@name` remains the explicit column form and is unaffected — it pins the column side even when
a local shadows it.

**A bare name with no binding in scope is still a column.** `df.where(value > 3)` is
untouched, which is the case the DSL's ergonomics exist to serve.

### Rejected

- **Leave it.** The status quo is a silent wrong answer in the flagship domain, and the one
  remaining defect class this project treats as unacceptable.
- **Add a value-side sigil and change nothing else.** `@` pins the column side and has no
  counterpart today, so a library author has no way to force value context (a function call
  inside a query is a hard error, so no expression form can do it either). A sigil would work,
  but it makes the CORRECT library spelling the decorated one and leaves the undecorated,
  obvious spelling wrong — precisely backwards for the people this is meant to serve. Worth
  adding later as an escape hatch for the query author who now wants the column; `@name`
  already is that.

## Consequences

- **Breaking, and it belongs in v0.2.0** alongside ADR 0025's four ordering changes. Batching
  every semantics break into one release is the point — dribbling them out one at a time is
  what makes an ecosystem distrust upgrades.
- The blast radius measured smaller than expected: the entire gate (435 bin + 151 cli + 3
  ordering, `vmparity`, and `checkall` across 85 `.helix` files) passed unchanged. Nothing in
  the corpus, the examples, or the benchmarks relied on column-beats-local.
- Pinned by `a_query_binds_a_name_to_a_local_before_a_column`, which asserts all four shapes:
  the library case both ways, a bare name with no binding still resolving to a column, and
  `@name` still pinning the column under a shadow. That test is the only thing that can catch
  a regression here — by construction the three engines agree either way.

## Open questions

- Should a shadow be *diagnosed* rather than silently resolved? A query whose bare name has
  both a binding and a column is now unambiguous but still surprising; a note would cost
  nothing and would make the visible collision actually visible.
- `with` creates columns as well as reading them. Does the same rule apply to the name being
  DEFINED, or only to names being read? The current change treats reads uniformly; the
  defining position deserves its own look.

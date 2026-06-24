# ADR 0005 — Syntax & surface conventions

- **Status:** accepted
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude
- **Context:** detailed syntax review of the Phase 1 surface, focused on the
  "one obvious way" and "consistency for 20 years" principles.

## Context

With the Phase 1 surface working, we reviewed the small, high-frequency syntax
choices that users touch constantly. The governing test for every item:

> **A feature earns its place only if it expresses something no existing feature
> can.** Convenience that overlaps an existing way is a 20-year liability, not a
> win.

## Decisions

### 1. Element binders: `it` by default, `=>` when named — ACCEPTED

The comprehension verbs (`map`/`filter`/`where`) bind the current element to
`it`. When you need a name — nesting, or more than one binder — introduce it with
`=>`:

```text
scores.map(it + 5)                          # `it` shorthand (the 90% case)
grid.map(row => row.map(v => v + 1))        # named binders disambiguate nesting
```

`it + 5` is exactly sugar for `it => it + 5` with the parameter elided (Kotlin's
proven model). **One rule:** *you get `it` for free for one element; name your
binder(s) when there is more than one or when nesting would make `it` ambiguous.*

This is not "two ways to do one thing" — `it` and `=>` express different needs
(anonymous vs named), and the rule for which to use is crisp.

### 2. `reduce` requires explicit binders — ACCEPTED

The old `reduce(0, acc + it)` made `acc` appear by magic — a genuine wart.
`reduce` now requires a named two-parameter function; there is no implicit
`it`/`acc`:

```text
scores.reduce(0, (acc, x) => acc + x)
```

This follows directly from rule 1: more than one binder ⇒ name them. Most folds
are better served by named aggregations (`sum`, `mean`, `min`, `max`, …), so raw
`reduce` is the rare escape hatch where explicitness costs nothing.

### 3. `if … then … else` keeps `then` — ACCEPTED

`then` stays. Rust/Kotlin/Swift drop a `then`-like keyword only because they
delimit branches with `{ }` — which Helix has deliberately banned. Without
braces, the alternatives are a symbol (`=>`, which contradicts Helix's
words-over-symbols choice of `and`/`or`/`not`) or significant indentation (which
[ADR 0004](0004-functions-errors-mutability.md) rejected for CoffeeScript-style
ambiguity). `then` is the cheapest unambiguous, whitespace-insensitive,
on-brand delimiter. Branches may still span lines.

### 4. `count`, not `len`/`length` — ACCEPTED

One spelling for "how many." `count` is the SQL/LINQ spelling, matching Helix's
SQL-flavored data-ops goal. `len` and `length` are not aliases — they do not
exist.

### 5. Methods are always called with `()` — ACCEPTED (chose rule A)

Two internally-consistent rules were on the table:
- **(A)** parens always: `scores.mean()`, `scores.sort()`.
- **(B)** parens only carry arguments: `scores.mean`, `scores.sort`.

We chose **(A)**. Decisive reason: [ADR 0003](0003-collection-api.md) makes
big-collection operations **lazy and potentially expensive**, and `()` is an
honest "this does work" signal that `column.mean` would hide. Even Swift's own
API guidelines (cited in favor of (B)) reserve properties for O(1) work — so
`mean`/`sum`/`max` (all O(n)) would be methods there too; only `count` (O(1))
would qualify, and a `count`-no-parens / `mean()`-parens split is the
memorize-the-category tax we refuse. One rule — "call methods with `()`" — wins.

### 6. No synonyms — REAFFIRMED

Reaffirms [ADR 0003](0003-collection-api.md): one verb per concept, aliases
forbidden. `where == filter` is the single sanctioned dual-spelling, and only
because `where` is the data-query verb DataFrames reuse.

## Rejected alternatives

- *Implicit `it`/`acc` for `reduce`* — magic second binder; rejected (decision 2).
- *Dropping `then`* — would force a symbol or significant whitespace; rejected
  (decision 3).
- *Parens-free no-arg methods (rule B)* — hides compute cost on lazy collections;
  rejected (decision 5).
- *`len`/`length` alongside `count`* — synonyms; forbidden (decision 6).

## Consequences

- `=>` added to lexer/parser/AST as an anonymous-function form
  (`Expr::Lambda`); currently only valid as a comprehension argument. First-class
  functions (storing a `=>` in a variable) are deferred to the
  function-definition work in [ADR 0004](0004-functions-errors-mutability.md).
- `reduce` is now the one verb requiring an explicit function; everything else
  accepts `it` or an optional named binder.
- Tests and examples updated; 25 unit tests passing.

## Open questions

- When first-class functions land, does a bare `=>` value become callable, and
  what's the call syntax (`f(x)` already parses as a free call)?
- Should `=>` ever be required (not just allowed) — e.g. to forbid `it` entirely
  in deeply nested chains for readability? (Leaning no: keep `it` always legal.)

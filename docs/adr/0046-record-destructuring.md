# ADR 0046 — Record destructuring, and what an absent field answers

- **Status:** **Accepted 2026-09-04; implemented.** `let {where, limit} = spec in …` on all
  three engines, `{where, limit} = spec` inside `do { }`, and the checker refusing a name a
  known record cannot have.
- **Date:** 2026-09-04
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0001](0001-missing-values.md) (absence is a value that flows, not a
  crash — the rule this form inherits), [ADR 0004](0004-functions-errors-mutability.md)
  (`let … in` as the local-binding form), [ADR 0028](0028-binding-wins-over-column.md)
  (a binding is what the reader named, in every position — the same instinct, applied to
  fields).

## Context

A query builder written in Helix takes a *spec record* — `{where: "id = $1", limit: 10}` —
and renders it. The fields are optional by nature: one call passes `where`, another passes
`order`, most pass a subset. The only way to read an optional field was `spec.get("where")`,
a method dispatch per key, and a field report measured six of them at 38% of a render call.
`let` bound exactly one name per binding, so the six reads could not be one line either.

`spec.where` was not an answer. A plain field access refuses an absent field, which is right
for an access (a typo should not silently become `missing`) and wrong for a spec record,
where absence is the normal case. Nor could the checker be leaned on: the spec arrives as a
parameter, whose type is `Unknown`, so a compile-time specialisation of `.get("lit")` would
never fire where it mattered.

## Decision

**A destructure is a binding form, in `let` and in `do`, that reads each named field and
answers `missing` for one the record does not have.**

```helix
fn render(spec) = let {where, limit, order} = spec in …
fn render(spec) = do {
  {where, limit} = spec
  …
}
```

- **Desugared in the parser**, to one binding of the value under a throwaway name (`$rec<N>`
  — `$` cannot appear in user code) and one *absence-tolerant field read* per name. The
  value is evaluated once. Nothing downstream knows the form exists.
- **The read is a new node, not a `get` call.** `Expr::FieldOrMissing` compiles to
  `Op::GetFieldOrMissing`: a symbol scan over the record's fields, the same cost as `.a`, no
  method dispatch. A dict destructures by key the same way. It is a new AST variant rather
  than a flag on `Field` so that no pass — the module rewriter, the UFCS pass, free-name
  analysis, the JIT's kernel analysis — can treat it as a plain field read by accident: each
  had to name it.
- **Absent is `missing`.** The answer `get` gives, and ADR 0001's rule: absence is a
  condition in the data. A plain `.a` keeps refusing.
- **The checker refuses what it can prove.** Where the record's shape is known, a name it
  cannot have is a mistake — `record has no field `limt`` with `did you mean `limit`?`, the
  words `.a` uses. Where the shape is `Unknown`, the read is `Unknown`. A receiver the
  checker can prove has no fields (`let {a} = 5`) is refused there; one it cannot see is
  refused by both engines at run time, with the same sentence.
- **No renames, no nesting.** `{a: x}` is refused with the spelling that does what was
  meant (`x = spec.a`); the field binds under its own name. Nested patterns are not in this
  decision. (The top-level statement form was excluded at first and landed the same day —
  see the addendum.)

## Consequences

- Six `get` dispatches become six symbol scans and one line. Measured on one binary,
  interleaved min-of-7, 2M calls, six lookups with three present: 392 → 358 ns per call
  (1.10×). Smaller than the field profile's 38% suggested — on a three-field record the
  `get` dispatch is ~40 ns — so the point of the design is that the fast spelling and the
  short spelling are the same spelling, not that the fast one is dramatically faster.
- The engines cannot drift on the form itself — there is one desugar — and can drift only on
  the new node, which has one arm in each engine (`eval_field_or_missing` shared by both)
  and is pinned by `rec_destructure` in the corpus under both DataFrame backends.
- A record that merely *begins* a `do` block's result expression is still a record: the
  binder is recognised by looking (`{`, names and commas, `}`, a single `=`), never by
  parsing and backtracking.

## Alternatives considered

- **Error on an absent field.** Consistent with `.a`, and wrong for the only use that
  asked: a spec record's fields are optional. It would also have made the form strictly
  worse than `get`, which it exists to replace.
- **Specialise `rec.get("lit")` in the compiler.** Fires only when the checker knows the
  receiver is a record — never for a parameter. The destructure carries its intent in the
  syntax, so it works where the type is unknown.
- **Require an explicit optional marker (`{limit?}`).** More to write for the common case,
  and the checker already catches the case a marker would protect against — a typo against
  a known shape.

## Addendum 2026-09-04 — the statement form

`{where, limit} = spec` at the top level, with `mut` and `export` in front as for any
assignment. It is the `let` desugar spread over assignments: `$rec<N> = value` (an
immutable temp with its own name, so the checker keeps the record's shape through it —
a `mut` global would read as `Unknown`), then one assignment per field through
`FieldOrMissing`. The parser queues the extra statements and `program` appends them. Every
rule an assignment has — `mut`, `export` (each field, never the temp), rebinding an
immutable, the module's export list, the checker's shape refusal — applies per field on
every engine, because each field binding *is* an assignment. Pinned by
`record_destructuring_is_also_a_statement` and the corpus program.

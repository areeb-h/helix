# ADR 0054 — Column-name positions: one rule, one escape, and the `with` key

- **Status:** **Accepted & implemented 2026-11-03 — D4: Option A (a bare `with` key stays
  binding-wins).** D1 records what v0.10.0 already ships; D2 and D3 are additions (nothing that
  ran before changes meaning), so the whole decision is non-breaking. Pinned by
  `at_name_pins_the_column_in_every_column_name_position`,
  `the_lint_notes_a_top_level_string_used_as_a_with_key` and the corpus program
  `df_column_name_shadowing`.
- **Date:** 2026-11-03
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0028](0028-query-name-resolution.md) (a binding wins in a query's
  EXPRESSION positions; its 2026-09-02 addendum extended that to `with` keys and join keys),
  [ADR 0052](0052-predicates-are-values.md) (why `@w` outside a verb that reads it is a
  Record), [ADR 0012](0012-dataframe-backend-seam.md) (the query DSL).

## Context

ADR 0028 settled what a bare name means where a query reads a VALUE (`where`, `filter`, a
`with` expression): a binding in scope wins, `@name` pins the column. It never reached the
positions where a frame verb expects a column's NAME — `sort`, `select`, `group`, an
aggregation's value column, `column`, `unique`, `rename`, a `with` key, a join key.

v0.10.0 closed that gap in code, and the decision lives in exactly two places: one CHANGELOG
paragraph (`with` and `join` "now take a column name from a binding … `select`/`sort`/`group`
resolve a bare word through the column-name rule") and the comment atop
`tests/corpus/df_column_name_shadowing.helix`. `docs/STATE.md` meanwhile still listed it as an
undecided breaking change needing an ADR. This ADR is that record, and an audit of whether the
rule actually holds everywhere it claims to.

### What the binary does today (measured 2026-11-03, `1bb6c44`, all three engines identical)

`w = "v"`, and every frame below also has a column called `w`:

| position | `X(w)` acts on | `X(@w)` | `X("w")` |
|---|---|---|---|
| `sort` | `v` ✓ | column `w` | column `w` |
| `select` | `v` ✓ | column `w` | column `w` |
| `group` | `v` ✓ | column `w` | — |
| aggregation value, `.sum(w)` | `v` ✓ | column `w` | — |
| join key | `v` ✓ (1 row; `@w`: 4) | column `w` | — |
| `column` | `v` ✓ | **refused** — "expected a column name string, found a value of type Record" | column `w` |
| `unique` | `v` ✓ (2 rows; `"w"`: 3) | **refused**, same message | column `w` |
| `rename` (old name) | `v` ✓ | **refused**, same message | column `w` |
| `with` key, `{w: e}` | **defines `v`** | does not parse | refused: "`with` takes a record of new columns" |

The same calls inside `fn lib(F, k)` with `k` a parameter give the same answers — the
"top level and a function resolve differently" inconsistency STATE.md recorded is gone.
Two edges behave well and should stay as they are:

- A binding that is not a String: `num = 5; N.sort(num)` → "`num` is a Int in scope, and a
  column name must be a String", help "write `@num` for the column called `num`, or bind a
  String naming the column you want."
- A bare name with no binding is the column (`D.sort(v)`), and with neither a binding nor a
  column it is "no column `nope`", listing the columns.

### Two defects the table shows

**1. The escape hatch has holes.** "`@name` pins the column" is the sentence ADR 0028 and the
corpus pin both lean on, and `column`, `unique` and `rename` refuse it with an error that
names the wrong thing ("a value of type Record" — `@w` evaluated as an ADR 0052 condition,
because these three take their argument as an ordinary value). A string literal still
reaches the column, so nothing is unreachable; it is an inconsistency and a bad message.

**2. The `with` key captures in the other direction, and has no escape at all.**

```helix
name = "Ada"
P = dataframe({id: [1, 2], name: ["x", "y"]})
P.with({name: "z"}).columns()    # ["id", "name", "Ada"] — a column called "Ada"
P.with({name: "z"}).column("name")   # ["x", "y"] — untouched
```

Exit 0, `helix check --lint` silent, three engines agreeing. ADR 0028's addendum chose
binding-wins for the `with` key to fix `fn rename(f, to) = f.with({to: @author_id})`, and it
did — but in a SCRIPT, where top-level String bindings are common (`name`, `label`, `title`,
`path`), any of them that shares a name with a column being defined silently redirects the
definition. And unlike every read position, the defining position has no way back: `{@name:
e}` does not parse and a string-keyed `{"name": e}` is a Dict, which `with` refuses. With such
a variable in scope, the column cannot be defined through `with` except by renaming the
variable.

This is the mirror image of the bug ADR 0028 exists to prevent, and ADR 0028's own argument
explains why it is milder: the script author can see both names in one file; the library
author cannot see the caller's schema at all. Milder is not fine — it is still a silent wrong
answer — and it has no spelling that avoids it.

## Prior approaches

| system | bare name in a column-name position | programmatic name | the documented pain |
|---|---|---|---|
| pandas | n/a — `df[col]` is always the variable's value | the same | `df.query("x > @var")` puts the sigil on the VARIABLE, the opposite of Helix's `@col`, so the two habits collide |
| dplyr (tidy eval) | the data column wins ("data masking") | `{{ var }}`, `.data[[var]]`, `.env$var` | the data-wins default captured function parameters in exactly ADR 0028's way; `rlang` grew three generations of escapes (`!!`, `{{ }}`, `.data`/`.env` pronouns) to let library authors opt out |
| Polars | none — names are strings: `pl.col("x")` | `pl.col(var)` | no ambiguity, at the price of quoting every column everywhere |
| SQL | an identifier is always a column | dynamic SQL / format strings | programmatic names mean string-building, the injection surface |

dplyr is the cautionary row: it chose the opposite default to Helix (data wins), and its
library-author story became a sequence of escape syntaxes. Helix's default is the one dplyr's
pronouns exist to reach. What Helix still owes is one escape that works in every position.

## Decision

**D1 — Record the rule v0.10.0 ships (no behaviour change).** In every column-name position —
`sort`, `select`, `group`, an aggregation's value column, `column`, `unique`, `rename`, a join
key, a `with` key — a bare name that is a **String binding in scope** names the column its
value spells; a bare name with no binding is the column of that name; a binding of any other
type is an error that names `@name`. One rule, identical at top level and in a function body.

**D2 — `@name` pins the column in every column-name position (an addition).** `column(@w)`,
`unique(@w)`, `rename(@w, …)` accept the sigil the way `sort`/`select`/`group`/`join` already
do, and `with` accepts `{@name: expr}` to define the column literally called `name`. Nothing
that runs today changes meaning: each of these is currently a refusal or a parse error. The
`Record` message disappears with it.

**D3 — Say so when a TOP-LEVEL String binding is a bare `with`/`agg` key (a lint note).**
ADR 0028 left "should a shadow be diagnosed?" open; this answers it narrowly, for the defining
positions — the only ones where the capture is silent. `helix check --lint` notes a bare `with`
or `agg` key, in a top-level statement, naming a String bound by an EARLIER top-level
statement: "`name` in this `with` key is a top-level String, so it defines the column `Ada`,
not one called `name` — write `@name` for the column called `name` (ADR 0054)". Function
bodies are exempt, and so is a name the statement rebinds (a lambda parameter, `let`/`do`):
that is the library case, where the binding is the whole point and a note on every correct call
would train people to ignore the lint. Notes never change the exit code.

**D4 — What a bare `with` key means. Options, for decision:**

- **Option A — keep binding-wins (today), rely on D2 + D3.** The script author gets an escape
  (`{@name: …}`) and a note that points at it. Non-breaking. The capture remains the default
  for top-level String bindings; it becomes visible rather than impossible.
- **Option B — a bare `with` key is literal, like every other record key in the language;
  computed keys get explicit syntax** (`{[to]: @author_id}`, or a two-argument
  `with(to, @author_id)`). Breaking: it reverts the v0.10.0 behaviour for `with` keys only, and
  `fn rename(f, to) = f.with({to: …})` would need the new spelling.

**Decided 2026-11-03: A.** It is the only option with no break. Option B buys consistency with
record literals, but it would be the second semantics change to the same position in two
releases. ADR 0028's own rule is "batch every semantics break into one release — dribbling them
out one at a time is what makes an ecosystem distrust upgrades." And D3 makes the remaining
hazard visible exactly where it lives (top-level script bindings). If the lint proves noisy or
the capture keeps biting, B remains available as one batched break later.

## Consequences

- D1 closes `docs/STATE.md`'s queue item 2. Its three original questions are answered: a
  binding wins (yes, shipped); a String is accepted (yes, shipped); top level vs a function
  (no longer differs).
- D2, as built: `column`/`unique`/`rename` read a column name from a String or an ADR 0052
  column node (`access::column_name_value`), which is what `@w` evaluates to. The parser accepts
  an `@name` key only in a record that opens directly after `.with(` or `.agg(`, storing it as
  `"@name"` (no identifier starts with `@`); the frame verb's key rule strips it. Anywhere else
  it is a parse error, and a pinned key built as an ordinary record — a `with` whose receiver
  is not a frame, such as a user function named `with` — is refused at run time in one
  sentence on all three engines (`pinned_key_outside_frame`), never turned into a field called
  `"@name"`.
- D3, as built: an AST pass in `lint_source` beside the other lints, top-level statements only.
- The corpus pin `df_column_name_shadowing` gained the `@` escapes for `column`/`unique`/
  `rename` and the `with`-key escape; its earlier output is unchanged.

## Open questions

- Should D3 also cover the READ positions (`select(name)` with a top-level `name` bound to a
  String)? There the failure is loud ("no column `Ada`"), so a note adds less; starting with
  the `with` key alone is the conservative cut.
- The predicate side of ADR 0052 makes `@w` a value outside the verbs that read it. Should
  `column`/`unique`/`rename` accept a whole ADR 0052 column node (`{kind: "col", name: "w"}`)
  generally, or only the `@name` spelling? D2 as written takes the spelling, which is what
  users type.

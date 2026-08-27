# ADR 0039 — String predicates inside a DataFrame query

**Status:** Accepted (v0.6.x)
**Supersedes nothing. Extends:** ADR 0012 (the DataHandle seam), ADR 0034 (a column
expression means what the same expression means on scalars), ADR 0036 (one semantics).

## The gap

A query accepted column references, literals, arithmetic and comparisons. Every string
method on a column was refused:

```helix
df.where(@diagnosis.starts_with("h"))       # error: this expression isn't
df.where(@diagnosis.contains("tens"))       #        supported inside a
df.where(@gene.re_match("^BRCA"))           #        DataFrame query yet
df.with({flag: @diagnosis.starts_with("h")})
```

This was found the direct way. A field report had just been handed a linear-time regex,
was reading a frame of genes, typed the first thing anyone would type, and got a refusal —
with the diagnostic pointing at **line 1 of the program**, because that arm alone among its
siblings hardcoded position `0, 0`.

For a language whose stated ground is VCF, FASTQ, GFF and CSV, `where(@gene.re_match(…))`
is not an advanced feature. It is the first query.

## What was NOT true

The report concluded that a string filter on a frame was "not expressible — not in a
query, not via `with`, not by dropping to an array and back". The last clause is wrong,
and being precise about it changes what this ADR is for. `to_json` is a *universal*
method, so the loop closes:

```helix
df.to_json().parse_json().where(it.get("gene").re_match("""^BRCA""")).to_dataframe()
```

That works today. It is a bad way to work: on 200,000 rows it costs **2,174 ms** where the
same regex over one column costs **57 ms** — the 2,117 ms difference is an entire frame
serialized to JSON text and rebuilt, and it also alphabetizes the columns and raises when
the filter matches nothing.

So this is a **usability** decision, not a capability one. That matters, because it is the
argument against also shipping an escape hatch (below).

## Decision

Admit a **closed, Bool-only** set of String methods into `ColExpr`:

```
starts_with(s)   ends_with(s)   contains(s)   re_match(pattern)
```

as one variant, `ColExpr::StrMethod(StrFn, Box<ColExpr>, Vec<Value>)`.

### The set is closed

For the same reason `FloatPredKind` is. Three separate functions over `ColExpr` must answer
for every variant, and an open name set makes all three lie:

- `may_be_float` would have to say "maybe", installing a NaN guard on text;
- `non_numeric_operand` would have to say "don't know" — which is exactly the route by
  which `@s.re_find("x") + 1` reaches polars' `+`, **which concatenates two `str`
  columns**. That is the sixteenth divergence of ADR 0036, re-entering through a new door;
- `validate_predicate` could not decide whether a call is a condition.

An open set would also admit `.split(",")`, whose `Array` result one backend can store and
the other refuses — a guaranteed divergence with no matching error text on either side.

### The set is Bool-only, deliberately, for now

Bool covers the whole of `where`/`filter` and the flag-column use of `with`. The
String-returning half (`re_find`, `re_replace`, `upper`) is a strictly larger change: it is
the first time `validate_predicate` must say "this is a String, not a condition", and the
first time `non_numeric_operand` answers something other than `Bool`. It gets its own
increment rather than riding along on this one.

### The arguments are values, not expressions

`Vec<Value>`, resolved by `ast_to_colexpr` before the IR is built. A Helix variable still
works (ADR 0028 resolves it to a literal); another **column** is refused by name:

```
error: `starts_with` inside a DataFrame query needs a fixed argument, not one that
       varies by row
help: pass a literal or a variable — comparing two columns of text is not supported yet.
```

This is what makes *"a regex is compiled once per query, never per row"* a property of the
IR's shape rather than a promise in a comment.

### Both backends call the scalar kernel

Not `polars.str().starts_with()`. This is the ADR 0036 decision restated, and the specific
hazards are not hypothetical:

- polars' `contains` is a **regex** by default; Helix's `contains` is literal text. Same
  name, different language, silently.
- polars' non-strict `contains` answers an **all-null column** for a bad pattern instead of
  raising.
- its null rules are its own.

Each is a place where a frame could come to mean something the language does not, and
proving all three still match is work that must be redone at every polars upgrade. Calling
`call_method` per cell costs an allocation and buys the guarantee outright — the same trade
`guarded_arith` and `guarded_compare` already made.

### One probe settles five sentences

`probe_str_call` asks the **scalar kernel** with a stand-in receiver, once, before any row:

| probe | answers |
|---|---|
| `""` | arity, argument type, an invalid pattern, and a build with no `regex` feature (ADR 0032's gate-the-body twin — which the `appliance` profile is) |
| `Int(0)` / `Float(0.0)` / `Bool(false)` | `` an Int has no method `starts_with` `` |

Both backends call it, so none of those sentences can drift between them, and none carries
a row number — **a type error is not a cell error**: every row fails identically, the
column is what is wrong, and "at row 0" invites you to inspect blameless data.

### The type question is decided from the SCHEMA on both sides

polars from its `fields`, native from the column's own `Col` kind. Deciding it from the
first *value* — which is what the native engine does for arithmetic — would disagree on an
**empty frame**: there is no value to learn from and the column is still the wrong type.
`tests/corpus/df_string_predicate_empty_frame.helix` pins that case.

### `missing` propagates

`missing.starts_with("h")` is `missing`, not `false` (ADR 0001), exactly as on a scalar.

This is the dangerous rule, and it is dangerous in a specific way: under `where`, a row
answering `false` and a row answering `missing` are both dropped, so **the two backends
agree**. The divergence appears only in `with({flag: …})`, as a wrong *column* at exit 0.
Sabotaging it confirmed this precisely — the nine `where` counts in the corpus program were
byte-identical while only the `with` line moved.

## Rejected: shipping an escape hatch alongside

**A mask verb (`df.take(mask)`) — no, and not later either.** It is a second spelling of
`where` (ADR 0003), and it would be the only verb whose argument must be an array of the
frame's exact length, a shape nothing else in the API has and nothing can check before
collect.

**Rows-out (`df.rows()`) — not here.** The JSON route already closes the loop, and these
predicates are what make it unnecessary. Shipping a hatch in the same change would ship a
second answer to a question just answered. If it is wanted later it is a *value* method
with its own ADR, and it needs its own answer to "what happens on a 10M-row frame".

## Consequences

- `helix check` stays deliberately blind to query arguments (`types/synth.rs`: the runtime
  schema is the boundary). Synthesizing them would reject every existing query.
- The catch-all refusal now reports the **node's own position**, so every unsupported
  expression in a query points at itself. That is a fix to a diagnostic that was wrong
  before this change and for every expression, not only these.
- A per-cell `call_method` is one allocation per row. The dictionary fast path — `Col::Str`
  is dictionary-encoded, so the predicate need run once per *distinct* value — is a pure
  performance follow-up with no semantics, and is where 10M rows over eight distinct
  diagnoses becomes eight regex runs.

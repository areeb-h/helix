# ADR 0043 — row windows, and a schema that is not syntax

**Status:** Accepted & implemented
**Date:** 2026-08-30

## The question

Two gaps, reported from the field while building a chunked, reclusterable store on Helix.
They look unrelated and are the same shape: **the language had no verb, so the caller had to
supply a closure.**

1. A DataFrame had `head(n)` and nothing else — no `tail`, no row-offset slice. So a sorted
   frame could not be cut into chunks, which is the whole of "recluster by a column".
2. `dataframe()` takes a RECORD, and record fields are *syntax*. `to_dataframe()` refuses an
   array of dicts. So a frame whose column names are known only at run time could not be
   built at all — and a store's chunk takes its schema from the data, never from the source
   text.

The report's own summary is the argument: `recluster` ended up taking a caller-supplied
`slicer` closure, "the third place blocker-1's shape has propagated".

## Decision

### `tail(n)` and `slice(offset, len)`, through the ADR 0012 seam

Both backends implement them, so `dfdiff` holds them identical.

**`tail` is its own verb, not sugar over `slice`.** Expressing it as one needs the row
count, which a LAZY frame does not cheaply have — so `df.tail(5)` would silently force a
materialization of a frame the caller had been careful to keep lazy.

**Both clamp; neither refuses.** An offset past the end is an empty frame, a length past the
end is short. This is the same rule `read_at` follows (ADR 0041) and for the same reason: a
window running off the end is how the *final partial chunk* reads, and erroring would force
the caller to compute the row count first — which, again, means materializing.

A NEGATIVE offset or length is refused rather than clamped, because there is a real question
behind it ("count from the end") and that question already has an answer: `tail`. Silently
treating `-1` as `0` would answer a different question than the one asked.

Order against a query is meaningful and not accidental: `.where(p).slice(0, n)` takes the
first n matching rows, `.slice(0, n).where(p)` filters the first n rows. Both are pinned in
the corpus, because a reader who assumes they commute will be wrong.

### `dataframe(dict)` as well as `dataframe(record)`

The dict form is the only way to name a column at run time.

**Column order differs between the two, deliberately and visibly.** A record keeps the order
written. A `Dict` is a sorted map, so `dataframe(d)` yields columns in sorted name order.

Sorting is the honest choice rather than a limitation: a `Dict` *has* no insertion order, so
inventing one would be a claim about where the frame came from that nothing can support.
Both forms are deterministic, which is what actually matters; `select` fixes an order when it
matters to the reader.

**A non-string key is refused by name**, not stringified. `1` and `"1"` would otherwise
become the same column, and silently merging two columns is a wrong answer, not a coercion.

## Why not the alternatives

**`to_dataframe([dict])` (an array of row-dicts).** A row-wise constructor for a columnar
engine: it would transpose on every build, and the schema would come from whichever keys the
first row happened to have. The column-wise form says what it means.

**Make `slice` refuse an out-of-range window.** Then every chunked scan must know the row
count before it starts, and on a lazy frame that is a materialization — the exact cost these
verbs exist to avoid.

**Let `dataframe(dict)` preserve insertion order.** A `Dict` does not have one. Preserving
it would mean changing `Dict` itself, which is a much larger decision about a different type.

## Consequences

- A sorted frame can be cut into chunks with no caller-supplied closure, and a chunk's
  schema can come from the data.
- Two corpus programs pin both: they are checked by `dfdiff` across both backends, by
  `vmparity` across all three engines, and against a hand-verified golden.
- The clamping rule now reads the same in three places — `read_at`, `read_bytes_at`, and the
  frame window — which is one rule to learn rather than three.

## Still open

`to_dataframe` continues to refuse an array of dicts, and that is now the only row-wise shape
anyone reaches for. If it earns a decision it should be a separate one, with an answer to
"whose keys define the schema" that is better than "the first row's".

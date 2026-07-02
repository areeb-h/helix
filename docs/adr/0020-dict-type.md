# ADR 0020 — `Dict`: a keyed map with O(log n) lookup

- **Status:** Accepted (implemented)
- **Date:** 2026-06-29
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0001 — Missing](0001-missing.md),
  [ADR 0003 — Collection API](0003-collection-api.md)

## Context

Helix had no keyed-lookup type. Every count / vocabulary / bigram lookup was an O(n)
scan — `xs.where(…)`, `.contains`, `index_of`, or `frequencies()` (which returns
`[(key, count)]`, so reading *one* key's count is still O(n)). A downstream language-
modelling workload measured the cost directly: a vocabulary-coverage pass over an 11k-word
table ran **21 s** (≈160M ops) before being sampled down, and the project's test suite
went **46 s → 71 s**, dominated by O(table) lookups. This was the single largest source of
slowness — the difference between toy-scale and real-scale.

## Decision

Add a first-class **`Dict`** value: an immutable key→value map.

- **Backing store: `BTreeMap`, not a hash map.** Lookup is O(log n) — the decisive win
  over O(n), and at 11k keys that's ~13 comparisons vs ~11000. The reason for a *tree*
  map over a hash map is **determinism**: Helix guarantees reproducible output, and a
  `HashMap`'s iteration order is randomized, so `keys()`/`values()`/`items()`/`Display`
  would be non-deterministic. A `BTreeMap` iterates in sorted key order for free. (True
  O(1) would require a hash map plus sort-on-enumeration; deferred — O(log n) already
  collapses the bottleneck.)
- **Keys are hashable scalars** (`DictKey`: int, string, bool, DNA). Floats are excluded
  (NaN has no total order; float-equality keys are a footgun), as are arrays/records — so
  comparison is always cheap and the ordering is well-defined. A bad key is a clear error.
- **Immutable**, like every Helix value: `insert`/`remove` return a *new* dict. Bulk
  construction is `pairs.to_dict()` (O(n log n)); `dict()` makes an empty one to grow with
  `.insert`. (`insert` currently clones the map — O(n) per call — so a build-in-a-loop
  should prefer `to_dict`; a copy-on-write fast path for the uniquely-owned case is a
  future optimization.)
- **Surface:** `get(k)` / `d[k]` (absent → `missing`, the safe accessor), `contains(k)`
  (alias `has(k)` — `contains` reads naturally on a collection, `has` on a keyed map;
  both dispatch to the same lookup, no synonym cost since it's one word for one concept),
  `keys()`, `values()`, `items()` (sorted), `count()`/`length()`, `insert(k, v)`,
  `remove(k)`; the free function `dict()`, the array method `to_dict()`, and
  `frequencies().to_dict()` to turn a histogram into a count lookup. Displays as
  `{k => v}` (arrow notation, sorted) — visually distinct from a record's `{field: v}`.
- **JSON:** `d.to_json()` serializes a `Dict` as a **JSON object** (`{"k": v, …}` in
  sorted key order, so output stays byte-reproducible), the natural inverse of
  `str.parse_json()` producing a keyed map. String keys emit directly; non-string
  scalar keys (int/bool/DNA) render as their string form (JSON object keys are always
  strings). This makes `Dict` a first-class member of the JSON round-trip alongside
  records and arrays.

## Consequences

- **The O(n) lookup tax is gone.** `xs.frequencies().to_dict()` then O(log n) `get` per
  query replaces the per-token table scan; the measured 21 s / 71 s pipelines collapse.
- **Determinism preserved** — sorted iteration means a dict-bearing program is still
  byte-reproducible, so it can appear in vmparity examples and golden output.
- **Minimal type-system footprint.** A `Dict` is a *runtime* type; the checker treats
  `dict()`/`to_dict()` as `Unknown` (like `parse_json`'s result), so `.get`/indexing stay
  permissive without a new `Type` variant or per-method signatures. Enforcement and
  dispatch live in the shared `call_method`/`eval_index`, so the VM and tree-walker run
  the identical code — vmparity covers it; no JIT/oracle surface.
- **A `Set`** is a thin future addition (a dict with unit values, or `to_set()`); the
  membership need is already met by `dict.contains`.

# ADR 0017 — Methods on data, free functions for the rest (no namespaces)

- **Status:** Accepted (implemented)
- **Date:** 2026-06-27
- **Deciders:** Areeb + Claude
- **Supersedes:** the native-namespace decision in
  [ADR 0011](0011-core-stdlib-boundary.md) (registry + small-core survive)

## Context

ADR 0011 grouped domain builtins under compile-time **namespace prefixes**
(`bio.read_vcf`, `stats.t_test`, `io.write_csv`, `json.parse`, `chart.bar`,
`export.html`). But `bio`/`stats`/`io`/… were never real modules or values — a
compile pass rewrote `io.write_json(x)` into a flat call keyed by the literal
string `"io.write_json"`. So the dot **looked like** library/member access while
nothing was being accessed (`let f = io` was an error), and the grouping had drifted
into a grab-bag (terminal charts were `chart.bar` but their SVG twins
`export.svg_bar`; writing JSON was `io.write_json` but serializing it was
`json.stringify`). The convention pretended to be something it wasn't.

Research into developer ergonomics and mental models reinforced the fix: method
chaining and the pipe are the same idea (in fluent languages "the `.` *is* the
pipe"), and the dot + autocomplete is the strongest discoverability lever. Helix is
already method-first (`xs.map().filter()`, `df.where().select()`), so the honest,
consistent, discoverable design is **methods on data** for data verbs and **plain
functions** for constructors, pure math, and symmetric operations — which is also
how pandas/scipy actually split (`df.to_csv()` method, `scipy.stats.ttest_ind(a,b)`
function, `pd.read_csv` constructor).

## Decision

Remove every namespace prefix. Each former namespaced builtin becomes one of:

1. **A method**, when it acts on data you already have (transform, analyze,
   serialize, output, chart). Dispatch is on the receiver's real type — no fake
   module. Examples: `value.to_json()` (universal), `s.parse_json()`,
   `xs.zscores()` / `xs.iqr()`, `xs.bar_chart(labels)` / `xs.histogram(bins)` /
   `xs.line_chart()` / `xs.sparkline()` / `xs.scatter(ys)` / `xs.svg_bar()`,
   `recs.mean_gc()` / `recs.to_html()` / `recs.write_csv(p)`,
   `df.write_parquet(p)` / `df.to_markdown()`, `text.write_to(p)`.
2. **A free function**, when it *constructs* data from a source, is pure math, or
   is a symmetric operation with no privileged receiver. Examples: `read_csv(path)`
   and the other `read_*`, `http_get(url)`, `normal_cdf(x)` / `normal_pdf(x)`, and
   the two-sample/fit operations `t_test(a, b)`, `correlation(a, b)`,
   `linear_regression(x, y)`, `multiple_regression(X, y)`.

No pipe operator is introduced: in a method-first language the `.` already serves
that role, so adding `|>` would be a second way to chain (against "one obvious
way"). The DataFrame **backend seam**, the **registry** as single source of truth,
and the **small-core** boundary from ADR 0011 are unchanged.

## Consequences

- **Discoverability:** typing `data.` and browsing completions now surfaces the
  whole data API (read → transform → analyze → serialize → write), uniformly.
- **No fakery:** there is nothing that looks like a module but isn't. A value is a
  value; a method dispatches on it; a function is a function.
- **Engine parity:** methods route through the one shared `call_method`; the two
  DataFrame serialize paths (`df_value_method` for the VM, `eval_df_method` for the
  tree-walker) delegate to one shared implementation, so the differential oracle
  and `vmparity` stay byte-identical.
- **Migration:** pre-0017 code using an old prefix gets a precise error — e.g.
  `stats.t_test(a, b)` → "`stats.t_test` is no longer available; it is now the free
  function `t_test(...)`", and `json.parse(s)` → "now the method `s.parse_json()`".
  A local that shadows a former prefix name (`stats = python.import(...)`) is
  unaffected. `src/namespace.rs` no longer rewrites anything — it survives only to
  produce these hints.
- **Performance:** purely a naming/dispatch change; no hot path was touched
  (methods already dispatched through `call_method`), so the benchmarks are
  unaffected.

## Alternatives considered

- **Keep the dotted convention** (status quo): rejected — it pretends to be module
  access and the grouping had drifted.
- **First-class namespace values** (`io` becomes a real module object): more
  machinery for the same call sites; doesn't fit the method-first style.
- **Functions + a pipe operator** (the Julia/R model): principled, but Helix is
  already method-first; adopting it half-way means two ways to chain, and adopting
  it fully would mean converting the existing method core to functions — a larger,
  less idiomatic change.

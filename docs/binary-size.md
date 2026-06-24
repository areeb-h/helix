# Binary size and the Polars dependency

The Helix binary is large (~65 MB release, default features) with ~1000 transitive
crates — including an async runtime (`tokio`) and a cloud object-store
(`object_store` → `reqwest`/`hyper`). This documents *why*, what was investigated, and
why it is not currently fixable without a major change — so the question is not
re-litigated.

## What is and isn't true

- **Self-contained: yes.** The default binary links only the system C runtime
  (`libc`/`libm`/`libgcc`) — no Python, no system BLAS, no OpenSSL, no zlib. The
  "single self-contained binary" claim holds.
- **Fast startup: yes.** The async runtime and object-store are *linked* but never
  *started* for local file operations — there is no network or async work on the hot
  path. The cost is binary **size** (linked code), not startup latency.
- **Small: no.** Polars and Cranelift dominate; trimming is not possible via features.

## Why it cannot be trimmed (verified against the Polars 0.54 manifests)

The async/cloud tail is reached by:

```
polars {csv, parquet, ipc, json, dtype-full, performant, cum_agg, …}
  → forces the `streaming` feature
  → pulls polars-stream
  → polars-stream depends on polars-io with features = ["async", "file_cache"]  (UNCONDITIONAL)
  → file_cache → polars-io/cloud → object_store → reqwest → hyper → tokio
```

The decisive fact is that `polars-stream`'s dependency on `polars-io` hard-codes
`["async", "file_cache"]` — it is not behind a feature. And essentially every
substantive Polars feature (`csv`, `parquet`, `dtype-full`, `performant`, …) enables
`streaming`. So *any useful* Polars 0.54 build pulls the whole tail.

Investigated and ruled out:

1. **Cargo features** — disabling `default-features`, dropping `streaming`, and even
   dropping `csv`/`parquet`/`ipc`/`json` all leave `object_store` in place, because
   `dtype-full`/`performant`/`cum_agg` re-force `streaming`.
2. **Polars upgrade** — 0.54.4 is the latest published version; there is no newer
   Polars to move to.
3. **Lighter file readers** (read CSV/Parquet via standalone crates, keep Polars for
   compute) — does not help, because the compute features themselves force `streaming`.

## The only real fixes

- **Patch/fork Polars** to make `polars-io`'s `file_cache`/`cloud` optional — a fragile
  maintenance burden across Polars updates. Rejected as a workaround.
- **Replace the DataFrame backend** (e.g. arrow-rs + standalone parquet/csv readers) —
  a multi-week rewrite of the DataFrame layer that would genuinely shed the tail, at
  the cost of Polars's optimized lazy scan/pushdown. A major architectural decision,
  warranted only if binary size becomes a real adoption blocker.

Until then the trade-off is accepted deliberately: a large but genuinely self-contained,
fast-starting binary, in exchange for the maturity and speed of the Polars engine.

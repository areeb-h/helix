# Binary size and the Polars dependency

The Helix binary is large (~65 MB release, default features) with ~1000 transitive
crates — including an async runtime (`tokio`) and a cloud object-store
(`object_store` → `reqwest`/`hyper`). This documents *why*, what was investigated, and
why it is not currently fixable without a major change — so the question is not
re-litigated.

## Update (2026-06-25) — re-measured, and the seam that changes the calculus

Two refinements after the ADR 0012 backend-seam work:

- **The tail is even more unconditional than the streaming path below suggests.**
  Re-measured on the pinned 0.54.4: `cargo tree -i object_store` shows `object_store`
  entering via **`polars-error`** — a crate every Polars build pulls — not only via the
  `streaming`/`csv`/`parquet` chain. So no feature combination that keeps Polars at all
  can shed it on 0.54.4. (Upstream has since made `object_store` optional and treats the
  unconditional pull as a bug; the honest path is to track/contribute that fix, not to
  `[patch]`-hack it.)
- **"Replace the backend" is no longer a layer rewrite.** The DataFrame engine is now
  decoupled behind the `DataHandle` seam (ADR 0012): `Value::DataFrame` holds an
  engine-agnostic handle and no `polars::` type escapes `src/backend/polars.rs`. The
  homegrown-Cranelift engine (prototyped in `experiments/dfbench/`, ~140× smaller) is
  therefore a *backend swap behind the trait*, the real route to the size win — not the
  multi-week rewrite the bottom of this doc describes.

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

- **Feature-gate Polars out entirely** (the option this document predates): ADR 0012's
  seam is now finished and audited airtight — no `polars::` type escapes
  `src/backend/polars.rs`, `Value::DataFrame` is a trait object, and only 7 reference
  sites exist outside the backend file. A `dataframes` cargo feature (default-ON; the
  small build is opt-in) sheds the 27 polars crates AND the object_store/tokio tail
  for ~Small surgery. **Landed 2026-08-22** (ADR 0032 steps 1+2): the `appliance`
  profile builds **8.6 MB at the shipped release profile** (12.7 with the JIT in) (fat LTO + strip; vs
  51.8 MB default release), 13.4 MB vs 75.7 MB stripped at the gate profile — with
  the full language surface intact (gate-the-body: every verb still type-checks and
  describes itself; running one names the feature to rebuild with).

Note on `dtype-full` (asked and settled 2026-08-22): trimming it is a REGRESSION, not
a size win — the bridge's total string-fallback becomes a polars schema-layer panic
(-> abort, an ADR 0024 violation) on Decimal/Categorical/Struct parquet columns, the
fixture suite would not catch it (every parquet in the tree is Helix-written), and the
tail enters via `polars-error` regardless of dtype features. It stays.

Until a decision on ADR 0032, the trade-off is accepted deliberately: a large but
genuinely self-contained, fast-starting binary, in exchange for the maturity and
speed of the Polars engine.

# Binary size and the Polars dependency

> **Dateline 2026-09-01 (v0.9.0) — this document is now history, and says so at the top
> because its title no longer describes the shipped binary.** ADR 0033 **Stage 4** landed:
> Helix's own DataFrame engine is the default and polars is gone from the default build. It
> stays behind `--features dataframes` as the correctness **oracle**, which is the reason it
> is kept at all — an engine cannot be its own evidence.
>
> Measured on the gate profile, stripped: **default 19.3 MB**, **appliance 12.5 MB**, and
> the oracle build (`--features dataframes`, which pulls polars back in) **77.5 MB** — the
> last number is the size of what the default no longer carries. Crates compiled fell
> **1,566 → 192**. Startup 4.9 → **2.96 ms** like-for-like (2.5 ms for the appliance
> build — a different binary, so a different row).
>
> Everything below is the record of *why* the polars tail could not be trimmed
> feature-by-feature, and of the reasoning that led to replacing it instead. It is worth
> keeping for that, and it should not be read as a description of the current build.

## Historical: the problem this document was written about

The Helix binary is large (~65 MB release, default features) with ~1000 transitive
crates — including an async runtime (`tokio`) and a cloud object-store
(`object_store` → `reqwest`/`hyper`). This documents *why*, what was investigated, and
why it is not currently fixable without a major change — so the question is not
re-litigated.

## Update (2026-08-24) — the major change happened

The "replace the DataFrame backend" route the bottom of this doc calls a
multi-week rewrite has landed as **ADR 0033 stages 0–3**: a native engine
(`src/backend/native/`, cargo feature `native-df`) behind the same ADR 0012 seam,
covering filter/select/with/sort/group + aggregations/join (inner, left, right,
outer)/unique/vstack/head, CSV read+write, and parquet read+write (zstd). ADR
0032 is **Accepted, steps 1/2/4 implemented**: `dataframes`, `bio`, and `jit`
are cargo gates (the tensor gate deliberately not taken). The **appliance
profile is now `http + mimalloc + native-df`** — a binary with *working* frames
at **~9.3 MB stripped (gate profile)** vs ~76 MB with the full default feature
set. The polars backend remains the **default** engine and the correctness
oracle; ADR 0033 Stage 4 (flipping the default) has not been taken. Everything
below stands as the record of *why* the polars tail cannot be trimmed
feature-by-feature.

> **Superseded 2026-09-01:** Stage 4 was taken in v0.9.0 — see the dateline at the top
> of this file. The 9.3 MB figure above is also stale: the appliance has since gained
> bundled SQLite and the native engine's parquet path, and measures **12.5 MB stripped**
> on the same profile.

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
  describes itself; running one names the feature to rebuild with). The appliance
  has since gained `native-df` — *working* frames at ~9.3 MB gate-stripped; see
  the 2026-08-24 update above.

Note on `dtype-full` (asked and settled 2026-08-22): trimming it is a REGRESSION, not
a size win — the bridge's total string-fallback becomes a polars schema-layer panic
(-> abort, an ADR 0024 violation) on Decimal/Categorical/Struct parquet columns, the
fixture suite would not catch it (every parquet in the tree is Helix-written), and the
tail enters via `polars-error` regardless of dtype features. It stays.

ADR 0032 has since been decided (Accepted; steps 1/2/4 implemented — see the
2026-08-24 update above). For the **default** build the trade-off is still
accepted deliberately: a large but genuinely self-contained, fast-starting
binary, in exchange for the maturity and speed of the Polars engine (which also
serves as the native engine's oracle). The small build is the appliance profile.

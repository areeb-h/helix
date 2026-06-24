# Helix DataFrame benchmarks

**One-line summary:** Helix already scales to multi-million-row analytical
queries by compiling high-level syntax into **Polars lazy DataFrame operations**.
A 50M-row filter→group→mean→sort→head query runs in **~0.20s from Parquet**
(~2.3s from CSV) on the machine below. These are **warm-cache smoke tests with
isolated phases**, not a controlled benchmark — read the caveats.

## Methodology

- **Build:** `cargo build --release` (fat LTO, codegen-units=1, panic=abort),
  Rust 1.96, Polars 0.54.
- **Machine:** AMD Ryzen 7 7700X, 6 cores visible to WSL2, Ubuntu.
- **Timing:** `/usr/bin/time -f "%e %M"` → elapsed wall seconds + peak RSS.
  **Best of 3** runs per case.
- **Phases isolated** (this is the point — the earlier smoke test conflated them):
  - *interpreter startup* — a script that just prints, no data;
  - *count-only* — `read_*(path).count()`;
  - *query-only* — `read_*(path).where(x > 500).group(grp).mean(y).sort(grp).head(5)`.
- **Data:** 5 columns (`id` i64, `grp` i64 ×50, `category` string ×8, `x` i64,
  `y` f64), at 5M / 10M / 50M rows, as CSV and Parquet.
- Reproduce: `scripts/bench.sh` (after generating `/tmp/d{5,10,50}.{csv,parquet}`).

## Results (best of 3, warm cache)

| case | wall | peak RSS |
|---|---:|---:|
| interpreter startup (no data) | 0.00 s | 6 MB |
| **CSV** | | |
| 5M  count() | 0.05 s | 142 MB |
| 5M  where+group+sort+head | 0.33 s | 229 MB |
| 10M count() | 0.10 s | 273 MB |
| 10M where+group+sort+head | 0.61 s | 415 MB |
| 50M count() | 0.50 s | 1360 MB |
| 50M where+group+sort+head | 2.32 s | 1970 MB |
| **Parquet** | | |
| 5M  count() | 0.00 s | 16 MB |
| 5M  where+group+sort+head | 0.04 s | 100 MB |
| 10M count() | 0.00 s | 16 MB |
| 10M where+group+sort+head | 0.06 s | 161 MB |
| 50M count() | 0.00 s | 16 MB |
| 50M where+group+sort+head | 0.20 s | 637 MB |

Conversion (for reference): 50M-row CSV (1.4 GB) → Parquet (57 MB, **~24×**) via
the **streaming sink** at **1.52 GB peak RSS** — down from 4.76 GB on the old
eager path (a 3.1× reduction). Memory isn't fully bounded yet because the CSV
scan side still buffers, but it no longer materializes the whole frame.

## What the numbers actually show

- **Helix adds ~nothing.** Interpreter startup is ~0s / 6 MB; essentially all
  wall time is Polars data work. The "compile syntax → lazy plan → Polars" design
  is not adding meaningful overhead.
- **`count()` on Parquet is O(1).** 0.00s / 16 MB at *every* size — Parquet stores
  the row count in metadata, so no scan happens. CSV `count()` must parse rows, so
  it scales (0.05 → 0.50s).
- **Parquet query is ~10× faster than CSV and ~3× lighter** (50M: 0.20s/637 MB vs
  2.32s/1970 MB) — projection pushdown (only `x`, `grp`, `y` are read) and
  predicate pushdown, both of which Polars derives from the lazy plan.
- **CSV is parse-bound** and scales roughly linearly (~0.046 s per million rows
  for the query).

## Cold vs warm cache (the honest disk story)

Evicted each file from the page cache with `posix_fadvise(DONTNEED)` (no root
needed; `scripts/coldbench.sh`), then timed the 50M query cold, then warm:

| | cold | warm |
|---|---:|---:|
| CSV 50M (1.4 GB) | **26.78 s** | 3.17 s |
| Parquet 50M (57 MB) | **0.52 s** | 0.22 s |

- **Cold CSV is disk-bound** — reading 1.4 GB off disk dominates (8.4× slower than
  warm). This is the real "not billion-safe yet" caveat for CSV.
- **Cold Parquet (0.52 s) beats *warm* CSV (3.17 s)** — the file is 24× smaller, so
  there's far less to read. Parquet is the path that stays fast cold.

## Helix vs raw Python-Polars (50M Parquet, warm, best of 3)

Identical query, `scripts/compare.sh`:

| | wall |
|---|---:|
| Helix — total wall (compiled binary) | 0.20 s |
| Python — total wall (incl. `import polars`) | 0.28 s |
| Python — query-only (`perf_counter`, pure Polars) | 0.125 s |

- The **pure query execution is the same engine** (~0.125 s) — as it must be,
  since Helix calls Polars.
- Helix's **~75 ms overhead** over pure-query is process start + building the lazy
  plan, including one extra schema/metadata read used to resolve column names in
  the predicate. Small, and a candidate to cache.
- Helix is **faster end-to-end than Python** here only because a compiled binary
  pays no `import polars` tax — not because the query is faster.
- Takeaway: Helix delegates efficiently and adds no meaningful query overhead.
  (Python Polars 1.41.2 vs Helix's Rust Polars 0.54 — close enough to compare.)

## Caveats (do not over-read these)

1. **Cold cache now measured** (via `posix_fadvise(DONTNEED)` per-file eviction —
   advisory, not a full `drop_caches`, but no root needed). Cold CSV is ~8×
   slower (disk-bound on 1.4 GB); cold Parquet stays fast. The main matrix above
   is warm-cache; the cold/warm table is separate.
2. **Separate statements = separate executions.** Helix collects at each terminal
   op, so `big.count()` then `print(big.where(...))` are **two passes that each
   re-scan the file** — there is no cross-statement caching/fusion. The benchmark
   isolates count-only vs query-only precisely to avoid conflating them. Fusion
   happens *within* a single chain, not across statements.
3. **`write_parquet` now streams** via Polars' `sink` API — 50M-row write peaked
   at 1.52 GB (was 4.76 GB eager). Not yet *bounded-constant* (the CSV scan side
   buffers), but no longer materializes the whole frame.
4. **Not a controlled benchmark.** Single machine, best-of-3 (no variance/CI).
   Now compared against **raw Python-Polars** (above) — which confirms Helix adds
   no query overhead — but *not* yet against pandas or DuckDB. This measures
   *that Helix delegates efficiently*, not a full cross-tool comparison.
5. `user + sys > real` (from the earlier run) indicates good throughput, but
   `sys` is dominated by I/O/kernel work — it is **not** a clean proof of
   multicore compute. Polars *does* execute multi-threaded, but that claim should
   rest on profiling, not on `time` output alone.

## Honest verdict

The architecture is right: high-level Helix syntax lowers to a Polars lazy plan
that executes columnar and multi-threaded, and the interpreter overhead is
negligible. Multi-million-row analytical queries are already fast. Formal
benchmarks (cold cache, variance, vs-pandas/DuckDB, streaming writes, wider
schemas) are future work — see the roadmap.

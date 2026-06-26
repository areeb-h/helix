# Cross-language benchmarks — Helix vs CPython / NumPy / pandas / scipy

A reproducible suite comparing Helix against the Python scientific stack on the kinds of
work a scientist actually does: tight numeric loops, a filter→map→reduce pipeline, a CSV
group-by, dense linear algebra, summary statistics, and reading a VCF.

```sh
./run.sh                 # full sizes (best of 3, wall-seconds)
./run.sh --scale 0.05    # 5% sizes for a quick check
HELIX=../../target/debug/helix ./run.sh   # force a specific binary
```

`run.sh` builds nothing — it uses `target/release/helix` if present (the honest number),
else `target/debug/helix`. It creates a local `.venv` with `numpy`/`pandas`/`scipy` on first
run. `gen_data.py` writes the data-bound inputs (`data/big.csv`, `data/big.vcf`); the
compute-bound workloads generate their inputs in-process. Both `data/` and `.venv/` are
git-ignored and regenerated.

## Results (best of 3, wall-seconds, lower is better)

Representative release-quality run (10M-element loops, 1M-row CSV, 50k-variant VCF):

| Workload | Helix | CPython | NumPy | pandas/scipy | Helix vs best alt. |
| --- | --- | --- | --- | --- | --- |
| B1  scalar loop Σ(x² mod 1000) 10M | **0.01** | 0.37 | 0.08 | — | 8× vs NumPy, 37× vs CPython |
| B2  filter→map→reduce 10M | **0.01** | 0.28 | 0.10 | — | 10× vs NumPy |
| B3  CSV read + groupby-mean 1M | **0.09** | — | — | 0.36 (pandas) | 4× vs pandas |
| B4  dense matmul 1024³ | 0.06 | — | 0.05 | — | ~tie |
| B5  correlation + linear regression 1M | **0.63** | — | — | 0.69 (scipy) | ~tie (slightly faster) |
| B6  read VCF + mean QUAL 50k | 0.31 | 0.02\* | — | — | \*see caveat |
| B7  matrix inverse 256³ | 0.11 | — | 0.08 | — | ~NumPy (was 1.62s before faer) |

Correctness anchors (must agree across languages): **B1 = 4615000000**, **B2 =
74999985000000**, **B4 = 1073741824.0** (= 1024³), **B6 mean QUAL ≈ 59.95** at full scale.

### Why Helix wins where it wins

- **B1/B2 (8–10× over NumPy):** Helix fuses the whole `map`/`filter`/`reduce` chain into one
  native loop with zero intermediate allocation. NumPy materializes a fresh array at every
  step (`arange`, `x²`, `% 1000`); Helix doesn't. (Run with `HELIX_NOJIT=1` to see the
  bytecode VM without fusion — much slower — which is what the speedup buys.)
- **B3 (4× over pandas):** the same lazy, multi-threaded Polars engine, with no Python
  per-call overhead.
- **B4/B5 (~tie):** here the heavy lifting is a library kernel — `matrixmultiply` (pure-Rust
  SIMD GEMM) for matmul, and Helix's stats for B5 — and they're as good as OpenBLAS / scipy.
- **B7 (~NumPy):** matrix inverse used to be the one weak spot (1.62s, hand-rolled Gaussian).
  It now uses **faer** (pure-Rust SIMD blocked LU) and lands near NumPy's LAPACK — without a
  C dependency.

## Honest caveats

- **Debug vs release.** If `run.sh` falls back to the debug binary, the *interpreter* overhead
  is higher, but the measured hot paths are already release-quality: numeric loops run
  JIT-native, and Polars / noodles / faer / matrixmultiply are dependencies compiled at opt-3
  (`[profile.dev.package."*"]`). Build `--release` for the cleanest end-to-end numbers.
- **Python includes import.** The Python times include interpreter start + library import, as
  a user pays them. Even discounting import, Helix wins B1/B2 on compute alone (fusion).
- **B6 is apples-to-oranges.** Helix's `read_vcf` does a *full spec-compliant parse* (typed
  CHROM/POS/REF/ALT/QUAL/FILTER + INFO via `noodles`); the Python script just
  `split("\t")[5]` on one column with no typing or validation. The numbers are not measuring
  the same work — B6 is here for completeness, not as a fair race.
- **R is not included** (unavailable in the dev environment); add an `Rscript` column if you
  have R.
- Single-machine, warm-cache, wall-clock best-of-3. Not a controlled microbenchmark harness —
  enough to ground "is Helix competitive on real work?" (yes), not to split hairs at the ms.

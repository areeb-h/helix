# Homegrown columnar engine vs Polars — measured

> **Outcome, 2026-09-01: the experiment was taken, and it shipped.** This time-boxed
> comparison decided in favour of a homegrown engine; it became
> [ADR 0033](../../docs/adr/0033-native-dataframe-engine.md), and **Stage 4 landed in
> v0.9.0** — the native engine is the default and polars is retained only as the oracle
> every result is compared against. Stripped, gate profile: default **19.3 MB** against
> **77.5 MB** for the build that still carries polars. The numbers below are the original
> experiment and are kept as the record of how the decision was reached.


A time-boxed experiment to decide whether replacing Polars with a homegrown in-memory
columnar engine is viable for Helix (motivated by the verified ~65 MB binary that the
Polars dependency forces — see `docs/binary-size.md`). Run with `cargo run --release
--bin bench`. Hardware/runs are local and indicative, not a formal benchmark.

## Performance (best-of-5, ms; lower is better)

| rows | op | homegrown | polars | homegrown ÷ polars |
|---|---|---|---|---|
| 100K | filter | 0.78 | 0.23 | 3.35× (slower) |
| 100K | group_sum | 0.04 | 0.94 | 0.04× (**25× faster**) |
| 100K | sort | 1.43 | 2.11 | 0.68× (faster) |
| 1M | filter | 3.78 | 0.94 | 4.02× (slower) |
| 1M | group_sum | 0.15 | 1.66 | 0.09× (**11× faster**) |
| 1M | sort | 14.0 | 16.3 | 0.86× (faster) |
| 5M | filter | 16.3 | 5.67 | 2.87× (slower) |
| 5M | group_sum | 1.28 | 5.43 | 0.24× (**4× faster**) |
| 5M | sort | 117 | 162 | 0.72× (faster) |

## Binary size & dependencies (release, stripped)

| | binary | crates |
|---|---|---|
| with Polars | **60 MB** | 1066 |
| homegrown only (rayon) | **436 KB** | 8 |

→ ~**140× smaller**, and the entire async/cloud tail (tokio, object_store, reqwest)
disappears.

## Honest reading

- **Homegrown is competitive, not categorically slower** — it wins on `sort` (rayon
  par-sort ≈ Polars) and loses on `filter` (Polars's SIMD predicate evaluation is its
  real strength; ~3–4× here, and the gap is the most improvable).
- **The `group_sum` win is partly structural, not free**: the homegrown version exploits
  dense small-cardinality integer keys (array accumulation), while Polars hashes
  arbitrary keys. For the common bio case (group by a categorical with modest
  cardinality) this advantage is real; for high-cardinality or string keys a homegrown
  group-by would need hashing and land closer to Polars. Do not read 25× as general.
- **The size win is the headline and is unambiguous** — the binary is ~140× smaller and
  the cloud/async tail is gone, directly serving Helix's "small, self-contained,
  local-first" identity.

## What this does and does NOT prove

Proves: the **core in-memory compute** (filter / aggregate / sort) of a homegrown engine
is feasible and performance-competitive, at a fraction of the size.

Does NOT prove a full replacement is cheap. A production engine still needs: the general
column-type matrix, nulls/`missing` throughout, **a Parquet reader (a hard format — the
single biggest cost)**, hash joins, string ops, predicate/projection pushdown, and the
long tail of correctness Polars has earned. That is a multi-month build.

## Recommendation

The result **de-risks** the homegrown direction more than conventional wisdom ("you
won't beat Polars") suggests, and the size payoff is enormous and on-brand. The
pragmatic path is **phased migration behind the existing DataFrame interface**: build the
homegrown engine incrementally (compute first — proven here — then CSV, then joins, then
Parquet), keep Polars as the fallback/oracle until the homegrown path reaches parity, and
flip the default only when it does. No big-bang rewrite; perf and correctness validated
at each step.

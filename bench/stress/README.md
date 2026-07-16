# Cross-language stress suite

Five workloads (plus an interop demo) that push Helix's **newest** capabilities and
pit each against the same task written in **pure CPython, NumPy/pandas, Rust, and Go**.

This is the harder sibling of `../crosslang/`: it leans on the feature set added most
recently — native `kmer_counts`, the FASTA/FASTQ/VCF readers, the `@column` dataframe
sigil, pattern matching (or-patterns + guards), and helper-fn pipeline fusion — and it
adds **Rust and Go** as compiled-language baselines.

```
bash verify.sh            # correctness gate: every language prints identical anchors
bash run.sh               # timed, full sizes
bash run.sh --scale 0.1   # 10% sizes for a quick look
```

`run.sh` prefers `target/release/helix` if present, else falls back to the debug binary.
Helix's measured hot paths are release-quality even in a debug build (the JIT emits native
code and the heavy deps — Polars, noodles, faer — are compiled opt-3 regardless), but build
`--release` for the cleanest numbers.

## Workloads

| ID | Task | New feature it stresses | Anchor |
|----|------|-------------------------|--------|
| **S1** | k-mer spectrum, k=10, over a 10 Mbp genome | `read_fasta` + native 2-bit `kmer_counts` | `distinct / total / max` |
| **S2** | per-read GC + Phred quality over 200k reads | `read_fastq`, `Dna.gc_content()`, `qual.phred()` | `reads / bases / gc4 / q4 / hiq` |
| **S3** | VCF: filter QUAL>50, count per gene, 200k variants | `read_vcf` + `@column` predicates | `total / pass / per-gene` |
| **S4** | classify `n%12` and sum weights over 20M | `match` with or-patterns + guards | one integer |
| **S5** | polynomial → keep evens → sum over 50M | JIT pipeline fusion (helper fn inlined) | one integer |
| **S7** | local-align 100 reads vs a gene | native `seq.align(target, "local")` (Gotoh affine DP) | `total / hits / max` |
| **S8** | canonical k-mer spectrum, k=10, 10 Mbp | strand-agnostic `canonical_kmer_counts` | `distinct / total / max` |
| **S6** | call CPython in-process on Helix data | `python.import`, `to_tensor`/`to_dataframe` bridges | capability demo |

Every workload prints **identical anchor lines in every language** — `verify.sh` is the gate.
A benchmark that doesn't compute the same answer everywhere is measuring nothing.

## Fairness notes (read before quoting numbers)

- **Each language uses its natural good idiom.** S1 Rust/Go hand-roll the fast 2-bit rolling
  hash into a hash map (their best case); CPython uses an idiomatic dict over slices; Helix
  calls the one built-in `kmer_counts(10)`. The point is *what ordinary good code in each
  language costs you* — Helix hands you the fast path as a method.
- **Rust and Go are compiled once, then the built binary is timed** (no compile time in the
  measurement). Python timings include interpreter startup + library import, as a user pays.
- **S3 is not apples-to-apples on parsing:** Helix does a full spec-aware VCF parse (typed
  columns, every INFO key lifted to a column); the Rust/Go/CPython baselines do a targeted
  split of just the fields this query needs. pandas parses into a DataFrame like Helix does.
- **S4/S5 are pure compute** (no I/O); they isolate match-dispatch and fused-loop codegen.
  `helix-nojit` (the bytecode VM, `HELIX_NOJIT=1`) is shown alongside the default JIT engine
  so the fusion speedup is visible.

## Results

Best of 3, wall-seconds, **release** build, WSL2 on an AMD Ryzen 7700X. Numbers are
machine-specific — re-run locally. Correctness was verified across all five languages with
`verify.sh` first (a benchmark that doesn't agree on the answer is measuring nothing).

Helix built with the default **mimalloc** allocator (ADR 0016). The "(pre-mi)" column is the
same release built with the system allocator, to isolate the allocator's effect.

| Workload | helix | (pre-mi) | helix-nojit | cpython | numpy/pandas | rust | go |
|----------|------:|---------:|------------:|--------:|-------------:|-----:|---:|
| S1  k-mers k=10 (10 Mbp)        | **0.40** | 0.66 | — | 4.26 | — | 0.52 | 0.44 |
| S2  FASTQ GC + Phred (200k)     | **0.32** | 0.46 | — | 1.42 | — | 0.06 | 0.18 |
| S3  VCF filter+group (200k)     | **0.22** | 0.39 | — | 0.11 | 0.37 (pandas) | 0.02 | 0.03 |
| S4  pattern match (20M)         | 4.43 | 4.84 | 4.45 | 1.10 | 0.17 (numpy) | 0.01 | 0.01 |
| S5  fused pipeline (50M)        | **0.14** | 0.15 | 9.71 | 5.92 | 0.52 (numpy) | 0.08 | 0.09 |
| S7  local alignment (100 reads) | **0.01** | 0.01 | — | 0.29 | — | 0.00 | 0.00 |
| S8  canonical k-mers (10 Mbp)   | **0.31** | 0.51 | — | 6.37 | — | 0.35 | 0.33 |

**mimalloc's effect is textbook-clean:** 1.4–1.8× on the allocation/hashmap-heavy paths
(S1 1.65×, S3 1.77×, S8 1.65×, S2 1.44×) and **zero** on the zero-allocation fused pipeline
(S5) and the tiny alignment (S7) — the allocator helps exactly where allocation happens.
The decisive consequence: **with mimalloc, Helix's k-mer kernels now beat hand-written Rust
and Go** (S1 0.40 vs 0.52/0.44; S8 0.31 vs 0.35/0.33) — the native 2-bit kernel on a fast
allocator edges out an idiomatic `HashMap` on the system allocator. S3 now clearly beats
pandas (0.22 vs 0.37).

### What the stress test found

- **S5 — fusion is the headline win.** The map→filter→reduce chain (with a user `poly`
  function called *inside* the loop) JIT-compiles to one native, zero-allocation loop:
  **68× over Helix's own VM**, **40× over CPython**, **6× over NumPy**, and within **1.7×
  of hand-written Rust/Go**. A dynamically-typed scientific language landing at ~60% of a
  compiled tight loop is the whole thesis.
- **S1 — the bio flagship holds up.** `kmer_counts(10)` as a single method beats CPython
  **6×** and lands within **~1.3–1.6×** of hand-rolled 2-bit Rust/Go — code a Helix user
  never has to write.
- **S2 — competitive, not dominant.** Beats CPython 3×; the compiled languages are 2–7×
  faster (reader + per-read method dispatch overhead). Honest mid-table.
- **S3 — neck-and-neck with pandas** (0.40 vs 0.37) — the right comparison, both being full
  DataFrame engines. The hand-split baselines win because they parse only the two fields the
  query needs; Helix/pandas do a full typed parse.
- **S7 — alignment is a native-code home run.** `seq.align(target, "local")` is a hand-rolled
  Gotoh DP in Rust, so it ties compiled Rust/Go (sub-10 ms) and beats CPython **~28×** —
  "samtools speed without the segfaults," from one method call.
- **S8 — canonical k-mers track S1.** Strand-collapsed counting beats CPython **12×** and lands
  within **~1.5–1.7×** of hand-rolled 2-bit Rust/Go, while correctly merging strand pairs
  (166k canonical vs 182k forward k-mers).
- **S4 — the real weakness this suite surfaced.** `match` inside a per-element `map` runs on
  the tree-walking/VM path (it is **not** JIT-eligible — note `helix` ≈ `helix-nojit`), so it
  is **~4× slower than CPython** and far behind compiled `switch`. Concrete optimization
  target: make match-arm dispatch JIT-eligible, or specialize the common literal/or-pattern
  arms. Until then, pattern matching is for clarity on cool paths, not megaloop hot paths.

Verified anchors (every language identical), at `--scale 0.02`:

```
S1  distinct=182128 total=199991 max=5
S2  reads=4000 bases=600000 gc4=4996 q4=182667 hiq=0
S3  total=4000 pass=1979 BRCA1=472 BRCA2=513 EGFR=250 KRAS=255 TP53=489
S4  51666663
S5  12442104450
S7  total=88 hits=2 max=45
S8  distinct=166461 total=199991 max=5
```

## Files

- `gen.py` — deterministic input generator (LCG-seeded; identical bytes every run)
- `s1_kmers.*` … `s5_fused.*` — one file per language per workload
- `s6_interop.helix` — CPython interop demo (needs `cargo run --features python`)
- `verify.sh` — correctness gate · `run.sh` — timed runner

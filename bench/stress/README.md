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
| **S6** | call CPython in-process on Helix data | `python.import`, `to_py`/`to_tensor` bridges | capability demo |

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

Run `bash run.sh` to populate. (Numbers are machine-specific; the repo ships the code, not a
results snapshot — re-run locally.) Correctness was verified across all five languages with
`verify.sh`; representative anchors at `--scale 0.02`:

```
S1  distinct=182128 total=199991 max=5
S2  reads=4000 bases=600000 gc4=4996 q4=182667 hiq=0
S3  total=4000 pass=1979 BRCA1=472 BRCA2=513 EGFR=250 KRAS=255 TP53=489
S4  51666663
S5  12442104450
```

## Files

- `gen.py` — deterministic input generator (LCG-seeded; identical bytes every run)
- `s1_kmers.*` … `s5_fused.*` — one file per language per workload
- `s6_interop.helix` — CPython interop demo (needs `cargo run --features python`)
- `verify.sh` — correctness gate · `run.sh` — timed runner

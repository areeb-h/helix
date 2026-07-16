# Kernel benchmark results — 2026-07-17

Regenerate with `./run.sh` from this directory. Every number below was produced
by that script on a quiet machine, after the anchor gate confirmed that all
languages of a kernel print byte-identical output.

**Machine**: AMD Ryzen 7 7700X (6 cores visible to WSL2), Ubuntu 24.04 on WSL2.
**Toolchains**: gcc 13.3.0 (`-O3 -march=native`, plus `-ffp-contract=off` /
`-fwrapv` where a kernel needs them), rustc 1.97.0 (`-O -C target-cpu=native`),
go 1.26.1 (plain `go build`, i.e. GOAMD64=v1 — see caveats), CPython 3.12.3,
NumPy 2.4.3, Helix `target/release` (opt-level 3, fat LTO).
**Page-size equalization**: `GLIBC_TUNABLES=glibc.malloc.hugetlb=1` is exported
for the C/Rust references (see "The huge-page correction" below).

## Scorecard vs single-threaded C

Wall-clock seconds, best of 3 (once, for entries over 5s), lower is better.
`%CPU` is reported because Helix parallelizes some kernels and the references
never do — a wall-clock win bought with 2.5× the cores is not a codegen win.

| # | Kernel | Helix | C | Rust | Go | CPython | NumPy | Helix vs C |
|---|--------|-------|---|------|-----|---------|-------|------------|
| k1 | dot 50M i64 | 0.16s (254%) | **0.10s** | 0.12s | 0.43s | 8.86s | 0.27s | **1.6× slower** (≈3.8× per-core) |
| k2 | mandelbrot 1200² | 0.41s | **0.07s** | 0.07s | 0.07s | 6.01s | — | **5.9× slower** |
| k3 | basel 1e8 | 0.08s | **0.05s** | 0.08s | 0.08s | 8.40s | 0.44s | **1.6× slower** |
| k4 | allpairs 15k | 0.04s | **<0.01s** | <0.01s | 0.05s | 4.36s | 0.11s | **slower** (timer-grain; ~7× per the audit) |
| k5 | montecarlo 1e8 | 0.54s | **0.26s** | 0.22s | 0.27s | 39.09s | — | **2.1× slower** |
| k6 | sieve π(10⁷) | **0.02s** | 0.01s | 0.01s | 0.02s | 0.67s | 0.07s | ~tie (**delegation** — beats NumPy 3.5×) |
| k7 | wordcount 5M | 1.60s | **0.20s** | 0.24s | 0.21s | 1.44s | 1.83s | **8× slower** (also loses to CPython) |
| k8 | matmul 1024³ (GEMM) | 0.32s | — | — | — | — | **0.06s** | **5.3× slower than NumPy** |
| k9 | matmul 512³ (naive) | 25.86s | **0.36s** | 0.35s | 0.32s | 14.79s | — | **72× slower** (also loses to CPython) |

**Helix loses to C on every kernel in this suite.** The one place it stands out
is k6, where it wins by *not doing the work* — calling a native `primes()`
builtin, the same way NumPy wins by calling BLAS. The honest counterpart is
`k6_sieve_trial.helix` (pure-Helix trial division): **83.00s**, i.e. ~8300×
slower than the builtin and ~5500× slower than C's sieve. That gap is the
kernel's whole point.

## The huge-page correction (why these numbers differ from earlier claims)

Earlier internal runs reported Helix at **parity with or beating C** on the dot
products. That was an artifact, and the fix is instructive:

Helix links **mimalloc**, which `madvise(MADV_HUGEPAGE)`s its arenas. The
system's THP policy is `madvise`-only, so glibc `malloc` — used by the C and
Rust references — never got huge pages. Measured: Helix `AnonHugePages`
1,142,784 kB vs C 0 kB; **minor page faults: Helix 1,779 vs C 195,388** (110×).
Because ~86% of this kernel's "build" phase is page-fault + page-zeroing (a cold
first touch runs at 2.1 GB/s; the byte-identical warm rewrite at 15.8 GB/s), the
benchmark was mostly ranking *allocators*.

One environment variable, no source change:

| | stock | `GLIBC_TUNABLES=glibc.malloc.hugetlb=1` |
|---|---|---|
| C | 0.43s (195,388 faults) | **0.10–0.12s** (1,296 faults) |
| Rust | 0.46s | **0.13s** |
| Helix | 0.16s @ ~254–330% CPU | 0.16s (already had huge pages) |

Helix's own number never moved. **C's improved 4.4×, and the "win" evaporated.**
`run.sh` now exports the tunable so the comparison is page-size-fair. Go has no
equivalent knob and is *not* equalized — its k1 number (0.43s) is stock, and we
say so rather than dropping the row.

## What each kernel actually measures

- **k1 dot** — allocator/page-fault behavior and array construction. The dot
  product itself is ~6% of the runtime (reduce ≈0.027s of a ~0.44s stock run).
  Helix parallelizes exactly the 94% (the array-building maps, via rayon) and
  not the 6% (the reduce is serial by design, to keep float results
  deterministic). Not a codegen benchmark.
- **k2 mandelbrot** — scalar FP codegen and branch prediction. Single-threaded
  everywhere (Helix 107% CPU). gcc contracts one `2*zr*zi+ci` into an FMA at
  `-march=native`; we build with `-ffp-contract=off` for rounding parity. Note
  the escape test lands **exactly 1 ulp** from the boundary at two pixels
  (x=80, y=320/880), so cross-build agreement is real but has zero headroom —
  `-ffast-math` does change the anchor.
- **k3 basel** — serial FP-add latency. On this box ~0.056s of *every* compiled
  entry is the shared dependent-`vaddsd` latency floor (3 cycles/add), i.e.
  98.8% of C's 0.057s and ~64% of Helix's. This kernel **under-discriminates
  codegen**: the 1.6× spread understates the real gap.
- **k4 allpairs** — codegen + auto-vectorization on an L2-resident array (120 kB).
  Single-threaded on every engine: Helix's parallel nested-reduce path declines
  when the inner range mentions the outer binder (`range(i+1, n)`), so the
  triangular loop can never take it. For points on a *line* this O(N²) shape has
  an O(N log N) closed form — it is a microbenchmark, not a real distance-matrix
  workload.
- **k5 montecarlo** — scalar integer/FP codegen over a bit-exact xorshift64
  stream shared by all five languages (Helix masks after its arithmetic `>>` to
  reproduce C's unsigned shift). Anchor is the raw hit count — no float
  formatting to argue about.
- **k6 sieve** — **delegation**: does the language ship the right primitive?
- **k7 wordcount** — string building + hashing. Helix (632 MB) and NumPy (913 MB)
  materialize the whole corpus; C (2.1 MB), Rust (3.0 MB), Go (7.7 MB) and
  CPython (11.0 MB) stream one word at a time. For Helix this is *also* an
  allocator benchmark, unlike for the streaming four.
- **k8 matmul (GEMM)** — **delegation**: Helix's tensor path (faer) vs NumPy's
  OpenBLAS. Isolated GEMM at n=2048: faer ~0.11s vs OpenBLAS ~0.061s — OpenBLAS
  is ~1.8× faster. At n=512 the GEMM is ~1% of the wall time and you are timing
  interpreter startup, which is why the default is 1024.
- **k9 matmul (naive)** — the honest triple-loop peer group. Helix's
  comprehension version never reaches the JIT (verified: identical time with
  `HELIX_NOJIT=1`) and allocates an n-element temporary per (i,j) pair.

## Known reporting caveats

1. **Go is not page-size-equalized** (k1) and builds at GOAMD64=v1 while C/Rust
   get `-march=native` / `target-cpu=native`. Measured cost of v1-vs-v3 on k2:
   nil. On k1 it is real and unquantified.
2. **k4's C/Rust times are at timer grain** (`<0.01s`). The audit's finer
   measurement puts Helix ≈6.9× behind C there.
3. **CPython's k9 number beats Helix's** (14.79s vs 25.86s). Reported, not hidden.
4. `-march=native` is a ~9% *pessimization* for gcc on k2 (79ms vs 72ms without).
   We keep it as the conventional "give C every advantage" flag; it slightly
   understates C.

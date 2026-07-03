# Helix JIT numeric-kernel benchmarks (vs C / Rust / Go / Python)

**Summary:** Helix compiles pure-numeric `map`/`filter`/`reduce` kernels — including
**array-indexed reductions** (`a[j]`) and **nested reductions** (`map` of `reduce`) — to
native machine code via Cranelift. On memory-bandwidth-bound kernels it is **~C-class**, beats
NumPy, and runs ~20–700× over its own bytecode interpreter — every result **bit-identical** across
all languages (and enforced bit-identical across Helix's own three engines by a differential oracle;
see [execution-engine.md](execution-engine.md)). On **compute-bound SIMD-friendly** kernels it
*loses* to properly-vectorized (`-march=native`) C, because Cranelift emits scalar code — see the
§5 correction. The tables in §1–4 below use each kernel's original flags (some scalar `C -O3`); §5
is the honest reckoning against uniform `-march=native` C.

For the **full honest picture — a ten-kernel run that includes the workloads Helix *loses* at
(mandelbrot, wordcount, montecarlo, sieve)** and the per-core (single-thread) numbers, jump to
[§5 Full-spectrum run](#5-full-spectrum-run-2026-07-03-where-helix-wins-ties-and-loses).

| kernel | size | Helix (JIT) | C `-O3` (1 thread) | best other | notes |
|---|---|---|---|---|---|
| **i64 dot product** | 50M | ~0.34 s | 0.47 s | Go 0.46 s | memory-bandwidth-bound; ~C-parity |
| **f64 dot product** | 50M | 0.36 s | 0.47 s | Go 0.48 s | beats 1-thread C (auto-parallel build) |
| **nested all-pairs** `Σ(i^j)` | 900M | 0.06 s | 0.08 s | C-OpenMP 0.01 s | beats vectorized 1-thread C; only SIMD+threads is faster |
| **all-pairs distance** `Σ|codes[i]−codes[j]|` | 225M | 0.03 s | 0.04 s (SIMD) | Go 0.09 s | array-indexed; parallel outer edges 1-thread SIMD C (§4) |

These are **best-of-3 warm-cache wall-clock measurements on one WSL2 machine**, not a
controlled benchmark — WSL scheduling gives ~±30% run-to-run variance, so read the *rankings*,
not the third digit. The caveats section is load-bearing; in particular Helix wins some cases by
**auto-parallelizing across cores** while the C/Rust/Go baselines are single-threaded loops.

## Methodology

- **Machine:** AMD Ryzen 7 7700X, 6 cores visible to WSL2, Ubuntu 24.04.
- **Toolchains:** gcc 13.3 (`-O3`, `-fopenmp` for the threaded C), rustc 1.96 (`-O`),
  go 1.26, CPython 3.12 + NumPy (venv). Helix = the `gate` profile binary (opt-level 3, no
  LTO — the JIT emits its own optimized native code, so this matches release for these kernels).
- **Timing:** `/usr/bin/time -f '%e %U %P'` → wall seconds, user-CPU seconds, CPU%. **Best of
  3** for the fast tier; interpreted baselines (CPython, Helix `HELIX_NOJIT=1`) best of 1.
- **Correctness anchor:** every language prints the **identical** integer/float result on every
  run; a mismatch fails the benchmark. Values chosen to stay within `i64` / exact-`f64` range so
  summation order is irrelevant (NumPy's pairwise/SIMD sum matches the naive left-to-right fold).
- **Two pitfalls that had to be defeated for a *fair* comparison:**
  1. **Constant-folding.** `(i−j)²` summed over a range is a polynomial; LLVM/GCC scalar-evolution
     folds the *entire* nested loop to a compile-time constant (Rust and C-OpenMP reported 0.00 s
     — they never ran the loop). The nested benchmark therefore uses `i ^ j` (XOR), which has no
     closed form, so every compiler does the real work.
  2. **Startup noise.** At small sizes the compute finishes in a few ms and the numbers are just
     process-startup + measurement grain. Each kernel is sized so compute dominates.
- **Parallelism disclosure.** Helix auto-parallelizes across cores (rayon) above a size
  threshold: the JIT `map` kernel (array construction) and the `#31` nested-reduce outer loop.
  The C/Rust/Go programs here are **single-threaded** unless labelled OpenMP. Where Helix beats
  single-threaded C it is because of this parallelism, not lower precision — the results are
  bit-identical. A same-parallelism reference (C-OpenMP) is included for the nested kernel.

## 1. Integer dot product (v1a — `arr[j]` reduce)

`S = Σ_{j<N} (j%97)·(j%89)`, N = 50,000,000. Two packed `i64` arrays, a multiply-add fold over
indexed loads — the exact shape the JIT reduce kernel compiles.

```helix
a = (range(0, 50000000)).map(j => j % 97)
b = (range(0, 50000000)).map(j => j % 89)
print((range(0, 50000000)).reduce(0, (acc, j) => acc + a[j] * b[j]))
```

| language | wall (best of 3) | vs C |
|---|---:|---:|
| Helix (JIT) | ~0.34 s | 0.7–1.0× |
| C `-O3` | 0.47 s | 1.00× |
| Rust `-O` | 0.47 s | 1.00× |
| Go | 0.46 s | 0.98× |
| NumPy (`np.dot`) | 0.76–0.94 s | ~1.8× |
| CPython (`array` + loop) | ~10 s | ~21× |
| Helix (no-JIT, bytecode VM) | ~11 s | ~24× |

The **reduce itself is memory-bandwidth-bound** — streaming two 400 MB arrays saturates DDR5, so
every compiled language converges at the RAM ceiling and Helix's native reduce reaches it (user-CPU
for the reduce is ~0.05 s, essentially free). Helix's *total* dips below single-threaded C because
its array construction (`range.map`) parallelizes across cores. ~24× over its own bytecode VM.

## 2. Float dot product (v1b + mixed-map build)

Same shape over packed `f64` arrays, `0.0` init. Building the float arrays uses the mixed
(`Int`→`f64`) map kernel `(j%97)*1.0`, which runs as a single parallel pass.

```helix
a = (range(0, 50000000)).map(j => (j % 97) * 1.0)
b = (range(0, 50000000)).map(j => (j % 89) * 1.0)
print((range(0, 50000000)).reduce(0.0, (acc, j) => acc + a[j] * b[j]))
```

| language | wall (best of 3) | vs C |
|---|---:|---:|
| **Helix (JIT)** | **0.36 s** | **0.77×** |
| C `-O3` | 0.47 s | 1.00× |
| Rust `-O` | 0.47 s | 1.00× |
| Go | 0.48 s | 1.02× |
| NumPy (`np.dot`) | 0.76–1.5 s | ~2–3× |
| CPython | ~9 s | ~20× |
| Helix (no-JIT) | ~10 s | ~22× |

The `f64` fold is naive left-to-right (`fmul`/`fadd` in source order), so it is **bit-exact** to the
interpreter and to NumPy. The reduce is memory-bound at C-parity; Helix edges ahead on the total by
auto-parallelizing the float-array construction. (Before the mixed-map build fix, the idiomatic
builder `(range).map(j => (j%97)*1.0)` fell to the per-element interpreter at ~3.5 s/array — see the
commit history; it now JITs as one parallel pass at ~0.1 s/array.)

## 3. Nested all-pairs reduction (#31 — parallel `map` of `reduce`)

`S = Σ_{i<N} Σ_{j<N} (i ^ j)`, N = 30,000 (900M pairs) — the O(N²) distance-matrix / N-body /
all-pairs shape. The inner reduce captures the outer index `i`; Helix runs the outer loop across
cores over the native inner kernel.

```helix
print((range(0, 30000)).map(i =>
  (range(0, 30000)).reduce(0, (acc, j) => acc + (i ^ j))
).sum())
```

| language | wall (best of 3) | CPU% | notes |
|---|---:|---:|---|
| C (OpenMP, 6c + SIMD) | 0.01 s | ~526% | vectorized **and** threaded |
| **Helix (JIT, auto-parallel)** | **0.06 s** | **~569%** | threaded, no SIMD |
| Rust `-O` (1 thread, SIMD) | 0.07 s | 100% | auto-vectorized |
| C `-O3` (1 thread, SIMD) | 0.08 s | 100% | auto-vectorized |
| Go (1 thread) | 0.17 s | 100% | scalar |
| NumPy (row-vectorized) | 0.25 s | 100% | outer Python loop, inner vectorized |
| CPython / Helix no-JIT | ~12 s @ 225M | 100% | interpreted, ~600× slower |

Helix's auto-parallel nested reduce (0.06 s, ~5.7 cores) **edges past even SIMD-vectorized
single-threaded C (0.08 s) and Rust (0.07 s)** — its parallelism compensates for the JIT not
auto-vectorizing. It beats scalar Go ~3× and NumPy ~4×. The **only** faster result is C-OpenMP,
which combines *both* SIMD and threads; Helix has the threads but not the SIMD.

## 4. All-pairs distance matrix (array-indexed nested reduce)

`S = Σ_{i,j<N} |codes[i] − codes[j]|`, `codes[i] = (i·C) % M`, N = 15000 (225M pairs) — the
**distance-matrix / all-pairs-similarity** shape (phylogenetics, clustering, sequence
comparison). Unlike the pure-arithmetic nested reduce above, the inner reduce **indexes an
array by both the outer index `i` and the inner counter `j`** (`codes[i]` and `codes[j]`), so
the inner kernel reads captured memory at a scalar index — the capability that makes it run
native. As of the parallel-outer landing, the **outer map is also parallelized**: the captured
array bases are shared read-only across rayon workers, and the per-`i` bounds obligation is
hoisted and checked ONCE before the parallel region (`codes[j]` needs `[0,N) ⊆ [0,len)`,
`codes[i]` needs the whole outer `[0,N) ⊆ [0,len)`), so each worker does unchecked native loads.

```helix
codes = (range(0, 15000)).map(i => (i * 2654435761) % 1000003)
D = (range(0, 15000)).map(i => (range(0, 15000)).reduce(0, (acc, j) => acc + abs(codes[i] - codes[j])))
print(D.sum())
```

Measured at N=15000 (225M pairs), where the times actually resolve — at N=6000 everything
finishes in a few milliseconds, below the 0.01 s timer grain (all print the same anchor
`75002627576474`):

| language | wall (best of 3) | vs C |
|---|---|---|
| **Helix (JIT, parallel outer)** | **0.03 s** | **0.75×** |
| **C `-O3` (SIMD, 1 thread)** | 0.04 s | 1.0× |
| Go (1 thread) | 0.09 s | 2.3× |
| Helix (JIT, serial outer — prior) | 0.14 s | 3.5× |
| Helix (no-JIT, VM) | ~28 s | ~700× |

**Read this honestly** — and see the [§5 correction](#5-full-spectrum-run-2026-07-03-where-helix-wins-ties-and-loses):
the "SIMD C" in the table above is `-O3` (baseline SSE2). At `-O3 -march=native` gcc auto-vectorizes
this to **AVX2** and runs it in **~0.010 s**, so Helix does **not** actually beat properly-vectorized
C here — it *loses* to it (0.03 s vs 0.010 s), exactly the SIMD-in-the-JIT gap named below. Against
the SSE2 single-thread baseline shown, Helix's parallel outer loop pulls ahead, but by **using all
cores** (~450% CPU), not by winning per-core. This is a compute-bound kernel: the array (`codes`,
~120 KB) is L2-resident, so it is limited by arithmetic throughput, and there C still has a per-core
edge Helix lacks — gcc/LLVM **auto-vectorize** the inner loop (AVX2 does ~4 `|codes[i]-codes[j]|`
per instruction) while Helix's Cranelift JIT emits a **scalar** loop. A fair fight *on equal
hardware* (a C `+OpenMP` version = SIMD × threads) would still beat Helix. What changed here is the
outer loop went from serial to parallel (0.14 s → 0.03 s, ~4.7×), which is enough to pass
single-threaded C on this shape — the same kind of "auto-parallel across cores vs a single-threaded
baseline" win as the dot products (§1–2) and the pure-arithmetic nested reduce (§3).

The remaining per-core gap to C is the one open lever: **SIMD in the JIT** (Cranelift emits scalar
code; an i64 reduce is safe to vectorize because integer add is associative — bit-identical to the
scalar fold — so it would not break the differential oracle). (An earlier draft of this doc reported
C at "0.00 s, constant-folded" — that was wrong: feeding N from `argv` so the compiler cannot
precompute still shows C finishing the real loop in ~2 ms at N=6000. It was too fast to measure, not
folded.)

## 5. Full-spectrum run (2026-07-03): where Helix wins, ties, **and loses**

Sections 1–4 above are the numeric-kernel fast paths — Helix's home turf. This section is the
opposite discipline: a **ten-kernel shootout that deliberately includes the workloads Helix is
bad at**, so the wins are not read out of context. Ten kernels × six languages (C, Rust, Go,
CPython, NumPy, Helix), same machine, best-of-3, every language verified to print the **identical
anchor** before timing. An independent read-only audit confirmed no kernel hardcodes, skips work,
or gets constant-folded (timings scale with N). A visual version of this table lives at the
session artifact (cross-language benchmark report).

Seconds, best-of-3 (lower is better). **∥** = Helix across all 6 cores; **·1** = Helix pinned to
one core (`RAYON_NUM_THREADS=1`) — the honest per-core number.

> **Correction (2026-07-03) — the C baseline is now uniform `-O3 -march=native`** (with
> `-ffp-contract=off` on mandelbrot to stay anchor-matching), the *fair, strong* baseline. The
> first version of this table used each kernel's manifest flags, several of which were plain `-O2`
> (scalar) — which scored the compute-bound kernels **too generously**. Re-measured against
> properly-vectorized C, **`allpairs` flips from a win to a loss and `basel` from a tie to a loss**:
> gcc AVX2-vectorizes those inner loops while Helix's Cranelift JIT emits scalar code (the standing
> "SIMD in the JIT" gap). Rust/Go stay at `-O`/default, so a `target-cpu=native` Rust would also
> gain on the SIMD-friendly kernels — those Helix-vs-Rust margins are soft too. Separately,
> `montecarlo` and `sieve` now **complete** (were `>260 s`) thanks to this session's lazy-`enumerate`
> and `isqrt`+short-circuit work. The two memory-bandwidth-bound dots show Helix `∥` as a *range*:
> the kernel faults ~1.2 GB fresh per run, so the wall swings with machine memory state (0.155 s
> fresh → 0.55 s under sustained load) while genuinely parallelizing (~4.7 cores) — bandwidth-bound,
> roughly C-class, not a clean multi-× win.

| kernel | class | C¹ | Rust | Go | CPython | NumPy | Helix ∥ | Helix ·1 | verdict |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| dot_i64 (50M) | memory-BW | 0.46 | 0.476 | 0.443 | 71.6 | 16.45 | 0.155–0.55 | 0.257 | **≈ C** |
| dot_f64 (50M) | memory-BW | 0.51 | 0.553 | 0.520 | 14.9 | 0.427 | 0.16–0.37 | 0.310 | **win** |
| allpairs (225M) | compute | **0.010** | 0.034 | 0.096 | 6.76 | 0.139 | 0.020 | 0.081 | **loss** |
| fib(40) | recursion | 0.102 | 0.161 | 0.305 | 7.63 | — | **0.025** | 0.006 | **win** † |
| matmul 512³ (tensor) | compute | 0.345 | 0.256 | 0.220 | 7.47 | 0.360 | **0.033** | 0.032 | **win** ‡ |
| basel 1/k² (100M) | float-div | **0.06** | 0.096 | 0.087 | 7.59 | 23.2 | 0.09 | 0.089 | **loss** |
| mandelbrot (1200²) | compute | 0.186 | 0.160 | 0.143 | 6.41 | 2.48 | 20.4 | — | **loss** |
| wordcount (5M) | string | 0.167 | 0.267 | 0.237 | 3.32 | 0.165 | 6.27 | 2.15 | **loss** |
| montecarlo (1e8) | rng | 0.264 | 0.240 | 0.265 | 35.5 | — | 42.9 § | — | **loss** |
| sieve (1e7) | memory | 0.014 | 0.016 | 0.019 | 0.628 | 0.097 | 92 § | — | **loss** |

¹ C uniform `-O3 -march=native` (mandelbrot `+ -ffp-contract=off`). † `fib` wins by **changing the
complexity class** — Helix auto-memoizes pure recursion (O(n) vs O(2ⁿ)), a language feature, not
faster codegen. ‡ `matmul` is the native tensor `.matmul()` (BLAS-like GEMM); the *naive
triple-loop* Helix path is **21.9 s** (VM scalar). § now **completes** — fixed this session
(lazy-`enumerate`, `isqrt`+short-circuit); was `>260 s`.

**Scorecard vs `-march=native` C: 3 wins · 1 wash · 6 losses** — the honest, humbler picture; the
original "5 wins · 1 tie" was inflated by a scalar-C baseline. The reading:

- **Helix's clear, durable wins are ALGORITHMIC / library, not codegen:** `fib` (auto-memoization,
  O(n) vs O(2ⁿ)) and `matmul` (native BLAS-like tensor GEMM) — ~4× and ~10× over C because Helix
  does *different, better-asymptotic* work, disclosed as such, not a loop-for-loop win.
- **On memory-bandwidth-bound streaming (the dots) it is ~C-class:** Helix parallelizes the build
  and hits the DDR ceiling, roughly where `-march=native` C lands (SIMD doesn't help a
  bandwidth-bound loop); the absolute margin swings with machine memory state — a wash, not a win.
- **On compute-bound SIMD-friendly kernels it LOSES to C:** `allpairs` (0.020 vs 0.010) and `basel`
  (0.09 vs 0.06) — gcc auto-vectorizes (AVX2), Cranelift emits scalar. The standing "SIMD in the
  JIT" gap. For i64 (`allpairs`) it is closable (integer add is associative → oracle-safe to
  vectorize); for f64 (`basel`) it is a **deterministic ceiling** — SIMD-reassociating the sum
  changes rounding and would break bit-identity across Helix's three engines, so Helix ties *scalar*
  C there by design.
- **NumPy is the array rival to respect** (vectorized dot_f64 0.43 s; C-level sieve/histogram).
- **The remaining losses are structural** (the roadmap): no native loop (mandelbrot), a slow
  string/histogram path that *anti-scales* under threads (wordcount); `montecarlo`/`sieve` were the
  catastrophic timeouts and are now fixed to *completing* (42.9 s / 92 s — still interpreter-speed,
  but no longer `>260 s`).

### Two things the anchor-verify gate caught

1. **A wrong reference value.** All five sieve implementations agreed on **664579**, disagreeing
   with the authored anchor 620420 — so the *programs* flagged the *reference*. 620420 is exactly
   the Prime Number Theorem estimate `x/ln x` (a lower bound); the true π(10⁷) must exceed it:
   `x/ln x = 620420 < π(10⁷) = 664579 < li(10⁷) ≈ 664918` (verified three independent ways).
2. **A compiler rounding divergence.** gcc at `-O3 -march=native` printed **86452960** for
   mandelbrot while Rust/Go/Python/NumPy — and Helix — all printed **86452986**. Cause: **FMA
   contraction** fusing `2·zr·zi + c` into a single rounding on 26 of 86 M iterations.
   `-ffp-contract=off` (or no `-march=native`) matches. Not a bug — FMA is *more* accurate — but
   notably Helix's strict-IEEE f64 sided with the reference; gcc's aggressive default was the
   outlier. (C timed with contraction off, so the number above matches the anchor.)

### The tie + four losses are the standing perf roadmap

- **basel (tie).** Serial, order-fixed f64 series; already at the optimal-serial ceiling. f64
  reassociation is non-associative (forbidden by the oracle), so a "win" would require an *opt-in*
  relaxed-order sum. Likely leave as-is.
- **mandelbrot (loss).** No native loop — per-pixel escape iterates on the interpreter. The lever
  is **JIT-compiling tail-recursive scalar functions into native loops** (also unlocks Newton /
  fixed-point / ODE steppers, and montecarlo's scalar-RNG form). Highest leverage.
- **wordcount (loss).** Per-element `String` allocation in the map + a parallel histogram that
  regresses under threads (6.27 s ∥ vs 2.15 s ·1). Needs interned strings + a scalable merge.
- **montecarlo (loss).** RNG-gen is fine (~7 s at 1e8); `enumerate()` over two 800 MB arrays
  balloons to multi-GB → swap. Needs a **fused streaming `enumerate().map().sum()`**.
- **sieve (loss).** Immutable model → functional trial division that is O(N²) because the lazy
  `filter` does not short-circuit. Needs short-circuit `any`/`all` + a bounded divisor range
  (→ O(N√N)); truly sieve-class mutable algorithms want a native builtin (delegate to a Rust crate).

## Caveats & honest boundaries

- **Not a controlled benchmark.** Single machine, WSL2, warm cache, best-of-3, ~±30% variance.
  Rankings are stable; absolute numbers are not.
- **Auto-parallelism vs single-threaded baselines.** Where Helix beats single-threaded C/Rust/Go
  it is because it parallelizes (array construction, the nested outer loop) across cores — a real,
  automatic win, but a multi-core-vs-single-core comparison. The single-core *reduce* is at C-parity.
- **No SIMD in the JIT (yet).** Cranelift does not auto-vectorize, so on a per-core basis Helix
  trails GCC/LLVM's SIMD. This is the one remaining gap to C-OpenMP and the clear next lever.
- **Euclidean `%`.** Helix's `%`/`//` are euclidean (always-non-negative remainder), a deliberate
  semantic choice; a `%`-heavy loop carries a small constant overhead vs C's truncating `%`.
- **Scope.** These are the numeric-kernel fast paths (i64/f64 `map`/`filter`/`reduce`, ranges and
  packed arrays). String/DataFrame/dynamic code runs on the VM; DataFrame throughput is measured
  separately in [benchmarks.md](benchmarks.md).
- **Correctness is not traded for speed.** Every JIT kernel is asserted bit-identical to the
  bytecode VM *and* the independent tree-walker over tens of thousands of random programs; a native
  read past an array bound falls back to the exact checked interpreter error.

## Reproduce

Copy each Helix program above into a `.helix` file and run it with and without the JIT:

```sh
helix run kernel.helix               # JIT (native)
HELIX_NOJIT=1 helix run kernel.helix # bytecode VM (same result)
time helix run kernel.helix          # wall/user/CPU% — CPU% > 100% shows parallelism
```

For the cross-language comparison, transcribe the same computation into `kernel.c` / `.rs` / `.go`
/ `.py` (single loops; use `i ^ j` — not `(i-j)²` — for the nested kernel so the compilers can't
fold it to a constant), compile `gcc -O3` / `rustc -O` / `go build`, and time best-of-3. Confirm
all languages print the identical value first.

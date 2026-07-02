# Helix JIT numeric-kernel benchmarks (vs C / Rust / Go / Python)

**Summary:** Helix compiles pure-numeric `map`/`filter`/`reduce` kernels — including
**array-indexed reductions** (`a[j]`) and **nested reductions** (`map` of `reduce`) — to
native machine code via Cranelift. On three representative kernels it is **at or ahead of
single-threaded C**, beats NumPy, and runs ~20–700× over its own bytecode interpreter — every
result **bit-identical** across all languages (and enforced bit-identical across Helix's own
three engines by a differential oracle; see [execution-engine.md](execution-engine.md)).

| kernel | size | Helix (JIT) | C `-O3` (1 thread) | best other | notes |
|---|---|---|---|---|---|
| **i64 dot product** | 50M | ~0.34 s | 0.47 s | Go 0.46 s | memory-bandwidth-bound; ~C-parity |
| **f64 dot product** | 50M | 0.36 s | 0.47 s | Go 0.48 s | beats 1-thread C (auto-parallel build) |
| **nested all-pairs** `Σ(i^j)` | 900M | 0.06 s | 0.08 s | C-OpenMP 0.01 s | beats vectorized 1-thread C; only SIMD+threads is faster |

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

`S = Σ_{i,j<N} |codes[i] − codes[j]|`, `codes[i] = (i·C) % M`, N = 6000 (36M pairs) — the
**distance-matrix / all-pairs-similarity** shape (phylogenetics, clustering, sequence
comparison). Unlike the pure-arithmetic nested reduce above, the inner reduce **indexes an
array by both the outer index `i` and the inner counter `j`** (`codes[i]` and `codes[j]`), so
the inner kernel reads captured memory at a scalar index — the capability that makes this run
native. The outer map runs serially (the inner captures arrays, so it isn't auto-parallelized
yet); the inner reduce is native.

```helix
codes = (range(0, 6000)).map(i => (i * 2654435761) % 1000003)
D = (range(0, 6000)).map(i => (range(0, 6000)).reduce(0, (acc, j) => acc + abs(codes[i] - codes[j])))
print(D.sum())
```

Measured at N=15000 (225M pairs), where the times actually resolve — at N=6000 everything
finishes in a few milliseconds, below the 0.01 s timer grain (all print the same anchor):

| language | wall (best of 3) | vs C |
|---|---|---|
| **C `-O3` (SIMD, 1 thread)** | **0.04 s** | 1.0× |
| Go (1 thread) | 0.11 s | 2.8× |
| **Helix (JIT, serial outer)** | **0.14 s** | 3.5× |
| Helix (no-JIT, VM) | ~28 s | ~700× |

**This is a compute-bound kernel where C wins**, and the doc is honest about why. The array
(`codes`, ~120 KB) is L2-resident, so it is *not* memory-bandwidth-bound like the dot products —
it is limited by arithmetic throughput, and there C has two edges Helix lacks: (1) gcc/LLVM
**auto-vectorize** the inner loop (AVX2 does ~4 `|codes[i]-codes[j]|` per instruction), while
Helix's Cranelift JIT emits a **scalar** loop — it does not auto-vectorize; (2) Helix runs the
outer map **serially** for this array-indexed shape (not parallelized yet). So Helix is ~3.5×
slower than SIMD C and ~1.3× slower than Go here.

The win is real but relative to the *interpreter*: the array-indexed all-pairs shape used to fall
entirely to the bytecode VM (~28 s / ~700× slower); it now runs a native inner reduce. (An earlier
draft of this doc reported C at "0.00 s, constant-folded" — that was wrong: feeding N from `argv`
so the compiler cannot precompute still shows C finishing the real loop in ~2 ms at N=6000. It was
too fast to measure, not folded.) Closing the gap to C is a known two-part lever: **SIMD in the JIT**
(Cranelift does not auto-vectorize) and **parallelizing the outer loop** over this shape — the
latter alone would add the ~5–6× the pure-arithmetic §3 kernel already gets from going multi-core.

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

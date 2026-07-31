# Kernel benchmark results — 2026-07-18 (HEAD `4cddbbc`+)

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
| k1 | dot 50M i64 | 0.16s (254%) | **0.10s** | 0.13s | 0.57s | 8.78s | 0.28s | **1.6× slower** (≈4× per-core; see k1 note) |
| k2 | mandelbrot 1200² | 0.42s (99%) | 0.08s | **0.07s** | **0.07s** | 6.85s | — | **5.3× slower** |
| k3 | basel 1e8 | 0.09s (94%) | **0.06s** | 0.09s | 0.10s | 10.36s | 0.48s | **1.5× slower** (ties Rust, beats Go) |
| k4 | allpairs 15k | 0.01s (313%) | **<0.01s** | <0.01s | 0.05s | 9.99s | 0.17s | **slower** (timer-grain; ~7× per the audit) |
| k5 | montecarlo 1e8 | 0.51s (108%) | 0.25s | **0.22s** | 0.26s | 44.60s | — | **2.0× slower** |
| k6 | sieve π(10⁷) | **0.01s** (105%) | 0.01s | 0.01s | 0.02s | 0.60s | 0.06s | ~tie (**delegation** — beats NumPy 6×) |
| k7 | wordcount 5M | 1.39s (108%) | **0.20s**† | 0.24s | 0.21s | 1.37s | 1.79s | **7.0× slower** (also loses to CPython) |
| k8 | matmul 1024³ (build + GEMM) | **0.04s** (120%) | — | — | — | — | 0.07s (471% CPU) | **1.75× FASTER than NumPy** (was 4.4× slower at 0.31s — see below) |
| k9 | matmul 512³ (naive) | 0.49s (109%) | 0.39s | 0.33s | **0.32s** | 15.45s | — | **1.3× slower** |
| k9m | matmul 512³ (map-temp) | 0.48s (108%) | 0.39s | 0.33s | **0.32s** | 15.45s | — | **1.2× slower** (was **27.12s / 75×** — see below) |

† k7's C entry came out of the suite run at **5.75s with 3% CPU** — a starved process, not a
slow one, and not a measurement. Re-run alone it is 0.20s at 108% CPU across five consecutive
runs, which is the number recorded. Any row whose `%CPU` is far from ~100% (for the
single-threaded references) or far from its usual value (for Helix) should be treated the same
way: re-measured, not published.

**Helix loses to C on every kernel in this suite**, though k9 (both spellings) is
now within 1.4× and k3 beats Rust and Go. The one place it stands out is k6,
where it wins by *not doing the work* — calling a native `primes()` builtin, the
same way NumPy wins by calling BLAS. The honest counterpart is
`k6_sieve_trial.helix` (pure-Helix trial division): **88.31s**, i.e. ~4400×
slower than the builtin and ~8800× slower than C's sieve. That gap is the
kernel's whole point.

**Against NumPy the picture is different, and k8 has now flipped.** k8 is
0.04s vs 0.07s (**1.75× faster**) and k6 wins 6×; those are the two kernels with a
NumPy reference where Helix leads. Both wins deserve the same caveat as NumPy's
own: k6 delegates to a builtin, and k8's win is a *whole-program* one — Helix
beats NumPy on build + convert + GEMM together while still losing the isolated
GEMM to OpenBLAS (~1.8×). It is a real end-to-end result, not a claim that faer
beats OpenBLAS.

**What changed since 2026-07-17, and what did not.** One number in this table
moved materially: **k9 map-temp, 27.12s → 0.50s (54×)**, which also erases the
75× penalty for writing the natural `map().reduce()` spelling instead of the
hand-transcribed one. Everything else is within run-to-run noise of the previous
table. That is the honest shape of the result: the arc that produced it (map-side
array indices for i64/f64, affine indices, value scalars on both the map and
reduce sides, and map→reduce fusion — commits `00c23fd`…`e178c76`) targeted
*comprehension shapes that previously fell to the bytecode VM*, and *k9 map-temp
is the only kernel in this suite that writes one*. The other eight already used
shapes the JIT compiled. So the suite confirms the fix and simultaneously shows
that this suite under-covers the new capability — the shapes it most helps (BLAS-1
vector ops, sums of a computed vector) have no kernel here yet. That gap is
**The suite also under-covers the JIT's builtin surface.** A separate audit of all 22 numeric
builtins found **17 that block compilation outright** — a builtin outside the JIT's subset does
not slow a loop down, it forces the *entire* loop onto the bytecode VM. `to_float`, `to_int` and
`sign` have since been lowered (132–308× each, measured off-suite); `floor`/`ceil`/`round`/
`trunc`/`clamp` still block because they can *raise*, and a kernel cannot. None of that is
visible in the nine kernels here, because none of them uses a blocked builtin in a hot loop. See
[docs/jit-builtin-coverage.md](../../docs/jit-builtin-coverage.md) for the standing table, the
reason for each exclusion, and how to re-run the audit.

tracked as k10, not papered over: measured off-suite on this same release binary,
the identical expression before and after the arc (n=5M, min-of-4), a **SAXPY sum**
(`Σ a*x[i]+y[i]`) went **0.64s → 0.02s** and a **vector-add sum** (`Σ a[i]+b[i]`)
**0.66s → 0.02s** — both ≈32×. Those are Helix-vs-Helix numbers and are
deliberately **not** in the scorecard: with no C/Rust/Go reference they say nothing
about how Helix compares to anything, only that it improved.

## k9: 72× → 1.4×, and the map-temp spelling from 75× to parity

The first run of this suite reported k9 at 25.86s against C's 0.36s and blamed
"the nested map-of-reduce shape never reaches the JIT". **The shape was never
the blocker** — a nested map-of-reduce with a *bare counter* index (`a[k]`)
always compiled (0.01s vs 0.43s under `HELIX_NOJIT=1`, a 43× gap). The real
blocker was that `a[i*n+k]` is an `Expr::Binary`, and the reduce kernel's index
collector only admitted `arr[counter]` and `arr[scalar]`. A *flat*, un-nested
`(0..n/2).reduce(0.0, (s,k) => s + a[2*k])` failed to compile for the same
reason — which is what isolates the cause from the nesting.

Two changes closed it, and only one of them is a compiler change:

1. **Affine indices** (`IndexBound::Affine`): admit `base + coef*counter` with
   `base`/`coef` loop-invariant, and have the VM prove the access set in bounds
   by checking the two *endpoints* in `i128` before the kernel's unchecked
   loads (the index is monotone in the counter, so the endpoints bracket every
   access). **53×**, anchor unmoved.
2. **A faithful transcription.** k9's inner loop was
   `(0..n).map(k => …).reduce(…)`, which materializes an n-element temporary for
   every one of the n² (i,j) pairs — 262,144 arrays at n=512 that C never
   allocates. C accumulates into a scalar; `(0..n).reduce(0.0, (s,k) => …)` is
   the literal port. This is a *fidelity* fix the audit had already demanded
   ("'same algorithm' is slightly overstated"), not a dodge — it removes an
   allocation the reference doesn't have.

The map-temp spelling is **kept and still measured**, as
`k9_matmul_naive_maptemp.helix`. It was **27.12s** here — a 75× penalty for
writing the natural `map().reduce()` instead of the hand-transcribed reduce.
Reporting it beside the fast spelling was the point: hiding it behind the
faithful port would have been exactly the flattery this suite exists to prevent.

**That gap is now closed: 0.50s (54×), i.e. parity with the transcribed spelling
(0.52s) and 1.4× C.** It took three separate fixes, and the order matters because
each one only exposed the next:

1. **Map-side array indices.** `value_eligible_cap` had no `Expr::Index` arm, so
   *any* map body reading `a[…]` ran on the bytecode VM. Admitting it required a
   bounds obligation the map side cannot discharge in general — a `map`'s binder
   is an *element value*, not a counter, so `xs.map(x => a[x])` indexes on
   arbitrary data (and possibly negative data, which the interpreter Python-*wraps*
   rather than rejecting). It is therefore admitted only over a lazy `Range`
   source, where the elements *are* the counter and the reduce side's
   two-endpoint `i128` proof transfers intact (`00c23fd`, and `7b8f047` for the
   f64 twin).
2. **Affine map indices.** `a[i*n+k]` needed `IndexBound::Affine` on the map side
   too, composed with the source range's step — which can exceed `i128`, so the
   endpoint check is `checked_*` and overflow *declines* exactly as an
   out-of-range endpoint does (`e1692ba`). This brought it to ~6s.
3. **map→reduce fusion.** The remaining ~12× was the intermediate array itself:
   262,144 n-element temporaries at n=512. Fused via the identity
   `map(f).reduce(init,g) ≡ reduce(init,(acc,i) => g(acc,f(i)))`, emitted as a
   JIT guard whose *fall-through is the original unfused expression* — so the
   fused body only ever runs as native code under proven preconditions and
   therefore cannot raise, which is what retires fusion's error-ordering hazard
   (unfused, `map` evaluates every `f(i)` before any `g`, so if both can raise the
   two spellings report different errors) (`e178c76`).

So the natural spelling and the transcribed one now cost the same, which is the
outcome that makes the fidelity argument above moot rather than merely defensible.

**Verification**: gate 366 bin + 124 cli, clippy baseline, vmparity 0; corpus
goldens `j1`–`j6` (all three engines byte-identical); seven named boundary sweeps,
each carrying an *engagement* assertion so a silent fall-back cannot pass as a
pass; ~8,500 fuzzed programs across the arc, 0 divergences. Unchecked native
loads are the risk class here, so every bounds check was **sabotage-proven** —
removed one at a time to confirm each turns a test red, because a guard whose
removal breaks nothing is decoration. The endpoint proof is done in `i128` (so the
check itself cannot overflow), a negative index — which the interpreter
Python-*wraps* rather than rejecting — falls back to the checked bytecode loop,
and the f64 paths additionally guard *type confusion*: an `Ints` buffer reaching
an `f64` load would reinterpret the bits as denormal junk (~5e-323 where 20.0
belongs) with no crash, so the marshal declines on representation before any
pointer is formed. Forcing it to accept prints exactly that junk, which is how the
guard was verified rather than assumed.

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

  **Its parallelism saturates at 2 threads — threads 3-6 are pure waste.**
  Measured (page-equalized, N=50M, min of 3):

  | `RAYON_NUM_THREADS` | wall | %CPU | vs previous |
  |---|---|---|---|
  | 1 | 0.24s | 104% | — |
  | 2 | 0.17s | 152% | 1.4× wall for 1.5× CPU — a fair trade |
  | 4 | 0.16s | 220% | 1.06× wall for 1.45× more CPU |
  | 6 | 0.16s | 300% | **0% wall for 36% more CPU** |
  | *C* | *0.11s* | *107%* | |

  This is the predicted consequence of the phase being **page-fault-bound
  rather than bandwidth-bound**: first touch takes `mmap_sem` in the kernel, so
  the faulting serializes no matter how many workers ask. Helix ends up trading
  **2.8× the CPU for 1.5× the wall time** here. That is a defensible
  latency-over-throughput choice, but it must not be read as codegen: the
  honest per-core comparison is **single-threaded Helix 0.24s vs C 0.11s =
  2.2×**. (An earlier version of this file said 3.8× per-core — that was
  CPU-seconds at 6 threads, which charges Helix for the *wasted* CPU as if it
  were work. Corrected.)
- **k2 mandelbrot** — scalar FP codegen and branch prediction *for the C/Rust/Go
  siblings*. **For Helix it currently measures the per-pixel wrapper, not the
  escape loop.** gcc contracts one `2*zr*zi+ci` into an FMA at `-march=native`;
  we build with `-ffp-contract=off` for rounding parity. Note the escape test
  lands **exactly 1 ulp** from the boundary at two pixels (x=80, y=320/880), so
  cross-build agreement is real but has zero headroom — `-ffast-math` does
  change the anchor.

  **Where Helix's 5.9× actually goes (measured 2026-07-17, grid 600 = 360k
  pixels and 10.18M total escape iterations):**

  | variant | time |
  |---|---|
  | trivial callee (`fn step(...) = i + 1`) — wrapper only, **0 escape iters** | 0.10–0.13s |
  | the real kernel — same pixels **+ 10.18M escape iters** | 0.10s |

  **The escape loop is free; the wrapper is the entire runtime.** The native
  tail loop is doing 10M float iterations inside the noise of the
  `map(y => map(x => …).sum()).sum()` that surrounds it. Two hypotheses were
  tested and **refuted** on the way: hand-CSE'ing `zr*zr`/`zi*zi` into `let`s
  changes nothing (0.11s either way — Cranelift already CSEs), and the
  per-float-compare NaN-poison guard is free (30M-iteration tail loops cost
  0.03s with and without a float compare — the never-taken guard predicts
  perfectly).

  So k2 is **not** a codegen result and must not be quoted as the Cranelift
  ceiling. It is the same class as k9's old 72×: a surrounding comprehension the
  JIT does not compile, making a fast inner kernel irrelevant. It belongs with
  the map-side gap, not with scalar codegen.
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
- **k8 matmul (GEMM)** — **the row title is misleading and the gap is not the GEMM.**
  Isolated GEMM at n=2048: faer ~0.11s vs OpenBLAS ~0.061s, so OpenBLAS is ~1.8×
  faster — but that ~1.8× was levied on a small slice of k8, and the row was never
  really a BLAS comparison. Phase split at n=1024, cumulative, the same script run
  at each stage:

  | phase | original | after captured-map | after `tensor()` |
  |---|---|---|---|
  | build one nested 1024×1024 | 0.08s | 0.01s | 0.02s |
  | build both | 0.17s (44%) | **0.02s** | 0.03s |
  | + `tensor()` both | 0.32s (38%) | 0.14s | **0.03s** |
  | + `matmul` (= k8) | 0.37s (13% was the GEMM) | 0.17s | **0.05s** |

  NumPy's own split: 0.04s import, 0.05s building both, 0.06s total — so NumPy's
  GEMM is ~0.01s. Helix was losing k8 at *building and converting the inputs*, not
  at matrix multiply; fixing faer would have recovered at most the 13%. Both input
  costs are now gone: construction 8.5× (the captured-map kernel) and `tensor()`
  conversion from 0.12s to under the timer's resolution.

  **Whole kernel, min-of-8: Helix 0.04s vs NumPy 0.07s — Helix is 1.75× faster**,
  at 67 MB against NumPy's 58 MB, and it wins while using ~120% CPU to NumPy's
  471%. Output bit-identical on all three engines (`606023.500000`).
  At n=512 the GEMM is ~1% of the wall time and you are timing interpreter
  startup, which is why the default is 1024.
- ~~**k8's build does not reach native code.**~~ **FIXED.** An `Int`-source `map`
  whose body produced `Float` used to compile *only if the body captured nothing*.
  Replacing a literal with a variable was the entire difference — at 4M elements
  `((7 * j) % 100) * 0.5` ran native at 0.01s while `((c * j) % 100) * 0.5` sat on
  the VM at 0.37s. The `i64` analysis took captures; the mixed `Int`→`Float` one was
  capture-free by construction, and the indexed mixed analysis that *does* take
  captures required a non-empty `index_bounds`, so an unindexed captured body
  matched no analysis at all.

  A free scalar now rides as a plain `i64` capture, which is sound because both VM
  dispatch sites require it to be a `Value::Int` at run time and decline otherwise —
  the same runtime proof the i64 map path always used. Measured at 20M elements,
  min-of-5 on both engines:

  | body | JIT | VM | |
  |---|---|---|---|
  | `((c * j) % 100) * 0.5` captured | 0.02s | 1.72s | **86×** |
  | `((7 * j) % 100) * 0.5` literal | 0.02s | 1.69s | 84× |
  | `(c * j) * 0.5` | 0.02s | 1.42s | 71× |
  | `i * dt * 0.001` | 0.02s | 1.45s | 72× |

  The captured spelling now matches the capture-free one (86× vs 84×), which is the
  result that matters: the *inversion* is gone, not merely the absolute time. A
  `Float` capture still declines to the bytecode loop rather than promoting early —
  sabotaging that check makes `c = 2.5` return `[0.0, 1.0, 2.0, 3.0]` on the JIT
  against `[0.0, 1.25, 2.5, 3.75]` on the other two engines.

  Two hypotheses were tested and rejected before the fix, not assumed: reassociating
  to promote first (`(j * 0.5) * c`) did not help, so it was the missing analysis
  rather than the `mix_combine` value-scalar rule; and nesting was not the cause —
  the same body at top level with a captured scalar was equally VM-bound. k8 only
  *looked* like a nesting problem because its inner body captures the outer binder.
### The %CPU column is a trade you can now decline

Helix's wall-clock standing on k1 and k4 is bought with cores the references never use, which
is why `%CPU` is in the table. Quantified (min-of-4, gate profile), the scaling is
workload-dependent in a way no single default can serve:

| | 1 thread | all cores | wall gain | total CPU |
|---|---|---|---|---|
| k4 all-pairs (compute-bound) | 0.59s @ 99% | 0.11s @ 550% | **5.4×** | 0.58 → 0.61 core-s (**+4%**) |
| k1 dot (allocation-bound) | 0.14s @ 96% | 0.08s @ 300% | 1.75× | 0.13 → 0.24 core-s (**+79%**) |

On compute-bound work the cores are nearly free. On k1 they are not: much of that kernel is the
OS faulting in and zeroing 800 MB of fresh pages, so parallel efficiency falls to ~45% and the
last cores buy almost nothing — 2 threads reach 0.10s for 0.13 core-s where all cores reach
0.08s for 0.24. Per core, C is ~4.3× more efficient than default-threaded Helix here and ~2.2×
more efficient than serial Helix; only the second number is about codegen.

`HELIX_THREADS=N` now caps the pool (`1` = fully serial), so the trade is the caller's to make
— see [docs/deployment.md](../../docs/deployment.md). Before this, the only lever was rayon's
own `RAYON_NUM_THREADS`, which is an implementation detail no Helix user had reason to know.
Results are identical at every thread count, pinned by `thread_count_changes_cpu_not_results`.

- **k9 matmul (naive)** — the honest triple-loop peer group. Both Helix spellings
  now reach the JIT: the transcribed inner `reduce` compiles to a native loop over
  affine indices, and the `map().reduce()` spelling *fuses* so the per-(i,j)
  temporary is never allocated (it used to be 262,144 arrays at n=512, and the
  earlier note here — "never reaches the JIT, identical time with `HELIX_NOJIT=1`"
  — described that state and is superseded: it is now **0.50s JIT vs 27.76s under
  `HELIX_NOJIT=1`, a 55× engagement gap**. That the no-JIT time still matches the
  previously published 27.12s is the clean confirmation that the old number *was*
  the VM path.

## Known reporting caveats

1. **Go is not page-size-equalized** (k1) and builds at GOAMD64=v1 while C/Rust
   get `-march=native` / `target-cpu=native`. Measured cost of v1-vs-v3 on k2:
   nil. On k1 it is real and unquantified.
2. **k4's C/Rust times are at timer grain** (`<0.01s`). The audit's finer
   measurement puts Helix ≈6.9× behind C there.
3. ~~**CPython's k9 number beats Helix's** (14.79s vs 25.86s).~~ **Resolved** —
   Helix's k9 is now 0.49s (transcribed) / 0.48s (map-temp) against CPython's
   15.45s. Kept struck through rather than deleted: this caveat was true when
   published, and the record of it being true is what made fixing it a priority.
4. `-march=native` is a ~9% *pessimization* for gcc on k2 (79ms vs 72ms without).
   We keep it as the conventional "give C every advantage" flag; it slightly
   understates C.
5. **This suite under-covers the 2026-07-18 arc.** Only k9's map-temp spelling
   exercises the comprehension shapes those commits fixed, so eight of the nine
   kernels are unchanged by them. The shapes most improved (BLAS-1 vector ops,
   sums of a computed vector) have no kernel here — tracked as k10. Until it
   exists, any claim about those shapes is a Helix-vs-Helix number and is labelled
   as such. The same is true of the JIT's *builtin* surface — see
   [docs/jit-builtin-coverage.md](../../docs/jit-builtin-coverage.md).
6. ~~**Helix is the heaviest implementation here** — ~1.20 GB peak RSS on k1
   against C's 783 MB, ~1.5× over, and not charged anywhere in the score.~~
   **Resolved** — k1's peak is now **815,912 kB against C's 782,848**, i.e. 1.04×.
   The entire overhead was one transient: the native map kernel read its source
   from memory, so a lazy `(0..n)` was materialized into a full-size buffer purely
   to be read once, and it stayed live alongside the output — every
   `(0..n).map(f)` peaked at *twice* its result. The kernel is now fed values
   generated per 16K chunk, so nothing is stored. Kept struck through for the same
   reason as (3): it was true, and recording that it was true is what got it fixed.

   The companion case — a map over a *real* array, which has a buffer that cannot be
   generated away — is now also handled, though it does not show in this table
   because no kernel here chains maps. When the receiver's `Rc` is the only handle
   (a dead intermediate, as in `xs.map(f).map(g)`) the kernel writes back into it
   rather than allocating a second full-size buffer. Measured at n=20M, 160 MB per
   buffer: a two-stage chain fell **340 MB → 186 MB**, a three-stage chain
   **344 MB → 186 MB**. A source that is still named keeps its own buffer and is
   never rewritten — that is what `Rc::get_mut` is checking, and removing the check
   makes a live `src` print its mapped values instead of its own.
7. **k2's 5.3× is NOT the inner loop — and the cause is now identified.** `row`'s
   `2.7 / to_float(g)` is a float division by a NON-LITERAL, which the mixed-function
   analysis declines (only nonzero Float literals are admitted — native `fdiv` yields
   inf where the interpreter raises on `/0`). `row` and `grid` therefore run on the
   VM, and every pixel pays ~250 ns of VM dispatch into the native `step`. Confirmed
   by precomputing the reciprocals and passing them as parameters: **0.39s → 0.07s**,
   anchor byte-identical. The kernel stays as written — the idiomatic spelling is the
   thing measured — and the fix (a /0 poison bail for non-literal divisors, the same
   immediate-bail mechanism the mixed ABI uses for NaN compares) is tracked in
   `docs/ROADMAP.md`. The earlier findings below remain true and ruled out. Holding the pixel count fixed and raising
   the iteration cap: at cap=100 (as shipped) Helix is 10× behind C, at cap=1,000
   it is 1.4×, and at cap=10,000 it is **1.0×** — at cap=100,000 Helix is slightly
   ahead. So the generated loop matches or beats gcc, and the whole gap is ≈250 ns
   of fixed cost per pixel. Measured and RULED OUT as the cause: the scalar
   function-call path (~2.5 ns/call over 4M calls, recursion included), callee arity
   (2/3/5 mixed args all ~2.5 ns), and the three-layer nesting (flattening it is
   worse). Every spelling reports as natively compiled. The cause is still open —
   see `docs/ROADMAP.md`. Caveat for anyone re-running this: gcc at `-march=native`
   contracts to FMA, so iteration counts drift between the two at high caps
   (86125823 vs 86125368 at cap=1,000); those runs are not anchor-clean.
8. **A row with an implausible `%CPU` is not a measurement.** k7's C entry came out
   of one suite run at 5.75s with 3% CPU — a starved process. Re-run alone it is
   0.20s at 108% across five consecutive runs, which is what the table records.
   Re-measure such rows rather than publishing them.

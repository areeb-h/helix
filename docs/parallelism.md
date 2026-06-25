# Vectorized kernels & parallelism

Helix runs `map`/`filter` over a typed integer array as **native machine code**, not a
per-element interpreter loop — the same Cranelift JIT path that already compiles `reduce`
loops. This is both a large single-threaded speedup and the foundation for safe
parallelism, because the two are the same work.

## Why this design (and how it differs from others)

The naive approach — spawn threads that run the interpreter over chunks — is blocked at
the core: a Helix `Value` is `Rc`-based, so it is `!Send` and cannot cross a thread
boundary. Retrofitting around that is exactly the trap others fell into:

- **Python** spent ~30 years on the GIL because its refcounting (our `Rc`) can't be shared
  across threads without atomics; the 3.13 free-threading build pays a single-thread tax.
- **Julia / Go** offer ergonomic threads but data races are *representable* — you can get
  it wrong.
- **Rust's** rayon is safe but demands `Send`/`Sync`/`Arc` ceremony unfit for a scientist
  writing a one-off analysis.

Helix sidesteps all of it: the JIT kernel deals only in scalar `i64` over a packed
`&[i64]` buffer — **no `Rc`, no heap, no shared mutable state in the hot loop**. There is
no refcount to make atomic, and a pure stateless kernel is race-free *by construction*, so
parallelism needs zero user annotations and admits no data race.

## What compiles to a kernel

A `map`/`filter` becomes a native kernel when the array is a packed `Int` array and the
single-parameter body is a pure `i64` expression — integer `+ - *`, `%` by a positive
constant, comparisons, `if`/`then`/`else`, and `let` (the same eligibility as a `reduce`
loop). Anything else — float arrays, `missing`, a body calling a function or using
`/`, multi-binder destructuring — transparently **falls through to the bytecode loop**.
Correct everywhere; accelerated where it can be. The JIT itself is x86-64 Linux only;
other targets always run the bytecode loop.

Every path is held to the tree-walker's result byte-for-byte (the parity oracle): integer
arithmetic wraps identically, and `map`/`filter` preserve element order.

## Performance

`map` over 10M integers, summing the result (debug build; the JIT emits the same native
code in any profile, while the bytecode VM is debug-compiled, so these ratios are upper
bounds — but the native kernel's absolute speed is real):

| Path | trivial body `x*2+1` | heavy body (≈12 ops) |
| --- | --- | --- |
| tree-walker | 6.8 s | — |
| bytecode VM | 2.85 s | 11.0 s |
| **native kernel** | **0.21 s** (~13×) | **0.53 s** (~21×) |

The native kernel is the headline win: it removes the per-element `Value` allocation and
the bytecode dispatch that made per-element `map` the slowest path in the language.

## Parallelism: implemented, correct, opt-in

The kernel runs across worker threads (`std::thread::scope`, no dependency) over disjoint
input/output chunks — race-free, order-preserving, and identical to a sequential run
(verified by a forced-parallel parity test). But it is **opt-in**, because measurement is
honest: for this class of integer kernels the work is *memory-bound* — reading and writing
the buffers dwarfs the per-element arithmetic (which has no loops or calls) — so splitting
across threads adds memory-traffic contention without adding bandwidth, and rarely beats
(sometimes regresses) the single-threaded native kernel. Threads pay off only when
per-element compute is heavy.

Set `HELIX_PAR_MIN=<k>` to enable it (parallelize inputs ≥ 2·k across cores). The machinery
stays exercised and ready for the compute-heavier kernels a later phase will admit (helper
functions called from a body) and for hardware where the trade-off differs.

## Roadmap

- Inline immutable numeric globals (`xs.map(x => x * k)`) into kernels — broadens
  eligibility to a very common shape.
- Helper-function calls inside kernel bodies (the genuinely compute-bound case where
  parallelism wins by default).
- SIMD lanes for elementwise kernels; `f64` kernels.
- The general case (arbitrary pure functions over heterogeneous arrays) needs a `Send`
  value representation — a measured, separate decision (Arc vs. biased refcounting vs. a
  freeze-to-`Send`-snapshot boundary), deferred because the typed-numeric path already
  covers the scientific inner loop.

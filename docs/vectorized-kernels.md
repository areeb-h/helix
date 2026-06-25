# Vectorized kernels

Helix runs `map`/`filter` (and `reduce`) over a typed integer array as **native machine
code**, not a per-element interpreter loop — the same Cranelift JIT path that compiles
`reduce` loops. This removes the per-element `Value` allocation and bytecode dispatch that
made per-element `map` the slowest path in the language.

## What compiles to a kernel

A `map`/`filter` becomes a native kernel when the array is a packed `Int` array and the
single-parameter body is a pure `i64` expression — integer `+ - *`, `%` by a positive
constant, comparisons, `if`/`then`/`else`, `let`, and **calls to JIT-eligible user
functions** (`x => normalize(x)`; the function is compiled natively and called from inside
the loop). Anything else — float arrays, `missing`, a body using `/`, multi-binder
destructuring — transparently **falls through to the bytecode loop**. Correct
everywhere; accelerated where it can be. The JIT itself is x86-64 Linux only; other targets
always run the bytecode loop.

Every path is held to the tree-walker's result byte-for-byte (the parity oracle): integer
arithmetic wraps identically, and `map`/`filter` preserve element order.

## Performance

`map` over 10M integers, summing the result (debug build; the JIT emits the same native
code in any profile, while the bytecode VM is debug-compiled, so these ratios are upper
bounds — the native kernel's absolute speed is real):

| Path | trivial body `x*2+1` | heavy body (≈12 ops) |
| --- | --- | --- |
| tree-walker | 6.8 s | — |
| bytecode VM | 2.85 s | 11.0 s |
| **native kernel** | **0.21 s** (~13×) | **0.53 s** (~21×) |

## Why not threads

An earlier iteration ran these kernels across worker threads. We removed it: measurement
showed this class of integer kernels is **memory-bound** — reading and writing the buffers
dwarfs the per-element arithmetic (which has no loops or calls) — so splitting across
threads adds memory-traffic contention without adding bandwidth, and rarely beats
(sometimes regresses) the single-threaded native kernel. The reliable win is the native
kernel itself.

The data-parallel heavy lifting that *does* benefit from cores (large DataFrame operations)
already runs multi-threaded inside Polars, automatically; and running independent analyses
in parallel is just separate `helix` processes, which is more reproducible anyway. The
kernel substrate is `Send`-safe scalar code, so threads can be re-introduced cheaply if a
genuinely compute-bound feature ever needs them — but not as an option nobody should turn
on.

## Pipeline fusion

A *chain* of `map`/`filter` (optionally feeding a `reduce`) over an `Int` source compiles
to a **single native loop** — the element is threaded through every stage in registers,
with **no intermediate array** at any stage. A filter that rejects an element branches
straight to the next iteration (stream fusion's *Skip*). Where other eager languages
(NumPy/pandas/R) materialize a full array at every step, and languages that fuse make you
opt into laziness (Haskell, Polars' lazy frames), Helix fuses **eager method-chain syntax
transparently** — and the fused result is held to the tree-walker oracle byte-for-byte.

Fusion triggers for `≥2` map/filter stages, or `≥1` stage feeding a `reduce`, over an
idempotent `Int` array or a `range`; anything else (float, a non-eligible body, a
side-effecting source) falls through to the per-stage path. It is sound precisely because
eligible bodies are **pure** — eliminating intermediates is unobservable.

`map`/`filter`/`reduce` chains over 10M integers (debug build):

| Pipeline | fused | bytecode | speedup | peak RSS (fused vs bytecode) |
| --- | --- | --- | --- | --- |
| `range(N).map(f).filter(g).reduce(+)` | 0.08 s | 5.6 s | ~70× | **17 MB** vs 170 MB |
| `xs.filter(g).map(f).count()` | 0.11 s | 2.8 s | ~25× | 134 MB vs 170 MB |

The scalar pipeline (`range → … → reduce`) **allocates nothing** — its 17 MB is just the
runtime — and runs at the bare reduce-loop's C/Go-class speed.

## Roadmap

- Statistical sinks (`.sum()`/`.mean()`, with an incremental Neumaier compensator);
  inline immutable numeric globals into bodies (`x => x*k`); `f64` kernels; SIMD lanes;
  horizontal fusion (several aggregates in one pass).

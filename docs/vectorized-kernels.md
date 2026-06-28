# Vectorized kernels

Helix runs `map`/`filter` (and `reduce`) over a typed integer array as **native machine
code**, not a per-element interpreter loop — the same Cranelift JIT path that compiles
`reduce` loops. This removes the per-element `Value` allocation and bytecode dispatch that
made per-element `map` the slowest path in the language.

## What compiles to a kernel

An `Int`-array `map`/`filter` becomes a native kernel when the single-parameter body is a
pure `i64` expression — integer `+ - *`, `%` by a positive constant, comparisons,
`if`/`then`/`else`, `let`, **calls to JIT-eligible user functions** (`x => normalize(x)`;
the function is compiled natively and called from inside the loop), and **captured `i64`
variables** (`map`: `x => x*k + b`, where `k`/`b` come from the enclosing scope — they're
loop-invariant, resolved once at the call site and passed to the kernel as a `caps` slice;
up to 8 captures).

A **`Float`-array `map`** compiles too: the kernel is monomorphized over the element type
(the "Julia recipe" — one source, an `i64` and an `f64` instantiation, the VM dispatches on
the array at run time). The `f64` body is a deliberately narrow, divergence-free subset —
`+ - *` only (literals and up to 8 captured `f64`/`i64` values coerced to `f64`), no `/`
(Helix raises on float `/0` where native `fdiv` yields `±inf`), no `if`/comparison/call (a
float fn could return an `Int`, breaking result-type agreement), and the body **must**
reference the binder (so a `Float` source guarantees a `Float` result, matching the
interpreter). `fadd/fsub/fmul` are the same SSE scalar ops the interpreter runs, in the
same left-to-right order, so the result is bit-for-bit identical.

A **mixed `Int`-source → `Float` `map`** compiles as well (`range(N).map(j => j*0.001)`):
the kernel reads an `i64` element and writes an `f64`, typing the body **node by node** by
the interpreter's promotion rule — an `Int OP Int` subexpression stays `i64` (wrapping
`iadd/isub/imul`) and only the first `Float` operand promotes via `fcvt`, so an `i64` wrap
that happens *before* a float enters is preserved exactly. Same `{+,-,*}` subset, and —
because a capture's runtime type isn't known at compile time — **no captures** (a captured
`i` in `(i+j)*0.001` still falls through).

Anything else — `Float` *filters*, a float body using `/` or `if`/comparison/call, a mixed
body with a capture, `missing`, multi-binder destructuring — transparently **falls through
to the bytecode loop**. Correct everywhere; accelerated where it can be. The JIT itself is
x86-64 Linux only; other targets always run the bytecode loop.

Every path is held to the tree-walker's result byte-for-byte (the parity oracle): integer
arithmetic wraps identically, and `map`/`filter` preserve element order.

## Idiom: keep hot-loop element work on the typed path

The fall-through to the bytecode loop is **correct but ~25–100× slower** than the native
kernel. Both `i64` and `f64` `map` bodies now compile: `range(2M).map(x => x*2+k)` (captured
`i64` `k`) is **0.04 s**, and an `f64` map over 10M elements (`xs.map(x => x*2.0 + k)`) is
**~0.012 s** — vs **~1.26 s** when the same body falls through to the bytecode loop (~100×).

A plain mixed body now compiles — `range(10M).map(j => j*0.001 + 2.5)` is **~0.031 s** vs
**~0.89 s** through the bytecode loop (~28×). The cases that still fall through are a **mixed
body with a capture** (`(i + j) * 0.001` — the captured `i`'s runtime type is unknown at
compile time) and **any float body outside the `{+,-,*}` subset** (a `/`, an `if`, a
comparison, a call). For those, the reliable fast idiom is to **build with vectorized array
ops rather than a `map` closure**, because the broadcast operators run on the packed buffer
regardless of element type:

```helix
# slow in a hot loop — captured `i` in a mixed body → interpreter:
xs = range(0, 64).map(j => (i + j) * 0.001)
# fast — typed broadcast over the packed buffer, no per-element closure:
xs = (range(0, 64) + i) * 0.001
```

Same result, no per-element dispatch. As a rule: **one fat native/vectorized op beats many
small interpreted ones** — a `range(0,N).reduce(0.0, (s,i) => s + work(i))` step-loop pays
interpreter dispatch per step, where the same computation expressed as array/tensor
broadcasts (or a single fused pipeline) stays on the typed path.

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

Fusion triggers for `≥2` map/filter stages, a stage feeding a `reduce`, or a filter
feeding a `count` (`xs.filter(g).count()` — counts with **zero allocation**, never
building the filtered array), over an idempotent `Int` array or a `range`; anything else
(float, a non-eligible body, a side-effecting source) falls through to the per-stage path. It is sound precisely because
eligible bodies are **pure** — eliminating intermediates is unobservable.

`map`/`filter`/`reduce` chains over 10M integers (debug build):

| Pipeline | fused | bytecode | speedup | peak RSS (fused vs bytecode) |
| --- | --- | --- | --- | --- |
| `range(N).map(f).filter(g).reduce(+)` | 0.08 s | 5.6 s | ~70× | **17 MB** vs 170 MB |
| `range(N).filter(g).count()` | 0.01 s | 2.7 s | ~270× | **17 MB** vs 131 MB |
| `xs.filter(g).map(f).count()` | 0.11 s | 2.8 s | ~25× | 134 MB vs 170 MB |

The scalar pipeline (`range → … → reduce`) **allocates nothing** — its 17 MB is just the
runtime — and runs at the bare reduce-loop's C/Go-class speed.

## Roadmap

- ~~captured numerics in bodies (`x => x*k`)~~ — **done** (single `map`; via a `caps`
  slice). Still to do: extend captures to the **fused** map→reduce path (a captured map
  feeding a `reduce` currently un-fuses and runs the map as a standalone kernel).
- ~~`f64` `map` kernels~~ — **done** (monomorphized over element type; `{+,-,*}` subset,
  `f64` captures). ~~mixed `Int`-source → `Float` `map`~~ — **done** (`range(N).map(j =>
  j*0.001)`; node-by-node typing, no captures). Still to do: `f64` *filters* and the wider
  float body (`/`, `if`, comparison, calls) once each divergence (float `/0` → raise,
  result-type, NaN-compare) is handled; the `f64`/mixed kernel in the **fused** pipeline;
  captures in the mixed kernel (needs compile-time capture types).
- SIMD lanes; horizontal fusion (several aggregates in one pass); statistical sinks
  (`.sum()`/`.mean()` with an incremental Neumaier compensator).

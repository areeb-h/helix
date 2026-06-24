# Caching, memory & core robustness

How Helix stays fast *and* correct *and* unsurprising. The governing rule, from
which everything else follows:

> **Immutability makes caching safe by construction.** A cached result of an
> immutable value can never go stale, because the value it was derived from can
> never change. The same property that makes Helix leak-free (no interior
> mutability ⇒ no `Rc` cycles) makes its caches trustworthy (no mutation ⇒ no
> invalidation problem). We only ever cache results keyed to immutable inputs, so
> there is nothing to invalidate.

This is why Helix can cache aggressively without the staleness bugs that plague
mutable systems — there is no "the data changed under the cache" case.

## What we cache (and how it stays safe)

1. **Structural sharing (always on).** Every collection is `Rc`-shared and
   copy-on-write. Binding, passing, and returning a value is an O(1) pointer copy,
   not a deep clone. This *is* a cache — of the value itself — and it's safe
   precisely because values are immutable. It's also why a 200-op run holds RSS
   flat (see [memory-safety](memory-safety.md)).

2. **`DataFrame.cache()` — explicit, eager, safe.** A lazy frame normally
   re-scans its source file on every materialization. `df.cache()` materializes
   it **once** into memory and re-wraps it as lazy, so every later query reuses
   the in-memory result:

   ```helix
   big = read_csv("huge.csv").cache()
   big.count()                  # reads the file once (here)
   big.where(age > 40).count()  # no re-scan — operates on memory
   ```

   It is **eager by design**: the cost is paid, visibly, at the `.cache()` call —
   no hidden recomputation, no background invalidation, no interior mutability.
   Because the result is an ordinary immutable value, it can never be stale.
   `.cache()` is a pure performance hint: results are identical with or without it
   (regression-tested, `dataframe_cache_is_transparent`). Use it when a base frame
   feeds several queries; skip it for a one-shot.

3. **Parquet metadata (free).** `read_parquet` is memory-mapped and `count()` is
   O(1) from file metadata — the format already caches what we need.

4. **JIT-compiled code (per run).** Each eligible function is compiled to native
   code once at startup and reused for the whole run; recursion never recompiles.

5. **Automatic memoization of pure overlapping-recursive functions — the
   "under the hood" cache.** This is the transparent one, and it's safe *only*
   because of a static analysis (`bytecode::memoizable_fns`) that admits a
   function **just** when all three hold:
   - **pure** — it never reaches `print`/`read_*`/`write_*` (transitively), so a
     cache hit can never skip a side effect;
   - **reads no mutable global** — its result is a function of its arguments
     alone (immutable globals like `pi` are fine; they never change);
   - **overlapping recursion** — ≥2 self-calls, the signature of exponential
     redundancy. Linear recursion (one self-call) is left on the JIT, where a fast
     native step beats a cached bytecode step.

   At runtime the VM additionally gates on **all-`Int` arguments** (float keys are
   excluded — NaN/precision make them unsafe hash keys) and **bounds the table**.
   The effect: `fib(35)` goes from ~30M calls to ~35 — instant — *and the result
   is identical*, because the function is provably a pure function of its inputs.
   It's observably transparent (only faster), needs no `.memoize()` annotation, and
   can't go stale because its inputs are immutable. This is also why Helix can beat
   C here: C can't *prove* `fib` is side-effect-free, so it daren't auto-memoize.

## What we deliberately do *not* cache

- **We don't memoize the unsafe cases** — impure functions, functions reading
  mutable globals, non-overlapping (linear) recursion, or float-keyed calls. The
  analysis above excludes each by construction; getting any of them wrong is
  exactly the stale-state/side-effect bug we refuse to ship.
- **No pointer-identity result cache.** Keying a cache on an `Rc`'s address is
  unsafe: once the `Rc` is dropped, a new allocation can reuse the address and
  return a stale hit (the ABA problem). We avoid identity caches entirely;
  `.cache()` stores the result *inside* the value instead, so the result and its
  key share a lifetime.
- **No cross-run persistent compile cache (yet).** Caching native code to disk
  across runs would need source-hash invalidation; deferred until it's clearly
  worth the complexity, because a wrong invalidation key *is* a staleness bug.

## Memory handling

The aim: process data far larger than RAM, with full precision.

- **Columnar + zero-copy (Arrow/Polars):** data lives in typed columnar buffers,
  not 24-byte boxed `Value`s, so numeric arrays avoid the interpreter's per-element
  overhead. (The scalar interpreter's `Value` is being shrunk separately — see
  [performance-roadmap](performance-roadmap.md) Track A.)
- **Lazy execution:** DataFrame verbs extend a query plan that materializes once,
  with predicate/projection pushdown — only the needed columns/rows are read.
- **Memory-mapped scans** (Parquet) and **streaming sinks** (`write_parquet`)
  keep peak memory bounded well below dataset size (50M-row write: 1.5 GB peak vs
  4.8 GB eager).
- **Accuracy under memory pressure:** aggregations use Neumaier compensated
  summation, so a large streamed `sum`/`mean` stays accurate to the last ulp — we
  never trade precision for throughput.
- **Local-first:** this is the bio/equity thesis — a 16-core desktop with mmap +
  SIMD + streaming beats a cluster running naive code, no GPU required.

## Core robustness: is the native-stack recursion limit still a problem?

The roadmap once listed "iterative/trampolined eval to remove the native-stack
recursion limit — *only if needed*." Here is the honest assessment of whether it's
needed, now that the VM exists:

- **The bytecode VM is already the iterative evaluator.** Helix function calls push
  frames onto a heap `Vec`, not the native stack — so recursion is bounded by
  memory, not stack size. The VM does **100 000-deep recursion on an ordinary
  thread stack** (test `deep_recursion_is_iterative`), with a 1 000 000-frame guard
  that turns true runaway recursion into a clean error. For everything the VM
  compiles, the native-stack limit is *already gone*.
- **The tree-walker (fallback) is guarded, never crashing.** Programs the VM can't
  yet compile run on the recursive tree-walker, on a 2 GiB thread with a 20 000-call
  depth guard — deep recursion works, and anything deeper is a graceful
  *"maximum recursion depth"* error, not a stack-overflow abort.

So the core is **already robust**: no input crashes it; deep recursion either runs
(VM, to a million frames) or errors cleanly (tree-walker, past 20 000). A standalone
trampoline of the tree-walker would be a large rewrite of a *fallback* path for the
narrow case of >20 000-deep recursion that also uses not-yet-compiled features
(arrays/methods) — and it would be **superseded automatically** as the VM widens,
since every feature moved into the VM inherits iterative recursion for free.

**Recommendation: don't build a separate trampoline.** Widen the VM (Stage 1b) —
that *is* the path to "no native-stack limit anywhere," and it pays for itself in
speed too. The trampoline stays "only if a real workload ever needs >20 000-deep
recursion through the fallback path," which none does today.

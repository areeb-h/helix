# Caching, memory, and core robustness

How Helix remains fast, correct, and predictable. The governing rule, from which
everything else follows:

> **Immutability makes caching safe by construction.** A cached result of an
> immutable value can never become stale, because the value it was derived from
> can never change. The same property that makes Helix leak-free (no interior
> mutability implies no `Rc` cycles) makes its caches trustworthy (no mutation
> implies no invalidation problem). Helix only caches results keyed to immutable
> inputs, so there is nothing to invalidate.

Helix can therefore cache aggressively without the staleness defects that affect
mutable systems; there is no case in which the data changes underneath the cache.

## What is cached (and how it stays safe)

1. **Structural sharing (always on).** Every collection is `Rc`-shared and
   copy-on-write. Binding, passing, and returning a value is an O(1) pointer copy
   rather than a deep clone. This is itself a cache — of the value — and it is safe
   precisely because values are immutable. It is also why a 200-operation run holds
   RSS flat (see [memory-safety](memory-safety.md)).

2. **`DataFrame.cache()` — explicit, eager, safe.** On the default (Polars)
   backend, a lazy frame normally re-scans its source file on every
   materialization. `df.cache()` materializes it **once** into memory and
   re-wraps it as lazy, so every subsequent query reuses the in-memory result:

   ```helix
   big = read_csv("huge.csv").cache()
   big.count()                  # reads the file once (here)
   big.where(@age > 40).count() # no re-scan — operates on memory
   ```

   It is **eager by design**: the cost is paid, visibly, at the `.cache()` call,
   with no hidden recomputation, no background invalidation, and no interior
   mutability. Because the result is an ordinary immutable value, it can never be
   stale. `.cache()` is a pure performance hint: results are identical with or
   without it (regression-tested, `dataframe_cache_is_transparent`). Use it when a
   base frame feeds several queries; omit it for a single use.

   On the native engine (`native-df`, the appliance backend), column storage is
   already `Rc`-shared with per-column memoized decode, so `cache()` is an `Rc`
   clone — a refcount bump, an identity operation
   (`cache_is_identity_and_count_is_free`, `src/backend/native/tests.rs`). The
   same call is cheap there because the memoization the polars backend buys
   eagerly is built into the native frame's storage.

3. **Parquet metadata (free).** `read_parquet` is memory-mapped and `count()` is
   O(1) from file metadata; the format already caches what is required. The
   native reader takes this further: columns decode lazily and are memoized
   per-column, so `count()` and `cache()` read the footer without touching a
   data page.

4. **JIT-compiled code (per run).** Each eligible function is compiled to native
   code once at startup and reused for the entire run; recursion never recompiles.

5. **Automatic memoization of pure overlapping-recursive functions — the
   transparent cache.** This is safe *only* because of a static analysis
   (`bytecode::memoizable_fns`) that admits a function **only** when all three
   conditions hold:
   - **pure** — it never reaches `print`/`read_*`/`write_*` (transitively), so a
     cache hit can never skip a side effect;
   - **reads no mutable global** — its result is a function of its arguments
     alone (immutable globals such as `pi` are permitted, as they never change);
   - **overlapping recursion** — ≥2 self-calls, the signature of exponential
     redundancy. Linear recursion (one self-call) is left on the JIT, where a fast
     native step outperforms a cached bytecode step.

   At runtime the VM additionally gates on **all-`Int` arguments** (float keys are
   excluded, since NaN and precision make them unsafe hash keys) and **bounds the
   table**: an entry cap (`MEMO_MAX_ENTRIES`) that, on overflow, **evicts** (clears
   and lets the table rebuild) rather than growing without limit — so memoizing over a
   very large or unbounded key space stays memory-bounded instead of climbing to OOM
   (2026-07 hardening round, see [audit.md](audit.md)). The effect: `fib(35)` is
   reduced from ~30M calls to ~35, *with an identical result*, because the function is
   provably a pure function of its inputs. It is observably transparent (only faster), requires no `.memoize()`
   annotation, and cannot become stale because its inputs are immutable. This is
   also why Helix can outperform C here: C cannot *prove* `fib` is side-effect-free,
   so it cannot auto-memoize.

## What is deliberately *not* cached

- **The unsafe cases are not memoized** — impure functions, functions reading
  mutable globals, non-overlapping (linear) recursion, and float-keyed calls. The
  analysis above excludes each by construction; an error in any of these would be
  precisely the stale-state or side-effect defect Helix avoids shipping.
- **No pointer-identity result cache.** Keying a cache on an `Rc`'s address is
  unsafe: once the `Rc` is dropped, a new allocation can reuse the address and
  return a stale hit (the ABA problem). Helix avoids identity caches entirely;
  `.cache()` stores the result *inside* the value instead, so the result and its
  key share a lifetime.
- **No cross-run persistent compile cache (yet).** Caching native code to disk
  across runs would require source-hash invalidation; this is deferred until it is
  clearly worth the complexity, because a wrong invalidation key constitutes a
  staleness defect.

## Memory handling

The objective is to process data far larger than RAM, with full precision.

- **Columnar and zero-copy:** data resides in typed columnar buffers rather than
  24-byte boxed `Value`s, so numeric arrays avoid the interpreter's per-element
  overhead — Arrow/Polars on the default backend; the native engine
  (`native-df`) keeps its own typed columns, dictionary-encoded for strings.
  (The scalar interpreter's `Value` is being reduced separately; see
  [performance-roadmap](performance-roadmap.md) Track A.)
- **Lazy execution:** DataFrame verbs extend a query plan that materializes once,
  with predicate and projection pushdown, so only the required columns and rows are
  read.
- **Memory-mapped scans** (Parquet) and **streaming sinks** (`write_parquet`)
  keep peak memory bounded well below dataset size (50M-row write: 1.5 GB peak
  versus 4.8 GB eager).
- **Accuracy under memory pressure:** aggregations use Neumaier compensated
  summation, so a large streamed `sum`/`mean` remains accurate to the last ulp;
  precision is not traded for throughput.
- **Local-first:** this is the bio/equity thesis — a 16-core desktop with mmap,
  SIMD, and streaming outperforms a cluster running naive code, with no GPU
  required.

## Core robustness: the native-stack recursion limit

The roadmap previously listed "iterative/trampolined eval to remove the
native-stack recursion limit — *only if needed*." The following is an assessment of
whether it is needed now that the VM exists:

- **The bytecode VM is already the iterative evaluator.** Helix function calls push
  frames onto a heap `Vec` rather than the native stack, so recursion is bounded by
  memory rather than stack size. The VM performs **100 000-deep tail recursion on an
  ordinary thread stack** (test `deep_recursion_is_iterative`; tail calls reuse
  their frame, so they run at constant depth), and the shared 20 000-frame
  `MAX_CALL_DEPTH` guard converts true runaway non-tail recursion into a clean
  error. For everything the VM compiles, the native-stack limit is *already
  eliminated*.
- **The tree-walker (fallback) is guarded and does not crash.** Programs routed to
  the recursive tree-walker (REPL, `HELIX_NOVM`, the rare fallback) run on an
  on-demand big-stack thread — 128 MiB in release, 1 GiB in debug,
  `HELIX_STACK_MB` to override — with the same 20 000-call depth guard; deep
  recursion works, and anything deeper produces a graceful *"maximum recursion
  depth"* error rather than a stack-overflow abort.

The core is therefore **already robust**: no input crashes it; deep tail recursion
runs in constant space on every engine, and non-tail recursion beyond the shared
20 000-frame guard errors cleanly on every engine (identical text,
`recursion_depth_is_aligned_across_engines`). A standalone trampoline of the
tree-walker would be a large rewrite of a
*fallback* path for the narrow case of >20 000-deep recursion that also uses
not-yet-compiled features (arrays/methods), and it would be **superseded
automatically** as the VM widens, since every feature moved into the VM inherits
iterative recursion.

**Recommendation: do not build a separate trampoline.** Widen the VM (Stage 1b),
which is the path to eliminating the native-stack limit everywhere and also
improves performance. The trampoline remains warranted only if a real workload ever
requires >20 000-deep recursion through the fallback path, which none does today.

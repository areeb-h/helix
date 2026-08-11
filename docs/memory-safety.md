# Memory safety and leak-freedom

Helix is written in safe Rust, and the interpreter is **provably leak-free**. This
note records the argument and the tests that support it.

## The guarantee, by construction

In safe Rust there are exactly two ways to leak memory:

1. **`Rc`/`Arc` reference cycles**, and
2. **explicit leaks** (`mem::forget`, `Box::leak`, `Rc::into_raw` without
   `from_raw`, `ManuallyDrop`, …).

An audit of `src/` finds **none of the second kind** in the interpreter/VM core.

> **Corrected 2026-08-11.** This paragraph previously claimed "the **single** `unsafe`
> block in the codebase is `jit::call_native`". That was wrong twice over: `call_native`
> does not exist anywhere in `src/`, and the count is not one. Measured across tracked
> non-test Rust sources: **97 `unsafe { … }` blocks and 38 `unsafe fn` declarations, in 8
> files.** The claim was written when the JIT had a single entry point and was never
> re-measured as the FFI surface grew. It is stated correctly below, because a
> memory-safety document that cannot be checked is worse than none.

`unsafe` is concentrated where crossing to native code requires it, and is absent from the
language core:

| file | `unsafe {}` | `unsafe fn` | why |
|---|---|---|---|
| `src/jit/ffi.rs` | 35 | 35 | calling Cranelift-generated kernels; every runner is an `unsafe fn` with its ABI contract documented at the site |
| `src/vm.rs` | 43 | 0 | the dispatch arms that invoke those runners |
| `src/simd.rs` | 6 | 3 | explicit vector intrinsics |
| `src/interp/methods.rs`, `src/pkg.rs`, `src/main.rs`, `src/serve.rs`, `src/render.rs` | 13 | 0 | allocator setup, mmap-backed file reads, and platform calls |

The safety argument is unchanged in substance and is the one that matters: calling
JIT-generated machine code is inherently unsafe, so it is confined to typed runner
functions guarded by the VM's runtime representation check, so the native ABI contract
(`extern "C" fn(*const i64, …)`) always holds. Those calls deal in packed `i64`/`f64`
buffers — no heap graph, no `Rc` — so they add **no leak surface**, and the JIT's owning
module outlives every call. The parser, type checker, interpreter and value model contain
no `unsafe`,
`forget`, `leak`, `into_raw`, and `ManuallyDrop`.

The first kind is **structurally impossible** here. Constructing an `Rc` cycle
requires *interior mutability* (a `RefCell`, `Cell`, or `Mutex` to install a
back-reference after construction). The audit finds **no interior mutability
anywhere** in `src/`. Helix's runtime values (`Value::Array`, `Tuple`, `Record`,
`Function`, `Tensor`, `DataFrame`, etc.) only ever hold `Rc`s to values that
already exist, and collections are immutable (`Rc<Vec<Value>>`, never
`Rc<RefCell<…>>`). The value graph is therefore a **DAG**, and every allocation
is freed deterministically when its last `Rc` is dropped.

## The guarantee, by test

Two tests in `src/interp.rs` support this empirically:

- **`no_reference_leaks`** — the test helper drops the `Interp` *before* returning
  a result, so if that result's allocation has `Rc::strong_count == 1`, the
  environment and every intermediate provably released their references. This is
  checked across 10 value-producing paths (bindings, recursion, comprehensions,
  `let`, destructuring, records, tuples, interpolation, `zip` with
  parameter-destructure, slicing). No `unsafe` and no external tools are required;
  `strong_count` is the ground truth.

- **Empirical RSS stability** (manual, via `scripts`): running 10 versus **200**
  allocation-heavy operations (each building ~200k-element arrays and discarding
  them) holds peak RSS **flat at ~10 MB**. A genuine leak would grow RSS by
  approximately 20×.

## Robustness: recursion depth

The interpreter is a tree-walker, so each Helix function call recurses on the
native stack (~25 KB per call in debug builds). Two measures keep this safe:

1. **`main` runs the interpreter on a dedicated 2 GiB-stack thread**, so deep but
   bounded recursion (e.g. `sum(15000)`) succeeds instead of overflowing the
   default 8 MB main stack.
2. **`MAX_CALL_DEPTH` (20 000)** converts runaway or excessively deep recursion
   into a clean Helix error — *"maximum recursion depth exceeded"* — well before the
   2 GiB stack could be exhausted, rather than an uncatchable stack-overflow abort.

`deep_recursion_is_safe` regression-tests both on the same large-stack thread.

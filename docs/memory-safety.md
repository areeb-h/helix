# Memory safety and leak-freedom

Helix is written in safe Rust, and the interpreter is **provably leak-free**. This
note records the argument and the tests that support it.

## The guarantee, by construction

In safe Rust there are exactly two ways to leak memory:

1. **`Rc`/`Arc` reference cycles**, and
2. **explicit leaks** (`mem::forget`, `Box::leak`, `Rc::into_raw` without
   `from_raw`, `ManuallyDrop`, …).

An audit of `src/` finds **none of the second kind** in the interpreter/VM core.
The **single** `unsafe` block in the codebase is `jit::call_native`; calling
Cranelift-generated machine code is inherently unsafe. It is confined to one
function, guarded by the VM's all-`Int`/correct-arity check so the native ABI
contract (`extern "C" fn(i64,…)->i64`) always holds, and deals only in `i64`
values — no heap, no `Rc` — so it adds **no leak surface**. The JIT's owning
module outlives every call. Everything else remains free of `unsafe`,
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

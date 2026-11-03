# Memory safety and leak-freedom

Helix is written in safe Rust, and the interpreter is designed to be **leak-free**. This
note records the argument, where it depends on a runtime check rather than on structure,
and the tests that support it.

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
> memory-safety document that cannot be checked is worse than none. (Re-measured
> 2026-08-23: unchanged — 97 blocks, 38 `unsafe fn`, 8 files.)

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

The first kind needs *interior mutability* — a `RefCell`, `Cell`, or `Mutex` through which
a value can come to hold a reference to something that already holds it.

> **Corrected 2026-11-03.** This section used to say the audit "finds **no interior
> mutability anywhere** in `src/`" and that collections are "never `Rc<RefCell<…>>`", so the
> value graph was a DAG by construction. Both stopped being true when the accumulation
> fixes landed (ADR 0029): two collections now keep values in a shared, mutable buffer, and
> a program could build a cycle through either — and leaked when it did. Measured on
> `54cc3eb`, peak RSS for 100k → 400k calls that each build and drop one such value:
> `d.insert("self", d)` 103 → 375 MB; the same through a record 111 → 407 MB, through a
> closure 112 → 412 MB; two dicts holding each other's older views 190 → 724 MB; an array of
> strings appended to itself 41 → 125 MB — on all three engines, every control flat at
> 19 MB. The argument below is the corrected one.

**Where interior mutability is, and which of it can hold a Helix value.** Measured across
non-test sources, `RefCell`/`Cell`/`OnceCell` appear in `value.rs`, `serve.rs`, `pg/`,
`autodiff.rs`, `cookiejar.rs`, `filelock.rs`, `symbol.rs`, `regexes.rs`, `fold/`,
`jit/ffi.rs`, `bytecode.rs`, `bytecode/ops.rs`, `backend/native/` and
`interp/builtins/net.rs` (which builds one of `serve.rs`'s). Only `value.rs` puts a **`Value`**
behind one; every other cell holds bytes, strings, numbers, OS handles or decoded columns,
none of which can reference a Helix value, so none can close a cycle. In `value.rs` there are
exactly two such places:

- a **dict's log** (`Dict::adds`, plus the merged-view cache derived from it), which every
  view of one dict chain shares, so `insert`/`remove` append instead of copying;
- a **shared array's buffer** (`ArrayData::Shared`), which every view of one array shares,
  so `concat` appends instead of copying.

**The invariant: neither buffer ever holds a value that can reach it.** It is enforced at the
only two places a value enters an *existing* buffer — the tip append in `dict_append` and the
newest-view append in `concat_shared`. Every other path (a branch, a compaction, a seed)
starts a brand-new buffer, which nothing can yet reference. At those two sites,
`value::reaches` asks whether the incoming value — through records, tuples, arrays, dicts,
and the captured values of closures — holds a view of the target buffer. If it might, the
append takes the copying path instead: the same answer, with the old buffer's view stored in a
new buffer the old one cannot reach. Two details make this sound rather than approximate:

- the scan covers each buffer **whole**, not a view's prefix — the `Rc` keeps every entry
  alive, so an entry written through a newer view closes a cycle just as well;
- it is **budgeted** (1,024 values), and running out answers "may cycle", which only costs a
  copy. Each buffer also carries a flag, kept on every append, saying whether it could hold a
  view at all, so a dict of growing arrays or an array of plain records is skipped in O(1).

With that invariant, the value graph is again a DAG, and every allocation is freed
deterministically when its last `Rc` is dropped. `a_value_holding_its_own_buffer_is_freed`
pins the five shapes above on all three engines (it fails on `54cc3eb` at +124 MB), and
`tests/corpus/no_rc_cycles` pins that refusing the append never changes an answer.

## The guarantee, by test

Two tests in `src/interp/tests.rs` support this empirically:

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
native stack (~1 KiB per call in release builds, ~25 KiB in debug). The default
engine is the bytecode VM, which keeps its call frames on the *heap*, so this
section concerns only the tree-walker paths (the REPL, `HELIX_NOVM`, and the
rare compile-fallback). Two measures keep them safe:

1. **Tree-walker paths run on an on-demand big-stack thread** (`run_on_big_stack`,
   sized by `serve::eval_stack_size`: 128 MiB in release, 1 GiB in debug,
   `HELIX_STACK_MB` to override), so deep but bounded recursion (e.g. `sum(15000)`)
   succeeds instead of overflowing the default 8 MB main stack.
2. **`MAX_CALL_DEPTH` (20 000)** converts runaway or excessively deep recursion
   into a clean Helix error — *"maximum recursion depth exceeded"* — well before the
   stack could be exhausted (20 000 release frames touch ~19 MiB against 128 MiB),
   rather than an uncatchable stack-overflow abort. The VM shares the same limit,
   so every engine refuses at the same depth with the same text.

`deep_recursion_is_safe` regression-tests both on the same large-stack thread.

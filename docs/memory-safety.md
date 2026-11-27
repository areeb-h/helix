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
   into a clean Helix error — *"maximum recursion depth exceeded"* — rather than an
   uncatchable stack-overflow abort. The VM shares the same limit, so every engine
   refuses at the same depth with the same text.
3. **The walker grows its stack on demand** (`STACK_RED_ZONE` in `src/interp.rs`,
   via `stacker`): every call checks the native stack it has left, and below 1 MiB
   (16 MiB in debug) continues on a fresh segment.

   Until 2026-11-03 the second measure claimed more than it did. This section said
   20 000 frames "touch ~19 MiB against 128 MiB"; measured, one Helix call costs the
   walker 3.8 KB to 13 KB of native stack depending on the function's shape, so
   `n + f(n - 1)` needed 108 MiB and a record literal, an array, a `map` callback or
   a few nested parentheses around the recursive call needed 143 to 247 MiB. Those
   died of SIGABRT short of the limit while the VM answered — an engine divergence
   and an abort — and the debug walker aborted `examples/language/recursion.helix`.
   The depth guard bounds how many calls happen, not how much stack each one takes,
   so it could never have been the whole answer.

`deep_recursion_is_safe` regression-tests both on the same large-stack thread.

## Robustness: value depth

Call depth is bounded; **value** depth is not. A program can nest a value as deep as its
memory allows — a tail-recursive loop wrapping an accumulator (`build(n - 1, [acc])`) makes a
million levels in under a second, on every engine, without a single deep call. Until
2026-11-24 almost everything the runtime did to such a value recursed once per level on the
native stack: printing it, `==`, `to_json`, `unique`, sorting tuples, arithmetic over nested
arrays, rendering a table — 87 operations by the first count — and, last, **freeing** it,
which is compiler-generated drop glue. All of them aborted the process (`fatal runtime
error: stack overflow`), on all three engines. `src/deep.rs` holds the three measures:

1. **Walks borrow stack as they deepen.** Every recursive walk over a value (display,
   `eq3`/`values_equal`, tuple order, sort keys, JSON, broadcasting, table rendering, the
   tensor shape) steps into a child through `deep::nested`, which continues on a fresh
   heap-allocated segment below the same red zone the walker uses for calls. A walk's stack
   is proportional to the depth it has reached and comes from the heap, like the value: a
   program that could build the value can walk it.
2. **Freeing is counted, on the payloads.** A container's payload — `Items` (a tuple's),
   `Fields` (a record's), `ArrayData`, `Dict`, `ClosureData`, `FuncVal` — has a `Drop` that
   runs only when its last reference goes. It moves its contents out and frees them itself,
   counting the frees in progress on this thread; up to 512 that is ordinary recursion,
   exactly what the glue did, and past it the contents are parked on a per-thread list that
   the outermost free empties from the top of the stack. So no free uses more than ~512
   levels of stack, whatever the depth. Because the hook is on the payload, dropping a scalar
   or a reference to a shared container never reaches it (an earlier version hooked `Drop` on
   `Value` itself and cost +2.9% of instructions on the examples; this one costs +0.2%).

Two cost bugs hid behind the overflow and are fixed with it, because a stack fix alone would
have traded a crash for a hang: printing copied each level's text into the level above it
(O(d²); now one buffer), and `tensor` measured a nested array's first row twice per level
(O(2^d); now once, and at most 64 axes). Tuple `<` compared each pair in full before
descending into it (O(d²); now one pass).

3. **Walks answer once per distinct container.** Depth is one way a value outgrows a naive
   walk; SHARING is the other. `[x, x]` wrapped sixty times is sixty-one arrays and 2^60
   leaves, and a value is a DAG, not a tree. Until 2026-11-26 comparisons of such values
   never finished and anything that built a tree from one — `v + 1`, the sort-key shape
   check, a table cell's text, `tensor` — ran out of memory and killed the process.
   `deep::once` remembers what a walk has decided about each container (keyed by address,
   with the operands held so no address is reused): equality and order answer once per
   distinct PAIR, elementwise arithmetic returns one result per distinct operand (so its
   output shares wherever its input did), sort-key shapes are interned, and the text and
   JSON writers copy a container's text from where it already lies in the buffer. An answer
   is remembered only when reaching it took 4,096 container steps, and only for a container
   something besides its parent holds — the only kind a walk can meet twice — so data that
   does not share never enters the memo. `remembering_shared_containers_changes_no_answer`
   (src/deep.rs) runs every memoized walk over 600 randomly shared values with the memo
   forced on and forced off, and requires identical answers.

**What is still proportional to the tree:** only output that IS the tree. A shared value's
text is as long as its leaves, so printing or serializing one past 1 GiB is the one-string
cap's error (in about a second); `tensor` of one past a billion elements is refused. A dict
insert's cycle check is budgeted per insert, which makes BUILDING a hundred thousand levels
of dict-in-dict take seconds — slow, and bounded.

`values_nested_100k_deep_print_compare_serialize_and_free` and
`values_that_share_a_part_cost_what_they_are_not_the_paths_through_them` (tests/cli.rs) and the
unit tests in `src/deep.rs` pin this; `scripts/deepvalues.py` (docs/testing.md) runs the full matrix in CI.

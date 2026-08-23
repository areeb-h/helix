# Execution engine

Helix provides **three execution engines** behind one surface, selected per program:

```
lex → parse → type-check ─┬─ bytecode::compile ─ Ok  ─┬─ jit::build ─ Some → native kernels
                          │                           └─ jit::build ─ None → vm::run
                          └─ bytecode::compile ─ Err ────────────────────── → Interp::run
```

`run_source` compiles to bytecode; the compiler is total over type-checked programs
(see [The compiler is total](#the-compiler-is-total)), so the VM is the sole automatic
engine and the `Err` edge above is a defensive backstop, not a routine path. Within the
VM, eligible numeric `map`/`filter`/`reduce`/`scan` bodies and tail-recursive functions
are compiled to native code by the Cranelift JIT and dispatched at run time; anything
ineligible falls through to the bytecode loop, always, and never to a wrong answer.
Since v0.4.0 the JIT is a cargo feature (`jit`, in the default set); a build without it
runs the identical bytecode on the VM.

Two environment variables select an engine explicitly, and the whole correctness story
rests on them:

| variable | engine |
| --- | --- |
| *(none)* | JIT where eligible, VM otherwise |
| `HELIX_NOJIT=1` | bytecode VM only |
| `HELIX_NOVM=1` | tree-walking interpreter |

All three must produce **byte-identical** output — values *and* error text — for every
program. That is enforced in CI, not asserted here: see [Correctness gates](#correctness-gates).

> **This document was written when there were two engines and has been corrected rather
> than rewritten.** Sections below that reason about "the two engines" are describing the
> VM-vs-tree-walker relationship, which is unchanged and still the foundation the JIT sits
> on. Where a section predates the JIT entirely it says so.

## Rationale for two engines

The tree-walker (`src/interp.rs`) serves as the reference implementation:
correct, complete, and straightforward to extend. It is approximately 100×
slower than the delegated data path for scalar and control-flow code, because it
re-traverses the AST on every step, dispatches through large `match` expressions,
and hashes a `String` in an `FxHashMap` on every variable access, with
insert/remove churn on every call.

The bytecode VM (`src/bytecode.rs` and `src/vm.rs`) eliminates those structural
costs without sacrificing the tree-walker's correctness, because it **reuses the
tree-walker's value type and semantic helpers** (`eval_binary`, `eval_unary`,
`as_bool`, `tri`, `call_builtin`). The compiler and VM do not reimplement
arithmetic, broadcasting, three-valued logic, or builtins; they change only
*how the program is sequenced*, not *what each operation means*. The two engines
are therefore observationally identical by construction.

## Bytecode VM design

The VM is stack-based (as in Wasm and the JVM), chosen for a simple, correct
first iteration that a JIT can later consume.

- **Slot-resolved variables.** A resolver pass in the compiler maps every
  function parameter and `let` binding to an integer slot index, and every
  top-level binding to a global slot. Runtime variable access is an array index
  rather than a hash lookup.
- **Heap call stack.** A call pushes a `Frame` onto a `Vec`; `Return` pops it.
  Recursion therefore resides on the heap, bounded by memory rather than the
  native stack: the VM performs 100 000-deep tail recursion on an ordinary
  stack. `VM_MAX_DEPTH` is the same shared `MAX_CALL_DEPTH` (20 000) the
  tree-walker uses, so every engine refuses runaway non-tail recursion with the
  identical error (`recursion_depth_is_aligned_across_engines`); tail calls
  reuse their frame and never count against it.
- **Whole-program fallback.** The compiler operates on an all-or-nothing basis per
  program: the first unsupported node causes `compile` to return `Unsupported`,
  and the tree-walker takes over. This keeps the two engines cleanly separated,
  avoiding a hybrid in which half a program runs in each. **The objective is to
  retire this fallback entirely**; see "Collapse to one engine" below.

### What compiles today

Literals, identifiers, unary/binary arithmetic, comparison & equality,
three-valued `and`/`or`/`??` (short-circuit), `if`/`then`/`else`, `let … in`,
user-function calls and recursion, builtin calls (`print`, math functions, etc.);
arrays, tuples, records and field access, indexing and slicing, string
interpolation, destructuring assignment, value-methods; **all comprehensions** —
`map`/`filter`/`where`/`reduce`/`any`/`all`, including **multi-binder patterns**
(`(a, b) => …`) and fused `range(...).reduce(...)` (which additionally
JIT-compiles to a native loop at speeds comparable to C and Go). In practice the
VM runs essentially every program; the tree-walker is reached only for the narrow
set of constructs described below.

### The compiler is total

`compile` no longer returns `Unsupported` for **any** type-checked program: every
construct either compiles to bytecode or, for a statically-known error (immutable
reassignment, a malformed comprehension or `reduce`), emits an `Op::Raise` that
fires the canonical error at runtime, matching the tree-walker's exact wording,
after the receiver's side effects, so behaviour is identical. `run_source`
therefore has **no automatic tree-walker fallback**: the VM is the sole automatic
engine. The tree-walker is reached only via explicit `HELIX_NOVM` (A/B
benchmarking and the differential oracle) and the interactive REPL.

Two features recently moved onto the VM:
- **First-class functions** — a standalone lambda or a bare function name becomes a
  `Value::VmFunc` (a reference to a compiled chunk; no captured environment, since
  the type checker rejects local capture and free variables resolve to shared
  globals), and a value-bound call dispatches through `CallValue`.
- **DataFrame/Tensor column-verbs** — via **type-directed compilation**: the type
  checker records each method receiver's inferred type in a `TypeMap` (keyed by node
  address, stable between check and compile), which the compiler uses to route a
  `Method` by the receiver's *true* type. A DataFrame receiver maps to
  `DfColumnVerb` (unevaluated column/predicate ASTs; bare names resolve against
  columns, then locals, then globals); a GroupBy maps to `GroupByAgg`; a known
  Array/Tensor maps to the value-method path (so `tensor.min(axis)` no longer falls
  back). This is the only sound disambiguation, because `where`/`sort`/`min` have
  distinct meanings per receiver and column arguments cannot be compiled as values.

### Tail-call optimization (constant-space recursion)

The heap call stack bounds recursion by *memory* rather than the native stack, but a
genuinely unbounded tail-recursive function — the idiomatic shape for a long-running
loop (a server's `accept → serve → accept` loop, an event loop, an accumulating fold)
— still pushed one `Frame` per call and grew the frame `Vec` until OOM. That makes the
natural "run forever in constant space" program a slow memory leak.

The VM therefore performs **tail-call optimization**. A `CallFn` in **tail position**
(its result is returned directly, possibly through intervening `Jump`s from `if`/block
arms) is rewritten to `Op::TailCallFn`, which **reuses the current frame** — it writes
the new arguments into the existing slots and jumps to the function's entry instead of
pushing a new frame. Tail recursion then runs in **genuinely constant stack space**.

- **Where:** a `tco_peephole` pass in `src/bytecode.rs` rewrites eligible `CallFn`s
  (run during `compile_func` and `compile_lambda`); the `Op::TailCallFn { idx, nargs }`
  handler in `src/vm.rs` does the frame reuse.
- **Consequence:** the cooperative event-loop server
  ([ADR 0022](adr/0022-http-version-roadmap.md)) loops tail-recursively and holds at a
  flat 16 MB indefinitely; before TCO the same loop grew without bound.
- **Semantics unchanged:** TCO only elides a frame; it never changes a result. It does
  turn one previously-diverging program from "grows memory then OOMs" into a genuine
  infinite loop — an *unbounded* tail recursion with no base case (`fn f(n) = f(n+1)`)
  now spins forever in constant space rather than eventually crashing, which is the
  correct behaviour for an intentional loop and the standard TCO trade-off. (Test
  fixtures that relied on such a function *erroring* were changed to a non-tail form,
  `1 + f(n+1)`, which still hits the depth guard.)
- **Parity:** reconciled across engines by `tail_calls_match_tree_walker_on_vm` and
  `recursion_depth_is_aligned_across_engines`; the tree-walker reaches the same
  results (it frame-reuses tail calls through its own trampoline; non-tail recursion
  is guarded at the shared `MAX_CALL_DEPTH`).

### Collapse to one engine

The VM is the engine for every valid, common program. The tree-walker survives
**only** as (a) the differential-fuzzer oracle, (b) a library of shared helpers
(`eval_binary`, `pattern_parts`, `call_method`, `df_column_verb`, etc.), and (c) a
**rare, lazily-threaded fallback** for a narrow residue (column-verbs and
aggregations on an `Unknown`-typed receiver, and a small number of
malformed-construct error messages).

Phases (all complete): multi-binder comprehensions; first-class functions;
column-verbs (type-directed); error-cases (immutable reassignment now raises on
the VM); and a gate test (`examples_compile_on_the_vm`) asserting that every
shipped example compiles to bytecode and never falls back.

**The big-stack thread is no longer on the default path.** The VM recurses on
the heap (frames in a `Vec`), so it runs on the ordinary main-thread stack and
performs 100 000-deep tail recursion without a special stack. Only the tree-walker
recurses on the native stack, and it is now reached only for the REPL,
`HELIX_NOVM`, or the rare fallback; those paths spawn a big-stack thread **on
demand** (`run_on_big_stack`, or a scoped thread in `run_source`), sized by
`serve::eval_stack_size`: 128 MiB in release, 1 GiB in debug, `HELIX_STACK_MB` to
override — shared with `listen` shard workers so the primary and its shards can
never diverge on recursion depth. A normal `helix script.helix` therefore no
longer reserves a multi-GiB stack in advance.

## Correctness gates

The differential oracle is the load-bearing idea of the whole project: a JIT is thousands
of lines of code generation standing between a program and its answer, so two simpler
implementations run beside it and all three must agree. Five gates enforce that, and a
sixth covers the ground none of them can reach.

- **Unit parity tests** (`src/vm.rs`): a battery of programs is run on the engines and
  their results compared (`parity_scalar_and_control_flow`,
  `parity_functions_and_recursion`). `deep_recursion_is_iterative` proves the heap-frame
  design; `errors_propagate` proves runtime errors still fire.
- **All-examples diff** (`scripts/vmparity.sh`): every runnable example
  (`examples/{language,numerics,dataframes,statistics,bio}`) must produce
  byte-identical output on the default engine and under `HELIX_NOVM=1` —
  DataFrame examples included, since column-verbs compile to bytecode.
- **The pinned corpus** (`tests/corpus/`): each program is run on all three engines and
  its output compared against a checked-in `.expected` file, so a change that alters
  behaviour identically on all three — which parity alone would accept — still fails.
- **Executed documentation** (`doc_examples_run_and_agree_on_all_three_engines`,
  `tests/cli.rs`): every `>>>` example in a `##` doc comment is extracted, run on all
  three engines, and compared against the output written beside it. A documented example
  that has drifted fails the build. See [Comments & doc-tests](comments-and-docs.md).
- **Differential fuzzing** (`scripts/opfuzz.py`): operators × operand shapes × compilation
  shapes, checked for byte-identical agreement. Engagement matters as much as agreement —
  a fuzzer passes trivially if the JIT silently declined, so tests assert
  `native_call_count() > 0` before trusting a comparison.

Every one of those gates **runs** its programs, which is a blind spot: a Helix program that
needs a generated fixture before it will start cannot be in any of them. That is not
hypothetical — nine `bench/` programs still called `io.read_csv` / `bio.read_fasta` /
`stats.correlation` long after ADR-0017 flattened those namespaces, and the thing that
finally noticed was `v0.1.0`'s release pipeline dying in its PGO training step. So:

- **Whole-tree type-check** (`scripts/checkall.sh`): `helix check` over every tracked
  `.helix` outside `tests/corpus/` — 86 programs in ~30 ms, because type-checking needs no
  fixture, no data and no network. It proves only that they compile, which is exactly the
  property the running gates could not cover. `tests/corpus/` is excluded on purpose: a
  dozen of those files are negative fixtures that must *not* compile, and their exact error
  text is already pinned on all three engines.

**This is not decorative.** Defects the oracle has caught, each found because one engine
answered differently: a signed-zero comparison that made a packed `min`/`max` disagree
with the boxed path; an integer subexpression that wrapped in the interpreter and did not
in a monomorphic f64 kernel; a malformed comprehension that errored on two engines and
silently returned `missing` on the third, laundered through `try` into an ordinary boolean
where no error text could reveal it.

## Measurements

`fib(30)` (pure scalar recursion, ~2.7M calls), debug build:
**tree-walker 3.86 s, bytecode VM 1.31 s (~3× faster).** The gap widens in release
builds and on call-heavy code. This engine is the foundation the JIT now sits on.

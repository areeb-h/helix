# Execution engine

Helix has **two execution engines** behind one surface, chosen per program:

```
lex → parse → type-check ─┬─ bytecode::compile ─ Ok  → vm::run        (fast path)
                          └─ bytecode::compile ─ Err → Interp::run    (tree-walker)
```

`run_source` tries the bytecode compiler first. If it returns `Unsupported`
(some construct isn't compiled yet), the program runs on the tree-walker
instead. `HELIX_NOVM=1` forces the tree-walker (for A/B comparison).

## Why two engines

The tree-walker (`src/interp.rs`) is the reference: correct, complete, easy to
extend. But it is ~100× slower than the delegated data path for scalar /
control-flow code — it re-traverses the AST on every step, dispatches through
large `match`es, and hashes a `String` in a `FxHashMap` on every variable
access (with insert/remove churn on every call).

The bytecode VM (`src/bytecode.rs` + `src/vm.rs`) removes those structural costs
without giving up the tree-walker's correctness, because it **reuses the
tree-walker's value type and semantic helpers** (`eval_binary`, `eval_unary`,
`as_bool`, `tri`, `call_builtin`). The compiler and VM never reimplement
arithmetic, broadcasting, three-valued logic, or builtins — they only change
*how the program is sequenced*, not *what each operation means*. So the two
engines are observationally identical by construction.

## The bytecode VM design

Stack-based (like Wasm/JVM), chosen for a simple, correct first iteration that a
JIT can later consume.

- **Slot-resolved variables.** A resolver pass in the compiler maps every
  function parameter and `let` binding to an integer slot index, and every
  top-level binding to a global slot. Runtime variable access is an array index,
  not a hash lookup.
- **Heap call stack.** A call pushes a `Frame` onto a `Vec`; `Return` pops it.
  Recursion therefore lives on the heap, bounded by memory rather than the native
  stack. This is the *proper* fix to the depth limit the tree-walker needs a
  2 GiB thread for — the VM does 100 000-deep recursion on an ordinary stack. A
  high `VM_MAX_DEPTH` still turns genuine runaway recursion into a clean error.
- **Whole-program fallback.** The compiler is all-or-nothing per program: the
  first unsupported node makes `compile` return `Unsupported` and the tree-walker
  takes over. This keeps the two engines cleanly separated — no fragile hybrid
  where half a program runs in each. **The standing goal is to retire this
  fallback entirely** — see "Collapse to one engine" below.

### What compiles today

Literals, identifiers, unary/binary arithmetic, comparison & equality,
three-valued `and`/`or`/`??` (short-circuit), `if`/`then`/`else`, `let … in`,
user-function calls + recursion, builtin calls (`print`, math fns, …); arrays,
tuples, records & field access, indexing & slicing, string interpolation,
destructuring assignment, value-methods; **all comprehensions** — `map`/`filter`/
`where`/`reduce`/`any`/`all`, including **multi-binder patterns** (`(a, b) => …`)
and fused `range(...).reduce(...)` (which additionally JIT-compiles to a native
loop at C/Go speed). In practice the VM runs essentially every program; the
tree-walker is reached only for the narrow surface below.

### The switch is flipped — the compiler is total

`compile` no longer returns `Unsupported` for **any** type-checked program: every
construct either compiles to bytecode or, for a statically-known error (immutable
reassignment, a malformed comprehension/`reduce`), emits an `Op::Raise` that fires
the canonical error at runtime — matching the tree-walker's exact wording, after
the receiver's side effects, so behaviour is identical. `run_source` therefore has
**no automatic tree-walker fallback**: the VM is the sole automatic engine. The
tree-walker is reached only via explicit `HELIX_NOVM` (A/B benchmarking + the
differential oracle) and the interactive REPL.

Two features that recently moved onto the VM:
- **First-class functions** — a standalone lambda or a bare function name becomes a
  `Value::VmFunc` (a reference to a compiled chunk; no captured environment, since
  the type checker rejects local capture and free variables resolve to shared
  globals), and a value-bound call dispatches through `CallValue`.
- **DataFrame/Tensor column-verbs** — via **type-directed compilation**: the type
  checker records each method receiver's inferred type in a `TypeMap` (keyed by node
  address, stable between check and compile), which the compiler uses to route a
  `Method` by the receiver's *true* type. A DataFrame receiver → `DfColumnVerb`
  (unevaluated column/predicate ASTs; bare names resolve against columns, then
  locals, then globals); a GroupBy → `GroupByAgg`; a known Array/Tensor → the
  value-method path (so `tensor.min(axis)` no longer falls back). This is the only
  sound disambiguation — `where`/`sort`/`min` mean different things per receiver and
  column args can't be compiled as values.

### Collapse to one engine (essentially done)

The VM is now the engine for every valid, common program. The tree-walker survives
**only** as (a) the differential-fuzzer oracle, (b) a library of shared helpers
(`eval_binary`, `pattern_parts`, `call_method`, `df_column_verb`, …), and (c) a
**rare, lazily-threaded fallback** for a narrow residue (column-verbs/aggregations
on an `Unknown`-typed receiver, and a couple of malformed-construct error messages).

Phases: ✅ multi-binder comprehensions → ✅ first-class functions → ✅ column-verbs
(type-directed) → ✅ error-cases (immutable-reassignment now raises on the VM) →
✅ a gate test (`examples_compile_on_the_vm`) asserting every shipped example
compiles to bytecode (never falls back).

**The 2 GiB stack thread is gone from the default path.** The VM recurses on the
heap (frames in a `Vec`), so it runs on the ordinary main-thread stack — it does
100 000-deep recursion without a special stack. Only the tree-walker recurses on
the native stack, and it is now reached just for the REPL, `HELIX_NOVM`, or the rare
fallback; those paths spawn a 2 GiB-stack thread **on demand** (`run_on_big_stack`
/ a scoped thread in `run_source`). So a normal `helix script.helix` no longer
reserves 2 GiB up front — the architectural smell is closed.

## Correctness gates

- **Unit parity tests** (`src/vm.rs`): a battery of programs is run on *both*
  engines and their results compared (`parity_scalar_and_control_flow`,
  `parity_functions_and_recursion`). `deep_recursion_is_iterative` proves the
  heap-frame design; `errors_propagate` proves runtime errors still fire.
- **All-examples diff** (`scripts/vmparity.sh`): every `examples/*.helix` must
  produce identical output under both engines. (The DataFrame example differs
  only in Polars' nondeterministic group-by row order — both runs use the
  tree-walker there, since DataFrame methods don't compile yet.)

## Measured

`fib(30)` (pure scalar recursion, ~2.7M calls), debug build:
**tree-walker 3.86 s → bytecode VM 1.31 s (~3×).** The gap widens in release and
on call-heavy code; it is the foundation for the JIT (Stage 3).

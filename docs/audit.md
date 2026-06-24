# Architecture & correctness audit (2026-06-22)

A deep, adversarial audit across four dimensions — the JIT, the new memoization,
cross-engine divergence, and panic/robustness — because the project now has three
execution engines that must agree, `unsafe` codegen, and a fresh caching layer.
Every finding below is fixed and regression-tested unless marked otherwise.

## Critical — fixed

1. **Memoization cached wrong results (transitive mutable read).** The purity check
   was transitive, but the "reads a mutable global" check ran only on the function's
   own body — so a memoizable function reaching a `mut` global *through a callee*
   was wrongly cached and returned stale values. Fix: a second fixpoint making the
   mutable-read check transitive (`bytecode::memoizable_fns`). Test:
   `memoization_respects_transitive_mutable_reads`.
2. **JIT recursion could crash (native stack overflow).** A JIT-compiled recursive
   function recursed on the native stack with no depth guard, so infinite recursion
   (e.g. a missing base case) aborted the process — while the VM/tree-walker raise a
   clean error. Fix: self-recursive functions are no longer JIT-eligible; recursion
   runs on the guarded VM (or is memoized).
3. **Parser had no nesting-depth guard.** Deeply-nested input (`((((…))))`, prefix
   chains, nested calls/arrays) overflowed the stack at *parse* time. Fix: a depth
   guard in `unary` (every nesting path funnels through it), which also bounds AST
   depth and so protects the type checker / compiler / tree-walker.
4. **Integer overflow panicked (debug) / diverged.** `+ - *` and negation panicked
   on overflow in debug builds and silently wrapped in release, and the JIT always
   wrapped. Fix: all engines now use **wrapping** integer arithmetic (well-defined,
   like Go / Rust-release / Java; no panic). Documented: use floats beyond the i64
   range.
5. **Integer comparison divergence.** The interpreter and VM compared i64 *as f64*
   (lossy above 2^53); the JIT compared exactly. Fix: all engines compare integers
   exactly as i64 — this also fixed a latent interpreter inconsistency (`a == b`
   false while `a <= b && a >= b` true for large ints).

## High — fixed

6. **JIT ABI soundness.** The calling convention was the ISA default, transmuted to
   `extern "C"` — a coincidence that holds on x86-64 Linux (SystemV) but is UB on
   Windows/macOS/aarch64. Fix: the JIT is gated to x86-64 Linux and forces
   `CallConv::SystemV` explicitly; elsewhere it declines and the VM runs everything.
7. **Float division by zero** (JIT `fdiv` → inf vs interpreter error). Fix: `/` is
   excluded from JIT eligibility, so division runs on the interpreter's
   zero-checked path. Test: `division_by_zero_is_not_jitted_to_inf`.

## Medium — fixed

8. `make_module` `unwrap`/`expect` could abort on startup if a Cranelift flag ever
   changed → now returns `Option` and falls back to the VM.
9. `range(huge)` materialized eagerly → OOM abort. Now capped (100M) with a clean
   error.
10. `zeros`/`ones` shape product could overflow and request an absurd allocation →
    now a checked element-count cap (1B) with a clean error.
11. `slice_indices` clamp could overflow i64 on an extreme negative bound → now
    saturating.
12. Out-of-`i64`-range integer literals silently became `0` → now degrade to their
    float magnitude.
13. `DataFrame.head(n)` truncated `n as u32` → now clamped.

## Known, documented edges (not bugs)

- **Float NaN comparison**: the JIT's `fcmp` follows IEEE (NaN → `false`) where the
  interpreter raises. Narrow (a NaN must first be produced via inf arithmetic in a
  JIT'd function, since `/` is excluded); a design decision pending — adopt IEEE
  everywhere, or keep the interpreter's error and trap in the JIT.
- **JIT scope after the audit**: it compiles **non-recursive** numeric functions
  only (recursion → VM/memoization). Combined with memoization handling overlapping
  recursion, the scalar JIT is currently lightly exercised; its strategic payoff is
  the upcoming fusing tensor compiler (Track C), for which it is the codegen
  backend.

## Verified sound (no bug)

JIT pointer lifetime / arity; the and/or/`??` three-valued logic across engines;
`if`-condition errors; whole-program fallback; `let`/global scoping; `.cache()`
transparency; the memoization depth guard, bound, and float-key exclusion; `any_call`
covering every `Expr` variant.

**Result: 106 tests, zero warnings, all four crash classes turned into clean errors,
all three engines reconciled on integer semantics.**

## Differential fuzzing (permanent regression infrastructure)

Beyond the one-time audit, three property-based fuzzers (deterministic, seeded, in
`src/vm.rs` tests) now hunt this bug class automatically on every `cargo test`:

- `differential_vm_vs_tree_walker` — 40k random scalar/array/index/interp programs;
  asserts the VM and tree-walker produce the same value (or both reject).
- `differential_functions_with_jit` — 10k random non-recursive functions called with
  random scalars; asserts VM+JIT ≡ tree-walker.
- `parser_never_panics_on_random_input` — 20k random inputs; the lexer/parser/checker
  must never panic, only return a value or a clean error.

**The function fuzzer found a real bug on its first run:** the f64 JIT specialization
always returns `Float`, but a float-argument function can still produce an `Int`
(an integer literal like `fn f(a,b)=0`, an Int-only subexpression, or a returned Int
param when args are mixed). Fix: the f64 specialization was **dropped** — the i64
specialization is type-safe (all-`Int` args → `Int` result always), f64 isn't, and
float functions run correctly on the VM. This eliminated the entire result-type
divergence class. (110 tests now.)

# Architecture and correctness audit (2026-06-22)

An adversarial audit across four dimensions — the JIT, the new memoization,
cross-engine divergence, and panic/robustness — undertaken because the project now
has three execution engines that must agree, `unsafe` codegen, and a new caching
layer. Every finding below is fixed and regression-tested unless marked otherwise.

## Critical — fixed

1. **Memoization cached incorrect results (transitive mutable read).** The purity
   check was transitive, but the "reads a mutable global" check ran only on the
   function's own body, so a memoizable function reaching a `mut` global *through a
   callee* was incorrectly cached and returned stale values. Fix: a second fixpoint
   making the mutable-read check transitive (`bytecode::memoizable_fns`). Test:
   `memoization_respects_transitive_mutable_reads`.
2. **JIT recursion could crash (native stack overflow).** A JIT-compiled recursive
   function recursed on the native stack with no depth guard, so infinite recursion
   (e.g. a missing base case) aborted the process, whereas the VM and tree-walker
   raise a clean error. Fix: self-recursive functions are no longer JIT-eligible;
   recursion runs on the guarded VM (or is memoized).
3. **Parser had no nesting-depth guard.** Deeply-nested input (`((((…))))`, prefix
   chains, nested calls/arrays) overflowed the stack at *parse* time. Fix: a depth
   guard in `unary` (every nesting path passes through it), which also bounds AST
   depth and thereby protects the type checker, compiler, and tree-walker.
4. **Integer overflow panicked (debug) or diverged.** `+ - *` and negation panicked
   on overflow in debug builds and silently wrapped in release, and the JIT always
   wrapped. Fix: all engines now use **wrapping** integer arithmetic (well-defined,
   as in Go, Rust release builds, and Java; no panic). Documented: use floats beyond
   the i64 range.
5. **Integer comparison divergence.** The interpreter and VM compared i64 *as f64*
   (lossy above 2^53); the JIT compared exactly. Fix: all engines compare integers
   exactly as i64. This also corrected a latent interpreter inconsistency (`a == b`
   false while `a <= b && a >= b` true for large integers).

## High — fixed

6. **JIT ABI soundness.** The calling convention was the ISA default, transmuted to
   `extern "C"` — a coincidence that holds on x86-64 Linux (SystemV) but is
   undefined behaviour on Windows, macOS, and aarch64. Fix: the JIT is gated to
   x86-64 Linux and forces `CallConv::SystemV` explicitly; elsewhere it declines and
   the VM runs everything.
7. **Float division by zero** (JIT `fdiv` returns inf versus the interpreter's
   error). Fix: `/` is excluded from JIT eligibility, so division runs on the
   interpreter's zero-checked path. Test: `division_by_zero_is_not_jitted_to_inf`.

## Medium — fixed

8. `make_module` `unwrap`/`expect` could abort on startup if a Cranelift flag
   changed; now returns `Option` and falls back to the VM.
9. `range(huge)` materialized eagerly, causing an OOM abort; now capped (100M) with
   a clean error.
10. `zeros`/`ones` shape product could overflow and request an excessive allocation;
    now a checked element-count cap (1B) with a clean error.
11. `slice_indices` clamp could overflow i64 on an extreme negative bound; now
    saturating.
12. Out-of-`i64`-range integer literals silently became `0`; now degrade to their
    float magnitude.
13. `DataFrame.head(n)` truncated `n as u32`; now clamped.

## Known, documented edge cases (not bugs)

- **Float NaN comparison**: the JIT's `fcmp` follows IEEE (NaN returns `false`)
  where the interpreter raises. This is narrow (a NaN must first be produced via inf
  arithmetic in a JIT'd function, since `/` is excluded); a design decision is
  pending — adopt IEEE everywhere, or retain the interpreter's error and trap in the
  JIT.
- **JIT scope after the audit**: it compiles **non-recursive** numeric functions
  only (recursion routes to the VM or memoization). Combined with memoization
  handling overlapping recursion, the scalar JIT is currently lightly exercised; its
  strategic value is the planned fusing tensor compiler (Track C), for which it is
  the codegen backend.

## Verified sound (no bug)

JIT pointer lifetime and arity; the and/or/`??` three-valued logic across engines;
`if`-condition errors; whole-program fallback; `let` and global scoping; `.cache()`
transparency; the memoization depth guard, bound, and float-key exclusion;
`any_call` covering every `Expr` variant.

**Result: 106 tests, zero warnings, all four crash classes converted into clean
errors, all three engines reconciled on integer semantics.**

## Differential fuzzing (permanent regression infrastructure)

Beyond the one-time audit, three property-based fuzzers (deterministic, seeded, in
`src/vm.rs` tests) now detect this bug class automatically on every `cargo test`:

- `differential_vm_vs_tree_walker` — 40k random scalar/array/index/interp programs;
  asserts the VM and tree-walker produce the same value (or both reject it).
- `differential_functions_with_jit` — 10k random non-recursive functions called with
  random scalars; asserts VM+JIT is equivalent to the tree-walker.
- `parser_never_panics_on_random_input` — 20k random inputs; the lexer, parser, and
  checker must never panic, only return a value or a clean error.

**The function fuzzer found a genuine bug on its first run:** the f64 JIT
specialization always returns `Float`, but a float-argument function can still
produce an `Int` (an integer literal such as `fn f(a,b)=0`, an Int-only
subexpression, or a returned Int parameter when arguments are mixed). Fix: the f64
specialization was **removed** — the i64 specialization is type-safe (all-`Int`
arguments always produce an `Int` result), f64 is not, and float functions run
correctly on the VM. This eliminated the entire result-type divergence class. (110
tests now.)

---

# Second hardening round (2026-07-02)

A second adversarial pass, prompted by two new realities: the language now has a
**networked surface** (an in-process HTTP server that accepts untrusted bytes from
the network — see [ADR 0022](adr/0022-http-version-roadmap.md)), and the VM had grown
long-running-process features (memoization, tail recursion) whose failure modes are
memory growth rather than a wrong answer. The theme of this round is **resource
bounds and liveness**, not just correctness: an attacker who cannot make Helix return
the wrong value can still try to make it exhaust memory, spin a core, or wedge. Three
agents swept the interner, the VM's caches and recursion, the network parser, and the
differential oracle's blind spots. Every finding below is fixed and regression-tested.

## Denial-of-service / resource exhaustion — fixed

1. **Unbounded request head (network DoS).** The HTTP request parser read header
   lines with no limit, so a client could stream an endless header line, or millions
   of tiny headers, and force unbounded buffering per connection. Fix: a per-line cap
   (`MAX_HEADER_LINE = 16 KiB`) enforced with a bounded `take()` on the reader, a
   header-count cap (`MAX_HEADER_COUNT = 1000`), and the existing body cap
   (`MAX_BODY`). The same caps apply on **both** server paths — the blocking
   `request()` reader and the event loop's byte-buffer `parse_request_buf` — so
   keep-alive connections are bounded too. (`9300968`)
2. **Malformed request could kill the server loop.** A request that failed to parse
   propagated an error out of `accept`/`poll`, tearing down the accept loop and
   ending service for everyone. Fix: a malformed request is answered with a `400` and
   the connection dropped, while the server **keeps serving** — a single bad client
   can no longer take the process down. (`f4ce3ac`)
3. **SSE backlog could grow without bound.** A slow or vanished SSE subscriber let
   queued events accumulate in memory indefinitely (a classic slow-consumer leak).
   Fix: a per-shard pending-bytes budget (`SSE_PENDING`) plus a global ceiling
   (`SSE_GLOBAL_PENDING = 64 MiB`); once exceeded, the laggard's queue is shed rather
   than growing until OOM. (`faa523c`)
4. **Memoization cache could freeze memory.** The VM's memo table grew for the
   lifetime of the process; a program that memoized over a very large or unbounded
   key space would climb until OOM. Fix: an entry cap (`MEMO_MAX_ENTRIES`) that
   **evicts** (clears and rebuilds) on overflow instead of freezing — bounded memory,
   preserving the fast path for the common small-key case. (`a54aba1`)
5. **Symbol interner growth from untrusted keys.** Interned symbols are never freed
   (by design — they back fast identifier comparison), so interning attacker-supplied
   strings (e.g. arbitrary JSON object keys) would be an unbounded leak. Two guards,
   audited sound this round: the interner is **capped at 50M** symbols, and dynamic
   key lookup uses `lookup` (read-only, no insertion) rather than `intern`, so parsing
   untrusted JSON with novel keys on every request cannot grow the interner.

## Correctness / liveness — fixed

6. **Tail recursion leaked the call stack.** Deeply/unbounded tail-recursive functions
   (the idiomatic shape for a server's accept loop or any long-running fold) pushed a
   VM frame per call, so a program meant to run forever in constant space instead grew
   the frame `Vec` until OOM. Fix: **tail-call optimization** — an `Op::TailCallFn`
   plus a `tco_peephole` bytecode pass that rewrites a `CallFn` in tail position into a
   frame **reuse** rather than a push, making tail recursion genuinely constant-space.
   The event-loop server relies on this to run indefinitely at flat 16 MB. Verified by
   `deep_tail_recursion` and reconciled on both engines by
   `tail_calls_match_tree_walker_on_vm`. (`48f3359`; see also
   [execution-engine.md](execution-engine.md).)
7. **Silent `NaN` from rational modulo / fractional power.** Rational `%` and a
   fractional `**` could overflow `f64` and yield a silent `NaN` (the exact class of
   silent-wrong-answer this project rejects — `missing` and errors are first-class
   precisely so nothing is silently wrong). Fix: both now **raise a clean error** on
   overflow instead of producing `NaN`. (`078892a`)

## Oracle blind spots — closed

The differential oracle (VM vs tree-walker) is the project's core correctness
guarantee, so a *gap in what it exercises* is itself a finding. This round added
targeted parity coverage for surfaces the fuzzers under-sampled — **Dict** operations,
**sorted iteration order**, **error propagation paths**, and **modulo** semantics —
all confirmed bit-identical across engines (`audit_flagged_surfaces_match_tree_walker`,
`481046e`). A known **by-design** engine difference is documented rather than
"fixed": the tree-walker guards recursion at `MAX_CALL_DEPTH = 20,000` while the VM
(heap frames) allows `VM_MAX_DEPTH = 1,000,000` — the VM legitimately runs deeper, so
the oracle treats depth-limit divergence at that boundary as expected.

**Result of this round: the networked surface is bounded against the standard
resource-exhaustion attacks (oversized/over-many headers, slow SSE consumers,
malformed requests), the two long-running caches (memo, interner) and tail recursion
are memory-bounded, two silent-`NaN` paths became clean errors, and the oracle's
coverage of Dict/ordering/errors/modulo is locked in — all regression-tested and
parity-checked (`scripts/vmparity.sh` → `RESULT=0`).**

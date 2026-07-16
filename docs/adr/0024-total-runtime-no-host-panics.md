# ADR 0024 — A total runtime: user input never aborts the host

- **Status:** Accepted — implemented and regression-tested (2026-07-10). Four reachable
  aborts/wrong answers found by adversarial audit and fixed across engines; the
  differential oracle (tree-walker ≡ VM ≡ JIT) stayed green through the change.
  Enforcement automation (a CI lint gate for new `unwrap`/`expect` in interpreter
  paths) and fuzzer-pool widening remain open (see ROADMAP "Future work — correctness
  backlog").
- **Date:** 2026-07-10
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0001 — Missing data](0001-missing-data.md) (the three-valued
  contract this ADR extends to `argmax`/`argmin`), [ADR 0004 — Functions, errors &
  mutability](0004-functions-errors-mutability.md) (errors as values, never crashes),
  [ADR 0018 — Reproducible random](0018-random.md) (the same determinism-as-contract
  stance).

## Context

Helix's error principle has always been "errors must be instructive" — a Helix program
that does something invalid gets a caret-annotated `HelixError`, not a stack trace. But
an audit posed a stricter question: is the runtime **total**? That is, does *every*
valid-syntax program either produce a value or a clean Helix error — or are there inputs
that abort the host process?

The audit found the answer was no, four ways:

1. `(0 - 9223372036854775807 - 1) // (0 - 1)` — i.e. `i64::MIN // -1` — **aborted the
   interpreter in every build mode.** Rust's `div_euclid` computes `self / rhs`
   internally, and signed `MIN / -1` is an *always-checked* overflow (not gated by
   `overflow-checks`): panic in debug **and** release. `i64::MIN % -1` likewise. The
   `arith` path (`+ - *`) had already been hardened with `wrapping_*` ("never a
   debug-build panic"); `//` and `%` had not.
2. `[3.0, sqrt(0.0 - 1.0), 1.0].sort()` — a `NaN` element — **aborted the
   interpreter.** The shared comparator treated NaN as `Equal` to everything, which is
   intransitive (`3 == NaN`, `NaN == 1`, yet `3 > 1`); Rust's sort (≥1.81) detects the
   non-total order and panics ("user-provided comparison function does not correctly
   implement a total order").
3. `round(3.14159, 2147483648)` silently returned `NaN` — the digit count was narrowed
   with a wrapping `as i32`, so `2^31` became `i32::MIN`, the scale underflowed to
   `0.0`, and the formula computed `0/0`.
4. `argmax([1, missing, 3])` raised a *type error* on `missing` (and silently skipped
   `NaN`), while every sibling aggregation (`sum`/`mean`/`min`/`max`/`median`) returns
   `missing` — a hole in the ADR 0001 three-valued contract.

None of these were hypothetical: each had a one-line reproduction, and the first two
kill the process a REPL, a server (`listen`), or an embedded interpreter is running in.
A scientific language whose runtime can be crashed by an arithmetic expression cannot
honor "great errors" — the crash *is* the error message.

## Prior approaches and their documented shortcomings

| System | Approach to these edges | Documented pain |
|---|---|---|
| **C / C++** | `INT_MIN / -1` is undefined behavior | The canonical UB trap; on x86 it raises SIGFPE and kills the process — the exact behavior a managed language exists to prevent. |
| **Java** | `Integer.MIN_VALUE / -1` throws… nothing: it silently wraps to `MIN_VALUE`; `%` gives 0 | JLS §15.17.2 chose wrapping for totality — the precedent this ADR follows — but did it silently, without documenting the policy per-operator. |
| **Python 3** | Arbitrary-precision ints (no overflow); `sorted()` with NaN | No int edge, but `sorted([3.0, nan, 1.0])` silently produces a *mis-sorted* list (comparison-based, NaN incomparable) — a famous silent-wrong-answer footgun numpy had to fix with `np.sort` placing NaN last. |
| **NumPy** | `np.sort` places NaN **last** (a total order); `np.argmax` on NaN returns the NaN's index | Total-order sorting is the accepted scientific-computing answer; but `argmax`'s NaN behavior is a documented surprise — propagating a sentinel (Helix's `missing`) is strictly clearer. |
| **Rust std** | `sort_by` panics on a non-total comparator; `f64::total_cmp` provided since 1.62 | Rust chose to *expose* the totality requirement and provide the correct tool; runtimes embedding user comparisons must use it. |

## Decision

**Totality is a language guarantee: every syntactically valid Helix program either
produces a value or a caret-annotated `HelixError`. No user input may abort the host.**
Concretely, per edge:

- **Integer overflow policy is *wrapping*, uniformly.** `//` and `%` now use
  `wrapping_div_euclid`/`wrapping_rem_euclid`, matching the `wrapping_add/sub/mul`
  policy `arith` already had: `i64::MIN // -1` → `i64::MIN` (wraps), `i64::MIN % -1`
  → `0` (the mathematically consistent pair, and Java's answer). Division/modulo *by
  zero* remains a Helix error — the zero guard is unchanged. Applied in **both** the
  tree-walker (`src/interp/ops.rs`) and the VM's Int fast path (`src/vm.rs`) so the
  differential oracle holds; the JIT needs no change because it only compiles `//`/`%`
  with a *positive constant* divisor (which structurally rules out `-1`).
- **Sorting uses a total order.** The shared `numeric_cmp` uses `f64::total_cmp`
  (exact `i64` compare for Int/Int, as before). NaN sorts to a consistent extreme
  (after `+inf`, numpy-style) instead of poisoning the comparator. This affects only
  *where a NaN lands in a sorted result*; reductions (`min`/`max`/`median`/`mean`)
  are untouched — they already filter NaN/missing to `missing` before comparing.
- **Narrowing casts of user-controlled counts are clamped, not wrapped.** `round`'s
  digit count clamps to f64's decimal-exponent span (±308) before `powi`; a scaled
  overflow (rounding finer than the value resolves) is a no-op returning `x`.
- **The three-valued contract (ADR 0001) covers *every* aggregation.**
  `argmax`/`argmin` propagate `missing` when any element is `missing` *or NaN*,
  exactly like `sum`/`mean`/`min`/`max`/`median`.

## Rationale

- **Wrapping, not erroring, for `MIN // -1`.** The alternative — raising a Helix error
  on the single unrepresentable quotient — would make `//` partial on a case no real
  program means to hit, and would *diverge from `arith`'s established wrapping policy*
  (an inconsistency users would have to memorize: `MIN * -1` wraps but `MIN // -1`
  errors). Java's precedent shows wrapping is the boring, safe choice; the pair
  (`q = MIN`, `r = 0`) even preserves `a == b*q + r` mod 2⁶⁴.
- **`total_cmp`, not a pre-scan error, for sort.** `sort` erroring on NaN (like
  comparisons do) was considered; rejected because a sort is often exactly the tool
  used to *find* the NaNs in data, and numpy's place-NaN-last behavior is the
  scientific-computing convention users arrive with. Reductions keep propagating
  `missing`, so no aggregate silently absorbs a NaN.
- **Fix both engines, prove with the oracle.** Every semantic change landed in the
  tree-walker and VM in the same commit, and the 342-test suite (including the
  tri-engine differential fuzzers) passed — the totality fix cannot itself introduce
  an engine divergence.

## Rejected alternatives

- **Checked arithmetic that raises a Helix "integer overflow" error** on `MIN // -1` —
  rejected: inconsistent with the wrapping `arith` path, adds a branch to the hottest
  ops for a case with a sensible wrapped answer, and (unlike `/0`) has no
  data-quality signal a user needs to hear about.
- **Erroring on NaN in `sort`** — rejected: breaks the "sort to inspect your dirty
  data" workflow and contradicts the numpy convention. The intransitive-`Equal`
  comparator (the old code) was never a candidate — it is precisely what panics.
- **`std::panic::catch_unwind` around the interpreter as a backstop** — rejected: it
  masks bugs instead of fixing them, doesn't work with `panic = "abort"` (the shipping
  release profile), and leaves the interpreter in an unspecified state.
- **Doing nothing about `round`'s `as i32`** (it's "just" a silly argument) — rejected:
  a silent NaN from a valid call is exactly the class of wrong answer a scientific
  language must not produce.

## Consequences

- The wrapping policy is now *the documented contract* for all five integer arithmetic
  operators (`+ - * // %`): overflow wraps mod 2⁶⁴; only division/modulo **by zero**
  raise. Any future checked-arithmetic mode (e.g. an opt-in `--checked` build) is a new
  decision layered on top, not a change to this default.
- NaN's sorted position (after `+inf`) becomes observable behavior; it must be
  documented in the language reference (ROADMAP backlog item) and held stable.
- Every fix carries a regression test (`int_min_floordiv_mod_do_not_overflow_panic`,
  `int_min_floordiv_mod_wrap_on_all_engines`, `sort_with_nan_does_not_panic`,
  `round_to_huge_digit_count_is_a_noop`, `argmax_argmin_propagate_missing`), so the
  property is pinned by CI, not by memory.
- The audit also exposed *why* these survived 40k fuzzed programs: the differential
  fuzzer's literal pool never generates `i64::MIN`, `-1` divisors, or NaN-producing
  subexpressions. Widening that pool is the standing backlog item — totality is only
  as strong as the adversarial inputs that test it.
- The totality guarantee makes the runtime safe to embed: a `listen()` server, the
  REPL, and (via the ctype bridge, [Helix ADR 0023](0023-hbc-emitter-artifact-format.md))
  a ring-0 kernel VM can all run untrusted-shaped programs without a process-kill
  escape hatch. (ctype's `hvm` already made the same choices independently: wrapping
  arithmetic, explicit `DivByZero`, no panics — the two runtimes now agree.)

## Open questions

- Should the enforcement gate be `clippy::disallowed_methods` for `unwrap`/`expect`
  scoped to `src/interp` + `src/vm`, or a custom lint? (Backlog; either pins the
  property structurally.)
- Whether `argsort` should *place* NaN (like `sort`) rather than propagate `missing`
  via `numeric_cmp`'s ordering — today it orders them consistently; the reductions
  propagate. Revisit if users report surprise.
- A future `Int128`/`BigInt` default would dissolve the wrapping question entirely
  (Python's answer) — out of scope while `i64` is the Int type (16-byte `Value` cap).

# Integer semantics at the `i64` boundary

Helix integers are `i64`. This document records **what actually happens** at the edges,
measured across all three engines, so the behaviour is a decision on record rather than
whatever the arithmetic happened to compile to.

Re-run the audit with the probes in this document against
`HELIX_NOVM=1` (tree-walker), `HELIX_NOJIT=1` (bytecode VM) and the default (JIT). Every
row below agreed **bit-identically across all three**; a row that ever stops agreeing is a
differential-oracle bug, not a semantics question.

## The rule: `+`, `-`, `*` wrap

| expression | result |
|---|---|
| `9223372036854775807 + 1` | `-9223372036854775808` |
| `9223372036854775807 * 2` | `-2` |
| `(0 - 9223372036854775807) - 2` | `9223372036854775807` |
| `9223372036854775807 * 9223372036854775807` | `1` |

Wrapping is **silent** — there is no trap, no error, and no promotion. It is also
**consistent**, including inside JIT kernels: a map/reduce over values that overflow
produces the same wrapped result the tree-walker does.

This matches C (in practice), Rust in release, Go, and NumPy `int64`. It differs from
CPython, whose `int` is arbitrary-precision and cannot overflow. A Helix program that
would need bignums today gets a wrong answer instead of a slow one.

## Division is total, and does not trap

Cranelift's `sdiv`/`srem` raise a hardware trap (SIGFPE) on divide-by-zero **and** on
`i64::MIN / -1`. Neither reaches the process:

| expression | result |
|---|---|
| `1 / 0` | `error: division by zero` |
| `1 % 0` | `error: modulo by zero` |
| `x // 0`, divisor arriving as array data | `error: integer division by zero` |
| `MIN // -1` | `-9223372036854775808` (wraps, consistent with `*`) |
| `MIN % -1` | `0` |
| `MIN / -1` | `9223372036854775808.0` (true division promotes to `Float`) |

The divide-by-zero rows were verified with the divisor supplied as **array data**, so
constant folding cannot mask them, and in `map`, `reduce` and compiled-function positions
as well as scalar ones. No probe produced a signal death (exit ≥ 128) on any engine.

`/` is true division and yields `Float`; `//` is floor division and stays `i64` (and so
wraps). That is why `MIN / -1` is exact and `MIN // -1` is not.

## Known divergence: `.sum()` promotes, `.reduce()` wraps

```
[9223372036854775807, 1].sum()                    => 9223372036854775808.0
[9223372036854775807, 1].reduce(0, (s, x) => s + x) => -9223372036854775808
```

Two spellings of the same computation give different answers. `.sum()` and `.mean()`
detect overflow and promote to `Float` (losing exactness above 2^53 but staying
approximately right); a hand-written `reduce` uses the ordinary `+` and wraps.

Both behaviours are defensible on their own; having both is not. This is recorded as a
gap rather than silently tolerated — see the open question below.

## Conversions

| expression | result |
|---|---|
| `to_float(9223372036854775807)` | `9223372036854775808.0` (nearest `f64`; `i64::MAX` is not representable) |
| `to_int(9.3e18)` | `9223372036854775807` (saturates at `i64::MAX`) |

`to_int` **saturates** rather than wrapping or trapping, so it is total for every finite
input.

## Open question: should `+` stay silent?

Three options, none free:

- **Keep wrapping.** Fast (no branch), matches NumPy/Rust-release, and every existing
  program keeps its current behaviour. Silently wrong when it matters.
- **Check and raise.** Costs a branch and a flag test per operation, and — more
  importantly — would have to be threaded through the JIT kernels via the poison
  out-param mechanism the raising builtins already need. Changes the result of every
  program that currently relies on wrapping.
- **Promote on overflow**, as `.sum()` already does. Removes the divergence above by
  making `+` behave like `.sum()`, at the cost of a type that can change under the user
  and of exactness above 2^53.

This is an ADR-level decision because it alters the meaning of existing programs; it
should not be changed as an incidental part of a performance pass. What is *not* in
question is the current state being **documented, total, and consistent across all three
engines** — which the table above establishes.

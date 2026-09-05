# Which builtins reach native code, and why the rest do not

> **Historical record (2026-07-31)**, with the 2026-09-05 update below — the audit's standing result; the remaining JIT-eligibility work is tracked in `docs/dx-plan.md`.

## Update 2026-09-05 — the SOURCE kind is half the question

The table below was measured over a **range** (an Int source): `map(BUILTIN(to_float(it)))`.
A field build re-ran it over a **`Floats` array** (`xs = range(…).map(to_float(it) * 0.5)`,
then `xs.map(to_int(it))`) and found `to_int`, `sign`, `floor`, `it / 2.0` and
`it * (1.0 / 2.0)` at **1.0×** while `jit-explain` said "compiled". Both were true: the
Int-source specialization existed, the Float-source one did not, and a `Floats` receiver
had only the monomorphic `+ - *` kernel to fall back on. Two things changed:

- **A Floats-source typed map kernel** (`float_source_map_eligible`, four build passes:
  Int-proven or value-scalar captures × Float or Int root) — the mixed family's per-node
  typing with the element a Float. Measured on the shapes above, 2M floats, JIT on / off:
  `to_int` 2.1× → **26×**, `sign` 2.0× → **25×**, `floor` 2.0× → **25×**, `it / 2.0` 2.4×
  → **22×**, `it * (1.0 / 2.0)` 1.9× → **25×** (`abs`, the control, 22–26× throughout).
- **`jit-explain` names each map site's specializations** and says when a source kind has
  none: `compiled (i64) — a Float source runs the bytecode loop`.

Also closed the same day, the cliff under the commonest map there is: `range(…).map(it * s)`
with a Float `s` — 1.0× against 16× for the literal `it * 0.5`. The Int-proven build types
`s` an Int and its marshal declines the Float at dispatch; the value-scalar build must refuse
`Int * capture` for a capture that MIGHT be an Int. A third marshal — every capture a runtime
Float, typed a genuine Float — is a proof the dispatch can make, and under it the capture
promotes exactly where the walker promotes it (`float_caps_map_eligible`; "mapmF"/"mapmFi",
"mapftF"/"mapftFi"). `it * s`, `it + s`, `to_int(it * s)`: 1.0× → 16×.

Found while pinning `floor(it / s)` with a Float `s`: the analysis behind every
value-scalar and indexed build had no arm for the four rounders (its unindexed twin did),
so no such build existed for a rounding body on any source. It does now.

A **conditional in a map body** (`if it > 5.0 then it else 0.0` — relu) is offered now, in
every typed body: `and`/`or` over the six comparisons, both branches of one kind (an `if`
whose branches differ yields an Int or a Float per element, which no packed buffer can hold, so
it declines), and a NaN meeting an ordering comparison poisons to the walker's "cannot
compare". **`**`** and the transcendentals followed the same day — see the retired "libm
exactness" note below. `clamp` (raises when `lo > hi`) and the two-argument `log`/`hypot` are
what remains off the native path.

Helix's JIT compiles a *subset* of expressions. A builtin outside that subset does not merely
run slower — it **forces the entire enclosing loop onto the bytecode VM**, because eligibility is
all-or-nothing per body. That makes a missing builtin a cliff, not a gradient: measured at
n=30M, `to_float` cost **132–227×** purely by being absent from the gates, and `to_int`/`sign`
cost **159–308×**.

This page is the audit's standing result. Re-run it whenever the numeric surface changes:
every builtin's cost is `JIT time ≈ NOJIT time` (it blocks) versus a large ratio (it compiles).

## Status

| builtin | result | reaches native code | why |
|---|---|---|---|
| `sqrt` | Float | **yes** | `fsqrt`, IEEE correctly-rounded |
| `abs` | preserves | **yes** | `iabs` / `fabs` |
| `min`, `max` | preserves | **yes** | compare-then-pick-the-original-operand, so NaN matches |
| `to_float` | Float | **yes** | `fcvt_from_sint`; identity on a Float |
| `to_int` | Int | **yes** | `fcvt_to_sint_sat` — saturating, never raises |
| `sign` | Int | **yes** | two compares + selects; NaN falls through to 0 |
| `floor`, `ceil`, `round`, `trunc` | Int | **yes** (since the poison out-param) | the kernel sets poison on an out-of-i64-range result; the VM discards the output and the bytecode loop raises the exact error — Int source (`mapm`/`mapmi`) and, since 2026-09-05, Float source (`mapft`) |
| `clamp` | preserves | **no** | **raises** when `lo > hi` |
| `exp`, `ln`, `log2`, `log10` | Float | **yes** (2026-09-05) | a host call to the very Rust function the walker applies — same machine code, same bits |
| `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `sinh`, `cosh`, `tanh` | Float | **yes** (2026-09-05) | host call, as above |
| `cbrt`, `degrees`, `radians`, `erf`, `normal_cdf`, `normal_pdf`, `relu`, `sigmoid`, `**` | Float | **yes** (2026-09-05) | host call; `**` reproduces the walker's `powi`/`powf` rule; `Int ** Int` declines (its kind is the run time's) |
| `log(x, base)`, `hypot` | Float | **no** | two-argument forms, not yet admitted |

## Why the excluded ones are excluded

**They raise, and native code cannot — so the kernel reports it.** `floor`/`ceil`/`round`/`trunc`
return an `Int` and raise when the result leaves the 64-bit range (`floor(1.0e30)` is an error,
not a saturation); `clamp` raises when `lo > hi`; `/` raises on a zero divisor where `fdiv`
yields inf. The **poison out-param** answers all of them: the kernel ORs a flag on the failing
case, the VM discards the whole output and re-runs the checked bytecode loop, which raises the
interpreter's exact error at the exact element. It began as the dividing f64 reduce's
(`call_reduce_f64_div`) and `MixedFn`'s NaN-comparison bail, and now carries the four rounders
and `/` through every typed map kernel (`mapm`, `mapmi`, `mapmv`, and the Floats-source
`mapft` family) and the f64 filter's NaN comparisons. `clamp` is the one still waiting.

`to_int` is the instructive contrast: it **saturates** instead of raising (NaN → 0, ±inf → the
i64 extremes), which is exactly `fcvt_to_sint_sat`, so it needed no bail at all. The dividing
line is not "converts to an integer", it is "can fail".

**A trap worth recording**: Helix's `round` is **half-away-from-zero** (`round(2.5)` = 3,
`round(-2.5)` = -3), *not* IEEE round-to-nearest-even. Cranelift's `nearest` instruction gives 2
for `round(2.5)`, so lowering `round` to it would be silently wrong on every tie — a class of bug
that no small-input test would catch. Whoever adds `round` must synthesize half-away-from-zero.

**libm exactness — retired (2026-09-05).** The transcendentals were a deliberate exclusion: their
results must match the host libm *bit for bit* or the three-engine oracle breaks, and Cranelift
has no instruction for them. They match BY CONSTRUCTION now: a kernel calls an `extern "C"`
shim (`src/jit/ffi.rs`, `jit_host_*`) that *is* the Rust function the walker applies —
`f64::exp`, `crate::stats::erf`, the `**` arm's `powi`/`powf` rule — one function, compiled
once into this binary, executed by both engines on the same bits. Not "the same libm, pinned
per platform": the same machine code. Pinned by
`transcendentals_and_pow_in_kernels_agree_and_engage`, which runs every name on three engines.

## A shape that used to block regardless of builtins — closed

An **`Int`-rooted body with `Float` intermediates** — `map(to_int(to_float(it) * 1.5))` — had no
kernel at all: the i64 kernel cannot hold a float intermediate, and the mixed kernel wrote an
`f64` output buffer so it required a *float* root (4.05s JIT against 4.01s VM at n=30M). The
Int-rooted mixed specialization (`mapmi`) closed it for an Int source, and the Floats-source
typed kernels (`mapfti`, 2026-09-05) for a Float one: `xs.map(to_int(it))` over a `Floats`
array writes an `i64` buffer natively.

## How to re-run this audit

For each builtin, time a hot loop containing it with the JIT on and off:

```bash
printf 'print((1..3000000).map(BUILTIN(to_float(it))).reduce(0.0, (s, x) => s + x))\n' > /tmp/p.helix
/usr/bin/time -f %e ./target/gate/helix run /tmp/p.helix
HELIX_NOJIT=1 /usr/bin/time -f %e ./target/gate/helix run /tmp/p.helix
```

Two traps in reading the result. Take **min-of-N** — a single cold run is worthless here. And
watch the JIT column for sub-timer-resolution times: a ratio computed against `0.00s` is
meaningless, so raise N until the compiled time is measurable rather than concluding from a
divide-by-almost-zero.

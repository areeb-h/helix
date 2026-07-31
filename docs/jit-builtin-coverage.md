# Which builtins reach native code, and why the rest do not

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
| `floor`, `ceil`, `round`, `trunc` | Int | **no** | **raise** out of i64 range — see below |
| `clamp` | preserves | **no** | **raises** when `lo > hi` |
| `exp`, `ln`, `log`, `log2`, `log10` | Float | **no** | libm — see below |
| `sin`, `cos`, `tan`, `atan` | Float | **no** | libm |
| `hypot`, `cbrt` | Float | **no** | libm |

## Why the excluded ones are excluded

**They raise, and native code cannot.** `floor`/`ceil`/`round`/`trunc` return an `Int` and raise
when the result leaves the 64-bit range (`floor(1.0e30)` is an error, not a saturation).
`clamp` raises when `lo > hi`. A kernel has no way to report that mid-loop, so admitting them
needs a **poison out-param** — the pattern the dividing f64 reduce already uses: the kernel sets
a flag on the failing case, and the VM discards the result and re-runs the checked bytecode loop,
which raises the interpreter's exact error. That mechanism exists today only for the scalar f64
reduce (`call_reduce_f64_div`) and for `MixedFn`'s NaN-comparison bail; extending it to the map,
filter and fused kernels is what these four are waiting on.

`to_int` is the instructive contrast: it **saturates** instead of raising (NaN → 0, ±inf → the
i64 extremes), which is exactly `fcvt_to_sint_sat`, so it needed no bail at all. The dividing
line is not "converts to an integer", it is "can fail".

**A trap worth recording**: Helix's `round` is **half-away-from-zero** (`round(2.5)` = 3,
`round(-2.5)` = -3), *not* IEEE round-to-nearest-even. Cranelift's `nearest` instruction gives 2
for `round(2.5)`, so lowering `round` to it would be silently wrong on every tie — a class of bug
that no small-input test would catch. Whoever adds `round` must synthesize half-away-from-zero.

**libm exactness.** The transcendentals are a deliberate, permanent exclusion, not a gap. Their
results must match the host libm *bit for bit* or the three-engine oracle breaks, and Cranelift
has no instruction for them — they would need an external call to the very libm the interpreter
uses, pinned per platform. The correctness risk outweighs the win.

## A shape that blocks regardless of builtins

An **`Int`-rooted body with `Float` intermediates** — `map(to_int(to_float(it) * 1.5))` — has no
kernel at all: the i64 kernel cannot hold a float intermediate, and the mixed kernel writes an
`f64` output buffer so it requires a *float* root. Measured at n=30M it is 4.05s JIT against
4.01s VM, i.e. not compiling, and no amount of builtin admission changes that. Closing it means
a Float-source → Int-output map specialization.

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

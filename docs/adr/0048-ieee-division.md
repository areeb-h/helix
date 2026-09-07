# ADR 0048 — `/` is IEEE 754 division

- **Status:** **Accepted & implemented 2026-09-07** (0.10.0-to-be). Supersedes the `/` half
  of ADR 0036 policy 1 ("division and modulo by zero are errors") and the sentence of policy 2
  that rested on it. The decision is the user's, on the field build's 1.30.
- **Decision:** `x / 0` is `inf`, `-x / 0` is `-inf`, `0 / 0` is NaN — never an error — on
  scalars, on arrays (which broadcast to the scalar kernel), on tensors (which already said
  so), on frame columns on both backends, and in native code, where `fdiv` says the same.
  `//` and `%` keep raising on a zero divisor. An exact `rational(1, 2) / 0` keeps raising:
  the rationals have no infinity.

## Context

Helix has had `inf`, `nan`, `is_nan`, `is_finite` and `is_infinite` since the tensor work,
and `tensor([1.0, 0.0]) / 0` answered `[inf, NaN]` — while the scalar `1.0 / 0.0` raised, so
the same arithmetic had two answers by carrier (field build, 1.30). The raise was also the
one thing that kept `/` off the native path: a JIT kernel containing `/` had to carry a
POISON out-param — a compare per element, a flag ORed on every division, and on any zero a
discarded result and a re-run on bytecode to raise the walker's exact sentence (`audit.md`
item 7 records the earlier stage, when `/` was simply excluded). The frames guarded every cell
the same way, naming the row.

## Consequences

- One rule on every carrier. `1 / 0` no longer needs `try`; a program that caught it reads
  `inf` or NaN now, and `tests/compat/MIGRATIONS.md` says so.
- The JIT's dividing kernels lose their poison signature: `body_raises` no longer counts `/`,
  the three codegen arms are a bare `fdiv`, and the dividing f64 fold takes the plain kernel.
  Nothing about `/` needs a guard, a fallback or a re-run.
- ADR 0036 policy 5 stands: a NaN reaching an ordering comparison raises, with the `is_nan`
  hint. The guard moved from the division to the comparison — which is where a NaN would
  become a wrong answer, and the only place it needs to be.
- ADR 0036 policy 3 stands: NaN is a value meaning "this computation failed", `missing` an
  absent datum, and nothing converts one into the other. `0 / 0` is the former.
- `to_json` writes `null` for a non-finite float (serde's rule, as before).
- What still raises on zero: `//` and `%`, for Int and Float alike (the integer operators;
  Python raises for both there too), and exact rational division.

## What the differential campaign compares

`tests/corpus/ieee_division.helix` pins scalars, arrays, a frame column on both backends and
the three raising forms on all three engines; `division_is_ieee_on_every_engine` and the VM
tests that used a zero divisor as their poison source (now a NaN comparison or a rounder
where the poison path itself is under test, an agreement on the IEEE value where the
division was) pin the JIT's native answer against the walker's.

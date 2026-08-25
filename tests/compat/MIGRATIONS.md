# Recorded behavior changes

Every entry here is a program whose output **intentionally** changed after a baseline
was captured, with the reason. `compat_baselines_hold` reads this file: a program
listed here is a known change and no longer fails the gate; a program not listed here
that drifts is a regression.

Format — the program name in backticks, then what changed and why.

## v0.5.1 → v0.6.0 — the semantics unification (ADR 0036)

v0.6.0 makes frames, arrays and scalars answer the same question. Twelve divergences
between the two DataFrame backends and the language were closed; five of them had
never been recorded anywhere. The full decision list is
[ADR 0036](../../docs/adr/0036-one-semantics.md).

The entries below are **pre-authorized**: they name programs whose pinned output the
unification is expected to move, so each behavior commit can land green. An entry here
is a promise that the change was intended — it is not a licence to stop reading the
diff. `scripts/dfdiff.sh` is what proves nothing else moved.

- `examples__dataframes__dataframes` — ADR 0036 policy 1, true division. At the v0.5.1
  tag this was the **only** `with({…})` in the entire tracked tree, which is why five
  arithmetic divergences hid behind one line. `@resting_hr / (@age / 10)` was computed
  with the polars backend's integer division and printed `hr_per_decade` as `18, 21,
  16, 35, …`; it now uses the language's true division and prints
  `17.5609756097561, …`. The column width changes with it.

- `tests__corpus__m5_nan_sort` — ADR 0036 policies 6 and 8. Two changes in one file.
  The source binds `nan = sqrt(-1.0)`, which stops being legal once `nan` is a builtin
  constant (ADR 0027 shadowing), so the file is rewritten to use the literal. And the
  answer moves: NaN sorted FIRST here only because `sqrt(-1.0)` yields a *negative* NaN
  on x86 and the old rule ordered by sign bit. NaN now sorts last, sign-independently.

- `examples__language__ordering` — ADR 0036 policy 6. The NaN block is rewritten:
  placement (now always last), the reductions it demonstrates (now propagating NaN
  rather than laundering it into `missing`), and a new frame line showing that a frame
  sort agrees with an array sort. This file is also a doctest source, so its `>>>`
  examples move with it.

- `examples__language__missing-data` — ADR 0036 policies 3 and 4, *if* it moves. This
  file teaches the missing/NaN boundary and is the most likely place the withdrawal of
  NaN-into-`missing` laundering shows up. Listed here so the commit can land; if
  `dfdiff.sh` and the gate show it unchanged, **delete this entry** rather than leaving
  a pre-authorization standing over a program that never moved.

- `examples__numerics__kernels` — same conditional status as above (mentions NaN).
  Delete the entry if it turns out not to move.

- `examples__numerics__math` — same conditional status as above (mentions NaN).
  Delete the entry if it turns out not to move.

`tests/corpus/t9_eq3_tuples.helix` also binds `nan` and needs the same source edit, but
its output does not move (`==` stays IEEE, so `nan == nan` is still `false`). Source
edit only — no entry, because nothing it prints changed.

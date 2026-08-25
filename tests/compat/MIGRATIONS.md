# Recorded behavior changes

Every entry here is a program whose output **intentionally** changed after a baseline
was captured, with the reason. `compat_baselines_hold` reads this file: a program
listed here is a known change and no longer fails the gate; a program not listed here
that drifts is a regression.

Format — the program name in backticks, then what changed and why.

**An entry is a licence to drift, so it must be earned.** Six were pre-authorized while
v0.6.0 was in flight and only the two below were ever used; the other four were deleted
once the work landed. A standing entry over a program that never moved is not harmless
bookkeeping — it silently blesses the *next* change to that program, which is precisely
what this file exists to prevent. Reconcile after the work, not just before it.

## v0.5.1 → v0.6.0 — the semantics unification (ADR 0036)

v0.6.0 makes frames, arrays and scalars answer the same question. Sixteen divergences
between the two DataFrame backends and the language were closed; five of them had never
been recorded anywhere. The full decision list is
[ADR 0036](../../docs/adr/0036-one-semantics.md).

Only two pinned programs moved, which is itself worth knowing: the tracked tree barely
exercised the surface that changed. At the v0.5.1 tag,
`examples/dataframes/dataframes.helix:27` was the **only** `with({…})` in the entire
tracked tree — one line carrying the whole arithmetic surface of the frame language.
`scripts/dfdiff.sh` exists because of that, and it is what proves nothing else moved.

- `examples__dataframes__dataframes` — ADR 0036 policy 1, true division.
  `@resting_hr / (@age / 10)` was computed with the polars backend's integer division
  and printed `hr_per_decade` as `18, 21, 16, 35, …`; it now uses the language's true
  division and prints `17.5609756097561, …`. The column width changes with it.

- `tests__corpus__m5_nan_sort` — ADR 0036 policies 6 and 8, two changes in one file.
  The source bound `nan = sqrt(-1.0)`, which stopped being legal once `nan` became a
  builtin constant (ADR 0027 shadowing), so the file uses the literal now. And the
  answer moved: NaN had sorted FIRST here only because `sqrt(-1.0)` yields a *negative*
  NaN on x86 and the old rule ordered by sign bit. Every NaN sorts last now,
  sign-independently.

`tests/corpus/t9_eq3_tuples.helix` also bound `nan` and needed the same source edit, but
its output did not move (`==` stays IEEE, so `nan == nan` is still `false`) — so it gets
no entry. A source edit is not a behavior change.

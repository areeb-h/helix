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

## v0.9.0 → 0.9.1-dev — the article on a type name

- `tests__corpus__m1b_assign_over_fn` — `` `f` is a Int, not a function `` became
  `` an Int ``. Pinned at both the **v0.5.1 and v0.6.0** baselines, which dates the bug:
  the ungrammatical form shipped in at least those two releases.

  `value::with_article` has always existed for exactly this, and its own doc comment names
  the case — "Every other vowel-initial name here (`Int`, `Array`) takes 'an'". Three
  runtime sites that build this same sentence route through it; the CHECKER
  (`types/synth.rs`) built it with a literal `"a"`, so one program said "an Int" from the
  runtime and "a Int" from the checker.

  Two things about how it survived are worth keeping. The spelling was the **expected
  output** in the corpus golden, so the corpus agreed with it. And a guard for precisely
  this article already existed — `src/vm/tests.rs` asserts that no message says "a Int" or
  "a Array" — but every program it checks goes through `run_vm`, which cannot reach the
  checker at all. A guard watching one of two producers, with the miss recorded as the
  answer. The article is now covered across both families by
  `the_runtime_no_method_error_takes_the_right_article` in `tests/cli.rs`, which runs the
  real binary and therefore type-checks first.

  Message-only: no program's exit code, stdout, or value changed.

- `tests__corpus__d7_where_misquote` — `` type Int has no method `where` `` became
  `` an Int has no method `where` ``, pinned at the **v0.5.1 and v0.6.0** baselines. This
  is the error-family unification, not the article fix above: the CHECKER said "type Int"
  where the RUNTIME said "an Int" for the same refusal, reached by two routes a caller
  cannot choose between — a receiver whose type is known refuses in the checker, and the
  same receiver through a parameter is `Unknown`, so the runtime answers.

  The runtime's form won because the sibling family already spoke it ("`f` is an Int, not
  a function" runs through `with_article` on both sides), which also made the change
  cheap: `docs/dx-plan.md` estimated ~40 pins, and it was four, because most of what grep
  matched was the sentence that did not move.

  Message-only. `a_refusal_reads_the_same_from_the_checker_and_the_runtime` now holds the
  property, and it catches all three producers — verified by sabotaging each in turn.

- `tests__corpus__t11_diag` — `` `fs[0]` expects 1 argument, got 2 `` became
  `` `fs[0]` takes 1 argument, got 2 ``, pinned at the **v0.5.1 and v0.6.0** baselines
  (2026-09-04). The arity refusal now reads `takes` at every layer for every kind of
  function: user functions said `expects` (checker, VM, walker) and builtins said `takes`
  — except the builtins routed through the shared helper, which said `expects` too. One
  helper, one sentence, and the range form (`takes 1 to 2 arguments`) lambda defaults
  needed anyway. Message-only.

# The behavior corpus — one program per verified behavior

Every `.helix` program here is run by `corpus_is_engine_identical_and_pinned`
(`tests/cli.rs`) on **all three engines** — tree-walker (`HELIX_NOVM=1`),
bytecode VM (`HELIX_NOJIT=1`), and JIT (default) — and checked twice:

1. **Engine identity**: exit code, stdout, and stderr must be byte-identical
   across the three engines. This is the differential oracle applied to real
   programs rather than fuzzer output.
2. **Golden output**: the result must equal the checked-in `<name>.expected`
   file (exit code + stdout + stderr, with the absolute source path normalized
   to `<src>`), so behavior cannot drift silently between releases.

Unlike `examples/` (which teaches), this corpus **pins**: each program is the
minimal reproduction of a behavior that was verified — and in most cases, of a
bug that was found and fixed. A future change that re-breaks one fails the test
by name.

## Adding a program

Write the smallest program that exhibits the behavior, then generate its golden:

```sh
UPDATE_CORPUS=1 cargo test --profile gate --test cli corpus_is_engine_identical_and_pinned
```

Keep them **deterministic** (no clock, no RNG without a fixed seed, no network,
no unseeded iteration order), fast (< 1s on the *tree-walker*, the slow
reference engine — it has no memoization and no native kernels), and free of
absolute paths beyond the source file itself.

## Regenerating after an intentional change

The same `UPDATE_CORPUS=1` command rewrites the goldens. **Read the diff**: it is
the exact, program-by-program statement of what your change did to the language's
observable behavior. If a golden changes and you did not intend it, that is the
test doing its job.

## What lives here

| Prefix | Behavior class |
|--------|----------------|
| `c1*`, `c2*`, `c3*` | Closure capture: shadowed bindings (innermost wins), binders over globals, live `mut` globals |
| `d1*`–`d7*` | DataFrame dispatch through untyped/Unknown receivers, `missing` propagation, join arity, verb naming |
| `m1*`–`m6*` | `fn`/global collisions, match guards, nested-`missing` equality, NaN sort totality, `i64::MIN` wrap |
| `r1*`, `t3*`–`t8*` | Recursion: tail-call optimization, the shared depth budget and its exact boundary, lexical scoping, `fn`-rebind rejection |
| `t9*`–`t11*` | Three-valued equality, tuple ordering, duplicate-field rejection, diagnostics |
| `i1*`, `l1*`–`l10*` | Lexer/parser diagnostics: escapes, chained comparisons, literal traps, BOM, interpolation-hole positions |

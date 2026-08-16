# Changelog

## v0.2.4 — 2026-08-16

**The linear-accumulation release** — [ADR 0029](docs/adr/0029-linear-accumulation.md)
implemented in full: building a collection one element at a time in a fold is
amortized-linear **on every engine, for every spelling** — arrays, dicts, and strings —
or it declines to the copy path and stays correct. The v0.2.3 field report measured the
released walker at the O(n²) signature (×2.1 → ×2.8 → ×5.2 ratios); this release is the
answer.

### Performance

- **The tree-walker's fold is linear** — the last engine with the append wall. The VM's
  take-append-store discipline transplanted: 262k array appends **6,768 → 23 ms**
  (294×), 64k dict inserts **17,689 → 16 ms** (1,100×). The walker's reduce also stops
  boxing packed receivers up front just to iterate them.
- **The string-interpolation fold is linear on all three engines** — previously
  quadratic everywhere (`13.6–14.6×` per 4×n; no engine had a fast path). The new
  `AppendStrIntoLocal` op and its walker twin render the tail first, so every fallible
  step happens with the accumulator untouched; a non-string init still formats exactly
  as before; a format spec on the accumulator hole still takes the general path (it
  re-pads, so append would be wrong); growth is fallible (a refused reservation reports
  instead of aborting). After: 4×n costs ~1.8×.
- **Duplicate fold binders `(a, a)` never take the fast paths** (shipped as a
  correctness guard in v0.2.2; the fast paths added here inherit it).

Every fix is pinned two ways: a complexity-class test (n vs 4n in one process — a
quadratic regression fails the gate rather than waiting for a field report) and a
semantics table byte-identical across all three engines *and* against the previous
release — self-referencing arguments, shared inits, scan's snapshot history, mid-fold
error restore, and the string fold's format-spec and non-string-init edges.

## v0.2.3 — 2026-08-15

Two fixes, both found by re-verifying v0.2.2's claims against the installed release —
the survivors of that audit.

### Fixed

- **The module-namespace guard now reaches string interpolation.** v0.2.2 fixed
  `print(mod.position(…))` but `emit("{mod.position(…)}")` still failed for all seven
  comprehension-lowered names — and the interpolated form is the idiomatic print, so the
  fix had missed exactly where users hit the bug first. An interpolation hole is parsed
  by a fresh parser; the import-namespace set now rides into it the same way function
  signatures always did.
- **`helix fmt` no longer indents a column-0 comment into the previous function's
  body.** Trigger: a nested lambda wrapped across lines — its closing brackets sit at
  line *end*, and the indent tracker only unwound *leading* closers, leaving a dead
  step for the next flush-left line to inherit. Dead steps are now discarded per line;
  all 54 example files still reformat to byte-identity.

## v0.2.2 — 2026-08-15

**The discoverability release** — plus the two deepest performance fixes since the
append wall, and four wrong answers removed. Everything below was driven by field
reports from the language's heaviest users (the #19 review and the physics-library
build); every fix is pinned by a test confirmed to fail on v0.2.1 first.

### Fixed

- **`helix test a.helix b.helix` ran only the first file, silently** ("running 1 test
  file") while `helix check` accepted many — anyone verifying two modules in one command
  believed both passed. Every path argument is now a root; files and directories mix.
- **Duplicate fold binders `(a, a)` diverged across engines**: the accumulator fast path
  matched by binder name, so `[[1],[2]].reduce([], (a, a) => a.concat([9]))` answered
  `[9, 9]` on the VM/JIT and `[2, 9]` on the walker — silent, exit 0. All engines now
  agree on the correct last-write-wins answer.
- **Qualified module calls beat comprehension sugar**: seven receiver-blind parse-time
  desugars (`position`, `sort_by`, `take_while`, `drop_while`, `min_by`, `max_by`,
  `zipmap`) intercepted `mymod.position(a, b, c, d)` and rejected a module's own 4-arg
  export. The parser now knows import namespaces; methods on them resolve to the module.
- **`unique()` on a large packed array could abort the process** — the general method
  dispatch boxed the whole buffer first (80M ints → 1.9 GB before one comparison). Now a
  packed fast path with fallible growth: refuses cleanly where memory genuinely runs out,
  and is 15–25% faster where it doesn't.
- **`helix test <file>` on a documented module** now answers what the directory run
  answers, instead of passing its examples and failing the file for asserting nothing in
  one output.

### Performance

- **The i64 map kernel admits affine indices**: `a[2*i]` 486 → 27 ms at 10M (16×), the
  3-point stencil `a[i] + a[i+1] + a[i+2]` 1871 → 31 ms at 20M (**50×**), measured
  against the v0.2.1 release binary. Values bit-identical on all three engines,
  including out-of-bounds and negative-index behaviour.
- **Array-of-strings accumulation is linear**: the fold spelling
  `lines.reduce([], (acc, s) => acc.concat([s]))` went **235.8 s → 61 ms** at 256k
  pieces (a `Values` arm in `concat_in_place`, guarded by a non-numeric witness that
  makes representation change impossible by construction).

### Added

- **`helix.toml` is a real manifest**: `description`, `authors`, `license`,
  `repository`, `keywords` — and `helix = ">=X.Y.Z"`, an **enforced toolchain floor**:
  an older binary opening the project reports *"this project requires Helix >= X, and
  this binary is Y"* once, instead of failing sixty confusing ways on unknown syntax.
  `version` must be comparable MAJOR.MINOR.PATCH. `helix new` writes the full template.
- **`helix describe` carries signatures** derived by probing the checker's own tables —
  accepted arities and per-arity return types (`round`: 1 arg → `Int`, 2 → `Float`),
  `null` where the checker genuinely does not constrain them, never fabricated.
- **`helix doc <name>` reverse lookup**: a method or builtin by name answers with every
  owner type, its effect, and an example receiver — the question users actually arrive
  with, previously answered by "error: unknown type".
- **`expect(k)` on Dict and Record** — the loud lookup: raises at the miss (with a
  one-edit did-you-mean over the collection's own keys) where `get`/`d[k]` keep
  ADR 0001's propagating `missing`.
- **Queries can name missingness**: `where(@v.is_missing())`, its `not` negation, and
  `drop_missing()` on DataFrames. `where(@v == missing)` still selects nothing — that is
  ADR 0001 semantics, and now there is an honest spelling for the real question.
- **The map at the door**: bare `helix` points at `helix help` / `helix doc` /
  `helix describe`; `AGENTS.md` at the repo root carries the commands, the three-engine
  correctness model, and the wrong-answer footguns.
- **Errors teach**: `prefix_sum` steers to `cumsum`/`scan`; an undefined name in a
  string hole explains interpolation and the `{{ }}` escape; `fn` inside `do {}` names
  the rule; unknown methods point at `helix doc <Type>` instead of dumping 79 names;
  reassigning `e`/`pi`/`inf` says what the constant is instead of suggesting a shadow.

## v0.2.1 — 2026-08-15

**The trust release.** No new features and no breaking API: seven wrong answers removed,
every one pinned by a regression test that was first confirmed to fail against the v0.2.0
binary, on all three engines. Three were release blockers found by a ~4,000-program stress
fleet within a day of v0.2.0 shipping; the deepest were on the DataFrame/Polars seam, not
in the language core.

### Fixed

- **Grouped `f64` `sum`/`mean` was nondeterministic** — the same pure expression could give
  a different answer across runs (and even twice within one program), because Polars'
  partitioned group-by merges intra-group values in a scheduling-dependent order. Grouped
  aggregations are now built so they structurally cannot take that path; the regression
  test evaluates the same query 30× in one process. This restores ADR 0020 reproducibility
  and un-blinds the three-engine differential oracle.
- **Grouped aggregations skipped `missing`** (ADR 0001 inverted): a group containing an
  unknown reported a number, and an all-`missing` group reported `sum` `0.0` —
  indistinguishable from a genuine zero. They now propagate `missing` per group, matching
  the array and whole-column paths. `count` is the deliberate exception: it counts rows,
  as `[1.0, missing].count()` always has.
- **Oversized collection materialization could abort the host** (SIGABRT, exit 134, not
  catchable) — the one ADR 0024 violation found. Three causes, all fixed: the tree-walker
  materialized packed arrays up front just to iterate them; the packed builder arms could
  overshoot their budget on `Vec` doubling; and the byte budget was checked only after the
  push. All three engines now refuse with the same catchable error, in the same words.
- **`.sum()`/`.mean()` returned `NaN` where IEEE-754 returns `±inf`** — Neumaier
  compensation went non-finite with the running sum. The compensation (and its accuracy on
  finite data) is kept; the non-finite case now answers what python3 and NumPy answer.
- **`erf` was the only math builtin not computed to double precision** (~1.4e-7 absolute
  error, discontinuous at 0, unbounded relative error near 0). It now matches python3's
  `math.erf` bit-for-bit across the tested grid, and `normal_cdf` routes through `erfc`,
  fixing the left tail — the p-value case — which no `erf` accuracy alone could fix.
- **DNA IUPAC arithmetic**: `gc_content` counted `S` ("G or C" — GC by definition) as
  non-GC, so `dna("S")` read 0.0 and `dna("GCS")` read *lower* than `dna("GC")`. The
  policy, now uniform and documented: a base participates iff its GC-ness is certain
  (`S` is GC, `W` is not; `N R Y K M B D H V` are excluded from numerator and denominator
  alike — the rule `N` always had). A sequence with no classifiable base answers `missing`,
  never a fabricated `0.0`, and that propagates through `mean_gc`, which had been averaging
  all-`N` sequences in as `0.0`.
- **`try 1 + 1` produced a true but useless error** ("got a Record", with no record in
  sight — it parses as `(try 1) + 1`). The error now explains that `try` binds tighter
  than the operator and shows the parenthesized fix. Ordinary record operands keep the
  ordinary message.

### Hardened

- **`sort` tie order is now a contract, not an accident**: DataFrame `sort` pins
  `maintain_order`. No misbehaviour was ever reproduced (300k rows, int and string keys),
  but unstable tie order is unspecified and every `.column()` re-executes the lazy plan,
  so the pin makes the stability ADR 0020 assumes survive any Polars upgrade.

## v0.2.0 — 2026-08-13

**The consistency release.** Every breaking change this project has decided is in this one
version, so upgrading is one deliberate step instead of a drip. Each break below names the
ADR that argued it; each was landed with its blast radius measured first, and the ordering
changes are specified cell-by-cell by `tests/ordering_matrix.rs` (247 pinned
expression/answer pairs, identical on all three engines).

The theme is one sentence: **a library author must be able to write correct code without
knowing anything about the caller** — not their schema, not their manifest, not which
builtins a future Helix adds.

### Breaking

- **One order, one domain** ([ADR 0025](docs/adr/0025-ordering.md), all four questions).
  `sort`, `argsort`, `sort_by`, `min`/`max`, `min_by`/`max_by` and `argmin`/`argmax` now
  agree about what can be ordered and what `missing` means:
  - `argsort` (and therefore `sort_by`) adopts `sort`'s policy: an array with `missing`
    **errors** with the same message and hint, and DNA orders. Before, `xs.sort()` and
    `xs.sort_by(it)` disagreed with each other.
  - `min`/`max` widen to `sort`'s type domain: strings and DNA now reduce
    (`["b","a"].min()` is `"a"`), so `min() == sort().first()` holds everywhere. The
    reduction policy is unchanged: `missing`/NaN still propagate as `missing`.
  - `min_by`/`max_by` and the **method** `argmin`/`argmax` adopt the reduction policy:
    `missing`/NaN propagate instead of raising leaked internals (`` `if` condition is
    `missing` ``, `index 0 is out of bounds`), and the empty array gets a named error.
    `xs.argmax()` and `argmax(xs)` now give the same answer to the same question.
  - Signed-zero ties on `argmin`/`argmax`/`min_by` remain IEEE first-wins, now documented
    with runnable examples in `examples/language/ordering.helix` (the gate executes them
    on all three engines, so the documentation cannot drift).
- **In a DataFrame query, a bare name means the binding, not the caller's column**
  ([ADR 0028](docs/adr/0028-query-name-resolution.md)). `fn above(frame, cutoff) =
  frame.where(@value > cutoff)` no longer changes meaning when the caller's data happens
  to have a `cutoff` column. `@name` still pins the column side; a bare name with no
  binding in scope is still a column. This was the last known silent wrong answer.
- **A top-level `fn` is file-scoped** ([ADR 0027](docs/adr/0027-builtin-shadowing.md)).
  It is callable above its own definition, and shadowing a builtin is retroactive for the
  whole file — a name means one thing per file. This removed a silent three-engine
  divergence, and it is what lets future Helix releases add builtins without breaking
  published libraries that already use those names.

### Fixed

- **A consumer's dependency can no longer capture a library's private import.** A file's
  own sibling now wins over a dependency key, so installing a package named `helpers`
  cannot rewire another library's `import helpers`. The failure was silent (exit 0,
  `check` ok, all three engines agreeing) and is now pinned by the one test able to see it.
- **Growing an array or dict in a fold is linear.** 256k `acc.concat([x])` appends went
  6.49s → 0.04s (158×) and 4M run in 0.25s; `Dict.insert` follows. The accumulator is
  taken, extended and stored as one VM instruction, so nothing observes it mid-move.
- **A `reduce` body can call a user function with a float signature** — 1.84s → 0.05s at
  n=20M (the kernel used to decline outright). A `/0` or NaN comparison inside the callee
  still raises the exact interpreter error.
- Mutual recursion and forward references compile and run on all three engines, with
  mutual tail calls constant-space to 1M frames.

### Added

- `raise(message[, help])` — a library can reject an argument in its own words, with a
  `help:` line, instead of `assertion failed: …`. Caught by `try` like any error.
- `source_path(rel)` — resolves against the directory of the file the call is written in,
  so a package can ship and read its own data regardless of the process working directory.
- `chars()` — a string as an array of characters (Unicode scalars). The previous
  spelling, `replace("", "\t").split("\t")`, was both undiscoverable and wrong.
- `helix test` runs your `## >>>` doc examples on all three engines and fails a test file
  that asserts nothing. A facade re-export (`export f = lib.f`) keeps defaults and named
  arguments. `helix verify` hashes a dependency's data files, not just its source.

### Upgrading

If your code never shadows a builtin, never names a `min_by` parameter after a DataFrame
column, and never sorts arrays containing `missing`, v0.1.1 programs run unchanged. The
repository's own corpus, examples and benchmarks needed **zero** edits beyond the ordering
test matrix itself; the one fixture that deliberately pinned the old shadowing behaviour
was regenerated.

## v0.1.1 — 2026-08-11

**The first installable release.** `v0.1.0` is published but nobody can install from it, and
this release exists to replace it rather than to add anything.

### What was wrong with v0.1.0

Probed against the live release: `helix-x86_64-unknown-linux-gnu.tar.gz` → **404**,
`SHA256SUMS` → **404**. Four of six platforms uploaded, and the installers refuse to install
what they cannot verify, so even those four were unreachable. Three independent causes:

- The profile-guided Linux build died in its training step on
  `bench/crosslang/b3_groupby.helix`, which still called `io.read_csv(…)` — a spelling
  ADR-0017 removed. **Nine** benchmark programs had rotted the same way; nothing in CI ever
  compiled them, because every gate the project had *runs* its programs and these need a
  250 MB generated fixture first.
- Six build jobs each called `action-gh-release`, which creates-if-missing, so six of them
  raced to create one release. The musl job died mid-upload.
- The checksum job was skipped for a failed dependency, and the incomplete release went
  public anyway.

### Fixed

- **The release is built as a draft and published last.** One job creates it, every build
  uploads into it, and publishing is gated on all six platforms plus `SHA256SUMS` being
  present. A failure now leaves an invisible, re-runnable draft instead of a broken public
  artifact.
- **Every build smoke-tests the binary it produced** where the runner can execute it, and
  the musl job asserts the artifact is genuinely static — the one machine that would
  otherwise discover it is the air-gapped one with no way to fix it.
- **`workflow_dispatch` takes a `dry_run` input** (default on) that builds and smoke-tests
  all six platforms while writing nothing, so the pipeline can be checked without publishing
  something to check it with.
- The nine stale benchmark programs are repaired and verified by running them.

### New

- **`helix check <script>…`** — type-check without running or writing anything. Takes many
  paths in one process; `scripts/checkall.sh` covers all 85 tracked programs in ~0.03 s and
  runs in CI, which is what closes the gap that let the benchmarks rot. It never executes
  the program, and it is honestly *only* a type check: code that checks clean can still fail
  at run time.

### Toolchain and hygiene

- All five CI jobs now block. Clippy is at zero warnings with `-D warnings` (no `#[allow]`
  suppressions); MSRV 1.96 is verified rather than asserted; `cargo audit` is green, with two
  advisories fixed by upgrade (crossbeam-epoch, quinn-proto) and four that have no reachable
  fix documented in `.cargo/audit.toml`, each with the crate that blocks it.
- `CONTRIBUTING.md` added.
- The Docker build context went from 1.3 GB to 9 MB — `.dockerignore` was not excluding
  `website/node_modules`, `website/.next` or the generated benchmark fixtures.

**No language changes.** A `v0.1.0` program runs identically on `v0.1.1`.

## v0.1.0 — 2026-08-11

First tagged release. Incomplete — see above; use `v0.1.1`.

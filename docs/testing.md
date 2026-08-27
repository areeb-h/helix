# Testing

Helix has a test runner and assertions built into the toolchain — no framework to
install, no configuration. Name a file `*_test.helix` and `helix test` runs it.

## Assertions

Three built-in guards raise a catchable error when they fail (and are silent when they
pass):

| Builtin | Passes when | On failure |
| --- | --- | --- |
| `assert(cond)` / `assert(cond, msg)` | `cond` is `true` | `assertion failed[: msg]` |
| `assert_eq(a, b)` | `a` equals `b` | `assertion failed: <a> != <b>` |
| `assert_close(a, b)` / `assert_close(a, b, tol)` | `\|a - b\| <= tol` (default `1e-9`) | `assertion failed: <a> is not within <tol> of <b>` |

`assert_close` is for floating-point results, where exact `==` is a footgun
(`0.1 + 0.2 != 0.3`). To check for missing data, use `x.is_missing()` rather than
`assert_eq(x, missing)`.

Because a failed assertion is an ordinary raised error, it is caught by `try` and exits
the program non-zero when uncaught:

```helix
r = try assert_eq(total, 100)
print(r.ok)        # false if it failed
```

## Raising a domain error

An assertion says *a check failed*. A library rejecting its caller's argument is saying
something else, and `raise(message)` — with an optional second argument for the `help:`
line — says it in the library's own words:

```helix
export fn go(path) =
  if path.starts_with("/") then path
  else raise("route path must start with '/'", "pass a path like \"/admin\".")
```

```text
error: route path must start with '/'
  --> route.helix:3:8
  |
3 |   else raise("route path must start with '/'", "pass a path like \"/admin\".")
  |        ^
help: pass a path like "/admin".
```

Written with `assert`, the same rejection reads `assertion failed: route path must start
with '/'` — which tells the caller the library is broken rather than that their argument
was wrong, and has nowhere to put the fix. A raised error is an ordinary error: `try`
catches it, and it exits non-zero when uncaught.

## Writing and running tests

A test file is a normal Helix file named `*_test.helix`. It can `import` your project's
modules (imports are anchored at the project root, so a test under `tests/` can import a
module at the root by its name). A test "function" is just a function you call:

```helix
# math_test.helix
import math

fn test_double() = assert_eq(math.double(21), 42)
fn test_negatives() = assert_eq(math.double(-3), -6)

test_double()
test_negatives()
```

Run them:

```
$ helix test
running 1 test file
  ok    math_test.helix

1 passed
```

`helix test [path]` discovers every `*_test.helix` under `path` (default: the current
directory), runs each file in isolation through the normal pipeline, and reports
per-file results plus a summary. A file **passes** if it runs to completion without
raising; the first failing assertion stops that file and prints the error (with its
caret), and the runner exits non-zero if any file failed — so it drops straight into CI.
Pass a single file (`helix test math_test.helix`) to run just that one.

## The Rust-side gate

The toolchain's own suite runs through `scripts/gate.sh` (clippy with `-D warnings`,
the full test suite on the `gate` profile, the dual-engine DataFrame campaign, and the
example parity diff). Its current shape: **454 lib tests + 257 CLI tests + 3 other
integration tests**, plus **32 dual-engine tests** that need the `native-df` feature.
Five pieces worth knowing about:

- **The differential DataFrame suite** (`src/backend/native/tests.rs`): the same data
  through **both** DataFrame backends — native and Polars — verb by verb, compared as
  column values and as frozen `framefmt` bytes. ADR 0034's decided semantic deltas are
  asserted *as* deltas, so an accidental divergence cannot hide behind a decided one.

  This paragraph was **false until v0.6.0**, and the way it was false is worth keeping
  written down. `native-df` is not in `Cargo.toml`'s `default`, `scripts/gate.sh` ran a
  bare `cargo test`, and CI's only `native-df` step was a `clippy` *without*
  `--all-targets` — so the test targets were never compiled. All 28 tests, including
  every `mod against_the_oracle` comparison, were written, reviewed, committed, and then
  executed by nothing, while this document told readers they ran. A suite that does not
  run is worse than no suite, because it also spends the confidence. The gate now runs
  them in their own target directory (the feature set differs from the main build, so a
  shared directory would make the two invocations evict each other's cache), and CI runs
  the full `native-df` suite on a compile it was already paying for.
- **Cross-backend program diffs** (`scripts/dfdiff.sh`): every tracked `.helix` run under
  *both* DataFrame backends, byte-compared, with any accepted divergence declared in
  `scripts/dfdiff-allow.txt` — currently 119 programs and **0 undeclared divergences**.
  The suite above tests the backends verb by verb; this one tests them the way a user
  meets them, through whole programs. It exists because verb-level parity was green while
  sixteen divergences were live: the deltas hid in expression *shapes* no verb test built
  (a division by a literal, two `.where()`s in a row, a NaN reaching a grouped aggregate).
  Its predecessor, `scripts/dfcheck.sh`, was worse than absent — it ran a path that had
  moved, so it diffed three copies of "no such file" and reported them identical while
  ADR 0033 cited it as acceptance evidence. A gate that cannot fail is not a gate.
- **Version-compatibility baselines** (`tests/compat/`): what a *released* version
  actually computed — exit code, stdout, and stderr for 119 deterministic programs —
  frozen and **never rewritten**. Every other gate here compares the tree against
  itself and therefore proves only consistency; this is the only one that can answer
  "does the program I wrote six months ago still compute the same number?". There is
  deliberately no environment variable that blesses a drift: an intentional change is
  recorded in `tests/compat/MIGRATIONS.md` with its reason, and that file accumulates
  into a checkable list of every user-visible behavior change. See
  [`tests/compat/README.md`](../tests/compat/README.md).
- **Cross-engine example byte-diffs** (`scripts/vmparity.sh`): every runnable example
  must produce byte-identical output on the default engine and under `HELIX_NOVM=1`.

  Until 2026-08-27 this one **could not fail**. It ended with `echo "RESULT=$fail"` and
  no `exit`; nothing in the repo parsed `RESULT=` (that echo was its only occurrence);
  `gate.sh` piped it to `tail` without `|| rc=1`; and CI ran it as a bare step, which
  passes whenever a script exits 0. A divergence across every example would have printed
  `DIFF …`, printed `RESULT=1`, and left both green — while this document listed it as a
  gate.

  That was the **third** gate here found unable to fail, after `dfcheck.sh` (diffed three
  copies of "no such file") and the 28 `native-df` tests (executed by nothing). Three
  times is a pattern, so it is written down as a rule: **a gate has to be sabotaged once
  to prove it can fail.** Add the check, then break the thing it watches and confirm the
  build goes red. An untested gate is a claim, not a check.
- **Whole-tree type-check** (`scripts/checkall.sh`): `helix check` plus `helix fmt
  --check` over every tracked `.helix` outside `tests/corpus/`, covering programs the
  running gates cannot start (they need generated fixtures).

## Testing the compiler itself

Beyond `helix test`, the repo carries generators that attack the toolchain rather than a
program written in it.

### `scripts/stranger.py` — files a newcomer would type

```
python3 scripts/stranger.py                 # 1438 programs, ~15-25s
python3 scripts/stranger.py --selftest      # prove each oracle can still fail
python3 scripts/stranger.py --list          # print the corpus without running it
python3 scripts/stranger.py --bless         # re-record the oracle-4 baseline
```

Every other generator here emits **well-formed Helix**. `scripts/opfuzz.py` walks
(operator x operand x compilation-shape), and every program it emits parses. The
differential fuzzers in `src/vm/tests.rs` build ASTs directly, so they cannot produce a
syntax error even in principle. `tests/corpus` is hand-written Helix. All three start from
the Helix grammar, which means none of them can emit `for x in xs:`, a `/* block comment
*/`, a file that opens with a UTF-8 BOM, a source line with a NUL byte in it, or a
DataFrame built from a **file on disk**. That last gap is not hypothetical: the
`read_csv(f).where(1)` SIGABRT lived exactly there, in a dependency, reachable only from a
real file, invisible to every generator we had.

`stranger.py` closes that gap by generating whole *files* the way someone who does not
know Helix types them — the cross product of Python, JavaScript, R, MATLAB, Julia, Go and
C habits (statement forms, null literals, booleans, length idioms, comment syntax, string
kinds, builtin guesses, indexing, operators) over five syntactic positions, plus two axes
no grammar-driven generator can reach:

* **bytes** — BOM, CRLF, lone CR, no final newline, NUL, invalid UTF-8, Latin-1, tab
  indentation, non-breaking spaces, an empty file, and 4KB single lines.
* **files** — 22 malformed CSVs (ragged, unbalanced quotes, duplicate headers, NUL,
  invalid UTF-8, BOM, wrong delimiter, integer overflow) crossed with the 16 things a
  newcomer does to a frame the moment they have one, plus paths that are a directory,
  `/dev/null`, an empty string, or nothing at all.

Four oracles judge the output, in this order of importance:

| # | Oracle | Gate | What only this can catch |
| --- | --- | --- | --- |
| 1 | **NEVER-ABORT** — exit in `{0,1}`, no `panicked at` / `internal error` / `Aborted` | hard | a panic *inside a dependency* |
| 2 | **DETERMINISM** — 3 runs x 3 engines, all 9 `(exit, stdout, stderr)` triples byte-identical | hard | non-deterministic error text; engine parity breaks |
| 3 | **NO-LEAK** — stderr free of host paths, `.rs:LINE`, Polars query plans, `$`-prefixed desugaring names | hard | the toolchain showing through the error |
| 4 | **HAS-HELP** — exit 1 carries a `help:` line, and it is not the canned "no `;`" line on a source with no `;` | **ratchet** | an error message that is correct but useless |

Oracle 1 is the *dynamic* complement to `no_new_panicking_calls_on_user_reachable_paths`
in `tests/cli.rs`. That test counts panicking **calls** in `src/`; this one counts
panicking **runs**. The budget cannot see a panic inside polars or memchr, because those
lines are not in `src/`. This can.

Oracle 4 fails on hundreds of programs today, so it is a **ratchet**, not a ban — the same
argument the unwrap budget makes for its ~90 `.unwrap()`s. Those calls are not sloppiness
and banning them outright would buy no safety; what matters is that the number cannot
silently **grow**. Same here: today's bad help lines are a backlog, not a regression, and
the useful property is that a change cannot add one. The count lives in
`scripts/stranger-baseline.json` and, like the unwrap budget, the harness also fails when
the count **drops** — a ratchet only ratchets if it tightens when the code improves.

Two deliberate exclusions. The corpus never generates `http_*`, `listen`, `random*`,
`clock_monotonic` or `sleep`: oracle 2 would flag them and be *right*, and a determinism
oracle is only meaningful over programs that are supposed to be deterministic. And stdin
is `/dev/null` for every run, for the same reason `scripts/gate.sh` needs `< /dev/null`.

`--selftest` exists because an oracle that never fires is indistinguishable from an oracle
that is broken: it feeds each oracle a result that must fail (including one genuinely
non-deterministic program, `clock_monotonic()`, kept out of the corpus for exactly this
purpose) and checks that each one notices. An empty run is a failure too — a typo'd
`--filter` would otherwise report "0 failing programs" four times and exit 0, which is the
shape of a gate that stops gating without anyone noticing.

**It runs in CI**, as a step of the `test` job — 15 seconds against a 25-minute job, and it
needs the binary that job just built.

### `scripts/opfuzz.py` — operators against their guards

Runs every (operator x stress-operand x compilation-shape) combination through all three
engines, checking that the process exits cleanly and that the tree-walker, the VM and the
JIT agree byte for byte. The operands sit on the guards: zero and negative divisors,
`i64::MIN`, and shift counts at and past the `0..=63` boundary.

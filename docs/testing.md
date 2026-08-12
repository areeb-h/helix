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
purpose) and checks that each one notices.

### `scripts/opfuzz.py` — operators against their guards

Runs every (operator x stress-operand x compilation-shape) combination through all three
engines, checking that the process exits cleanly and that the tree-walker, the VM and the
JIT agree byte for byte. The operands sit on the guards: zero and negative divisors,
`i64::MIN`, and shift counts at and past the `0..=63` boundary.

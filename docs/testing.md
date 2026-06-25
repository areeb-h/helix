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

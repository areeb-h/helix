# Comments and documentation

Helix has two comment forms and one rule that makes them worth distinguishing:

| form | meaning | verified? |
| --- | --- | --- |
| `#` | an ordinary comment — a note to whoever reads the line | no |
| `##` | a **doc comment** — describes the definition that follows | yes, if it contains examples |

Both are lexically identical (everything after `#` to end of line is skipped), so `##` costs
nothing and breaks nothing. The distinction is a convention the *tooling* enforces, not a
new syntax.

## The rule that makes this different from Python

**A documented example is executed, on all three engines, every time the gate runs.**

```helix
## The reverse complement of a strand: complement each base, then reverse — so the
## result reads 5'->3' along the opposite strand.
##
##     >>> dna("ATGC").reverse_complement()
##     GCAT
fn rc(s) = s.reverse_complement()
```

That block is not decoration. `doc_examples_run_and_agree_on_all_three_engines`
(`tests/cli.rs`) extracts every `>>>` line, runs it, and compares the result against the
expected output written beneath it — under the tree-walker, the bytecode VM, and the JIT.
A doc comment whose example has drifted **fails the build**.

Python's docstrings rot silently. `doctest` exists, but it is opt-in, lives outside the
normal test path, and most projects never wire it up; nothing checks that the prose still
matches the code. The difference here is not that Helix has a fancier comment character —
it is that **the example is a test, and it is a test on three independent implementations
at once.** Every documented example therefore also strengthens the differential oracle,
which is the property the whole language is built around. Python cannot offer that, because
there is only one CPython.

The practical consequence: write the example you would have written anyway, and it becomes
a regression test for free.

## Writing an example

An example is one or more `>>>` lines inside a `##` block. The lines run in order, in the
context of the file that contains them, so an example may call the very thing it documents.
The output of the **last** `>>>` line is compared against the plain lines that follow it.

```helix
## Fisher's exact test on a 2x2 table, returning the two-sided p-value.
##
##     >>> tbl = [[8, 2], [1, 5]]
##     >>> fisher_exact(tbl).round(4)
##     0.0350
fn fisher_exact(tbl) = ...
```

Rules, all of them checked by the verifier:

- Expected output is compared **exactly**, after trimming trailing whitespace. If a value
  prints as `0.35000000000000003`, write that, or `.round(4)` it in the example. Do not
  round in the prose and pretend.
- An example that produces no output needs no expected lines.
- An example expected to FAIL is written with its error text, which is checked the same
  way — errors are part of the interface:

  ```helix
  ##     >>> [1, 2].sort_by()
  ##     error: `sort_by` takes exactly one key function
  ```

- Examples run in file order and share nothing between blocks. State a block needs, it
  sets up itself.

## What belongs in which form

Use `##` for what a reader needs in order to *use* the thing: what it computes, what the
arguments mean, what it does at the edges (empty input, `missing`, ties), and an example.

Use `#` for what a reader needs in order to *change* the thing: why this algorithm, what
was tried and rejected, which invariant the next line depends on, and what measurement
justified an optimisation.

The second kind matters more here than in most codebases, and it is worth saying why:
several defects in this project survived for months because a comment recorded an intent
that the code did not implement. `numeric_cmp` claimed `total_cmp` places `NaN` "after
`+inf`, as numpy does" and named `sqrt(-1.0)` as the example — but that NaN has its sign
bit set and sorts to the *front*. The comment was wrong on its own example, and nothing
could catch it, because prose is not executable.

So: **if a comment makes a claim about behaviour, make it an example instead.** Prose
should explain *why*; examples should assert *what*. When a comment says "this returns X",
that sentence is a test that has not been written yet.

## Anti-patterns

```helix
# bad — restates the code, cannot rot because it says nothing
x = x + 1   # add one to x

## bad — a behavioural claim in prose, unverifiable, and this one is FALSE
## Returns the smallest element; ties go to the first.
fn smallest(xs) = xs.min()

## good — the claim is the example, so it cannot drift
##     >>> [3, 1, 2].min()
##     1
fn smallest(xs) = xs.min()
```

The middle case is not hypothetical: `min` on `[0.0, -0.0]` does *not* reliably give the
first element — it depends on the array's representation (see `docs/ROADMAP.md`). A prose
claim would have shipped that; an example would have caught it.

# Comments and documentation

Helix has three comment forms and one rule that makes two of them worth distinguishing:

| form | meaning | verified? |
| --- | --- | --- |
| `#` | an ordinary comment — a note to whoever reads the line | no |
| `##` | a **doc comment** — describes the definition that follows | yes, if it contains examples |
| `#[ … ]#` | a block comment — spans lines, and nests | no |

`#` and `##` are lexically identical (everything after `#` to end of line is skipped), so
`##` costs nothing and breaks nothing. The distinction is a convention the *tooling*
enforces, not a new syntax.

> **An example only runs if it is on a `##` line.** `helix test` reads `>>>` examples out
> of `##` doc comments and nowhere else, so
>
> ```helix
> # >>> dbl(21)
> # 42
> export fn dbl(x) = x * 2
> ```
>
> is a comment that executes nowhere. Until v0.9.0 that also *satisfied* `check --lint`,
> which counted a `>>>` on any comment line — so a codebase commenting with `#` throughout
> could clear every finding and end up with a green lint over examples nothing runs, which
> is the one thing the rule exists to prevent. The lint requires `##` now and says so in
> the message. A plain `#` line BETWEEN the example and the definition is still fine: that
> is prose, and the extractor skips it too.

`#[ … ]#` is the one that is genuinely different: it runs until its matching `]#`, across
as many lines as you like, and it **nests** — so a region that already contains a block
comment can be commented out whole, which is the case the non-nesting version of this
syntax gets wrong in most languages that have it.

```
#[
  A module header, without a `#` on every line.
  Several paragraphs, if it needs them.
]#

print(1 + #[ inline, too ]# 2)

#[
  Commenting out a region that already has comments:
  #[ this inner one does not end the outer block ]#
  print("not run")
]#
```

Two details, both consequences of newlines meaning something in Helix:

* A block comment that crossed lines **leaves its line break behind**. `a = 1` and `b = 2`
  separated only by a multi-line comment stay two statements — a comment can never change
  what a program means.
* `helix fmt` reproduces a block comment byte for byte, including its indentation, and does
  not add or remove a line around it. The author owns the vertical; fmt owns the horizontal.

A doc comment is still `##` per line: `#[ ]#` blocks are not scanned for `>>>` examples,
because a doc comment is attached to the definition it precedes and is read line by line by
the extractor.

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

That block is not decoration. `helix test` extracts every `>>>` line, runs it, and compares
the result against the expected output written beneath it — under the tree-walker, the
bytecode VM, and the JIT. A doc comment whose example has drifted **fails the build**.

This holds for **your** code, not just Helix's own: `helix test` runs the examples in the
modules under the path you give it, with no framework to install and nothing to wire up.
(Helix's own source is checked by the same extractor from
`doc_examples_run_and_agree_on_all_three_engines` in `tests/cli.rs` — one implementation,
included by path, because two would drift and a drifted example-finder reports success
forever.)

Examples are taken from files that contain **only definitions** — the same property that
makes a file importable. Running a module to set up its example cannot re-send an email or
rewrite a file, and its own output is empty, so nothing has to be subtracted from the
example's. A script with top-level statements is skipped, and `helix test` says so rather
than passing over it quietly.

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
- Because an example runs **in its file's scope**, a setup binding must not collide with a
  name the file already binds — bindings are immutable, so it fails with "`name` is
  immutable and cannot be reassigned" rather than shadowing. The verifier catches this the
  first time you run it; the fix is to pick another name.

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

## Two doc-example rules worth knowing (2026-08-24)

- **An example LINE is one statement — there is no `...` continuation marker**
  (the failure message says so too). But a `>>>` BLOCK is a multi-line program:
  consecutive `>>>` lines run together, and only the last line's value is
  compared against the expectation.
- **That makes an import preamble just work.** A module's doc example may bring
  in a sibling the module itself does not import:

  ```
  ##     >>> import message as m
  ##     >>> m.user("hi").role
  ##     user
  ```

  The synthesized program is the module source plus the block's lines in order,
  so relative imports resolve exactly as a caller's would.

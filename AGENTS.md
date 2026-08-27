# Working on / with Helix

Helix is self-describing — ask the binary before guessing:

```
helix search <term>       # FIND a capability by what it does — searches names,
                          #   signatures, docs AND notes. START HERE when you do
                          #   not know the name; `--json` for tools
helix describe <Type>     # ONE type's whole method table as JSON — sig, doc, example,
                          #   effect. ~6% of the full dump: ask this, not the catalog
helix describe <name>     # one builtin/method entry as JSON (every owner of the name)
helix describe            # the WHOLE API as JSON — 120 KB, rarely what you want
helix doc <Type>          # the same table, printed for a human
helix doc <name>          # a method or builtin by name: owners + an example receiver
helix doc builtins        # every free function
helix check file.helix    # fast type-check; never rejects a runnable program
helix check --json f.helix # the same diagnostics as data: line, col, message, hint,
                          #   AND the rendered prose. `--lint` notes come through too
helix jit-explain f.helix # which numeric kernels compiled, by line (`--json` too)
helix test <dir>          # runs *_test.helix files AND every `## >>>` doc example
helix test --engines <dir> # …and re-runs each on all three engines, failing on any
                          #   disagreement. Nothing else can check this; use it in CI
helix eval "print(1 + 2)" # one-liner
helix run tool.helix --n 3 # a script's own args bind to its `fn main` (ADR 0037)
helix run tool.helix --help # generated from `fn main` + its `##` doc; does NOT run it
helix fmt file.helix      # token-stream formatter; provably cannot change a program
```

The right loop for generated code is **generate → `helix check` → run**. If a method
might not exist, `helix doc <name>` answers in one command — this project's costliest
mistake was months of building around a "missing" `scan` that `helix doc Array` printed
all along.

Ask `helix search <word>` when you do not know the name, and `helix describe <Type>`
before writing against an unfamiliar type. Those are the two questions you have *before*
you know any names, and both cost a few KB rather than the 120 KB catalog. This is not a
hypothetical: two field reports independently resorted to dumping that catalog and
grepping it, and the words they had ("repeated header", "group by") were never the names
they needed (`get_all`, `frequencies`) — which is why `search` reads the docs and notes,
not just names. And read the diagnostics — in a 14-case sweep of the mistakes agents
actually make, **eleven named the exact fix** (`to_json(x)` answers "``to_json`` is a
method: `x.to_json()`"; a C-style body answers ``fn f(x) = x + 1``). Reading the error
is usually faster than reading anything else, this file included.

## The correctness model

One program, one answer, on three engines: the tree-walker (`HELIX_NOVM=1`), the
bytecode VM (`HELIX_NOJIT=1`), and the Cranelift JIT (default). They must agree
**byte-identically** — values *and* error text. Differential check:

```
helix run p.helix > a.out; HELIX_NOJIT=1 helix run p.helix > b.out; HELIX_NOVM=1 helix run p.helix > c.out
cmp a.out b.out && cmp a.out c.out
```

**Any divergence is a Helix bug. Report it; never code around it.**

## Syntax that trips up agents

- Function bodies use `=`: `fn double(x) = x * 2`. Multi-statement bodies use `do { }`.
- `if c then a else b` is the expression form — no ternary, no parenthesized `if (c)`.
- **`match` exists** and is what a long `else if` ladder on one value should be:
  `match n { 0 => "none", x if x > 10 => "big", _ => "small" }` — literal arms, an
  optional `name if cond` guard, `_` for the rest, commas between.
- **`"""raw strings"""` do not interpolate and have no escapes.** THE form for a regex
  (`{4}` in an ordinary string is interpolation and silently becomes the number 4 —
  `helix check` refuses that), a Windows path, or any text with braces or backslashes.
- Strings have no `+`: use interpolation `"{a}{b}"` or `parts.join("")`. Both are linear.
- `{ }` in a string is interpolation; a literal brace is `{{`.
- `try` binds tighter than operators: `try (a + b)`, never `try a + b`.
- One binding per line; `do { }` separates statements by newline, never `;`.
- `fn` is item-level only — inside `do { }` bind a lambda: `f = (x) => …`.
- A function value in a record field is called parenthesized: `(rec.f)(x)`.
- Imports are `import lib.stats` (for `lib/stats.helix`), `import lib.stats as st`, or
  `import lib.stats.{mean, sd}` to bring names in unqualified. Not `use`, not
  `from … import …`.
- **`mut` is top-level only, and this is a design question, not a spelling one.** A
  function body evolves state by *rebinding* — `do { n = 0` / `n = n + 1` / `n }`, each
  line shadowing the last — and state that crosses a sequence is *threaded*, with
  `reduce`. Reach for `mut` only for state that must outlive a call. Deciding this after
  writing an imperative loop means rewriting the program, which is why it is here rather
  than left to the (very explicit) error.
- Most things are methods on a receiver, not free functions: `x.to_json()`,
  `s.parse_json()`, `xs.join(", ")`. `helix doc <name>` answers which in one command,
  and calling a method as a function tells you so by name.
- **Three ways to find something, in the order to try them.** `helix search <words>` when
  you know the JOB but not the name ("repeated header", "count occurrences", "session") —
  it covers builtins, methods AND the language forms above, every word has to match, and
  each row says which field it matched. `helix doc <Type>` for everything one receiver can
  do. `helix describe <name>` for one full entry, syntax included (`helix describe match`).
  Not finding something here is worth one more query before concluding it is absent: this
  project's costliest recorded mistake was months of building around a `scan` that
  `helix doc Array` printed all along.

## Footguns — wrong answers, not errors

1. **Filtering on `missing` finds nothing, silently.** `where(@v == missing)` returns
   0 rows because `missing == missing` is `missing`. The keep-non-missing idiom is
   `where(@v == @v)`; `drop_missing` (on Array and DataFrame) is the explicit form.
2. **i64 arithmetic wraps silently** — `9223372036854775807 + 1` is min-i64, exit 0.
   Deliberate; see `docs/integer-semantics.md`.
3. **`sum()` and its `reduce` spelling diverge at the i64 edge**: `sum()` widens to
   float where the reduce wraps. Documented divergence, same doc.
4. **Float `==` is exact**: `[0.1, 0.2].sum() == 0.3` is `false`. Use `assert_close`.
5. **Falling off a JIT kernel is silent** — same answer, much slower. If a hot loop is
   slow, suspect the shape, not the math: `helix jit-explain prog.helix` lists every
   kernel site the compiler offered the JIT, with its line and whether native code was
   generated, PLUS the functions compiled whole (a tail-recursive numeric function is
   native but is not a kernel site). A comprehension whose line is absent was never
   offered at all. It reports
   what the JIT was asked and what it answered, **not yet why** a shape was refused.

A lookup miss is *distinguishable* on request: `d.expect(k)` raises where `d.get(k)`
and `d[k]` return `missing` (ADR 0001 keeps the propagating default).

## Packaging

`helix.toml` is a full manifest — `helix new <name>` writes the template:

```toml
[package]
name = "physics"
version = "0.1.0"            # must be MAJOR.MINOR.PATCH — it is an ordering claim
description = "Orbital mechanics and physical constants"
authors = ["You <you@example.com>"]
license = "MIT"
helix = ">=0.2.1"            # toolchain floor — ENFORCED with one clear error
[dependencies]
```

The `helix` floor is the field to always set: an older binary opening the project says
*"this project requires Helix >= X, and this binary is Y"* once, instead of failing
sixty confusing ways on syntax it doesn't know. Dependencies are hash-pinned in
`helix.lock` (`helix sync` / `helix verify`).

## For contributors

- Gate before any commit: `bash scripts/gate.sh < /dev/null` (the stdin redirect is
  mandatory). **Never run `cargo fmt`.**
- Every fix ships with a regression test confirmed to FAIL on the previous binary
  first, on all three engines.
- Design decisions live in `docs/adr/`; read the ADR before changing a semantics.

# Working on / with Helix

Helix is self-describing — ask the binary before guessing:

```
helix describe            # the WHOLE API as JSON (names, effects) — built for agents
helix doc <Type>          # methods on Array / String / Dna / DataFrame / …
helix doc <name>          # a method or builtin by name: owners + an example receiver
helix doc builtins        # every free function
helix check file.helix    # fast type-check; never rejects a runnable program
helix test <dir>          # runs *_test.helix files AND every `## >>>` doc example
helix eval "print(1 + 2)" # one-liner
helix fmt file.helix      # token-stream formatter; provably cannot change a program
```

The right loop for generated code is **generate → `helix check` → run**. If a method
might not exist, `helix doc <name>` answers in one command — this project's costliest
mistake was months of building around a "missing" `scan` that `helix doc Array` printed
all along.

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
- Strings have no `+`: use interpolation `"{a}{b}"` or `parts.join("")`. Both are linear.
- `{ }` in a string is interpolation; a literal brace is `{{`.
- `try` binds tighter than operators: `try (a + b)`, never `try a + b`.
- One binding per line; `do { }` separates statements by newline, never `;`.
- `fn` is item-level only — inside `do { }` bind a lambda: `f = (x) => …`.
- A function value in a record field is called parenthesized: `(rec.f)(x)`.

## Footguns — wrong answers, not errors

1. **Filtering on `missing` finds nothing, silently.** `where(@v == missing)` returns
   0 rows because `missing == missing` is `missing`. The keep-non-missing idiom is
   `where(@v == @v)`; `drop_missing` (on Array and DataFrame) is the explicit form.
2. **i64 arithmetic wraps silently** — `9223372036854775807 + 1` is min-i64, exit 0.
   Deliberate; see `docs/integer-semantics.md`.
3. **`sum()` and its `reduce` spelling diverge at the i64 edge**: `sum()` widens to
   float where the reduce wraps. Documented divergence, same doc.
4. **Float `==` is exact**: `[0.1, 0.2].sum() == 0.3` is `false`. Use `assert_close`.
5. **Falling off a JIT kernel is silent** — same answer, much slower (an eligibility
   diagnostic is planned). If a hot loop is slow, suspect the shape, not the math.

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

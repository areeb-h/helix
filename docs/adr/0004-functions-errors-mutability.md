# ADR 0004 — Functions, errors & mutability

- **Status:** Proposed
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude
- **Research:** [Domain 4](../research/2026-06-21-foundational-design.md#domain-4--functions-errors--mutability) (Hoare foundation high/3-0; syntax synthesis medium)

## Implementation status

A first error-handling form is implemented: `try EXPR` evaluates `EXPR` and catches
any runtime error, yielding a record `{ok, value, error}` (`{ok: true, value, error:
missing}` on success; `{ok: false, value: missing, error: <message>}` on failure).
This is expression-based and reuses records and `missing`, consistent with the
language's design, and it recovers from failures without aborting the program.
Programs that use `try` run on the tree-walker (the bytecode VM does not yet
implement exception handling). A `Result` + `?` propagation form, as discussed below,
remains future work.

## Context

Three intertwined decisions remain before Helix is a self-sufficient language:
how to *define functions* (brace-free, per the established syntax), how *errors
propagate*, and what the *value/mutability model* is. The load-bearing verified
result is Hoare's: failure/absence cases should be **tagged unions discriminated
at compile time** — the same argument that favors `Result` over exceptions.

## Prior approaches and their documented shortcomings

- **Exceptions** — invisible control flow; a reader cannot see which calls can
  fail (the same "must check every reference" problem Hoare flagged for null).
- **Go `if err != nil`** — explicit but verbose; clutters the primary path.
- **Rust `Result` + `?`** — errors as values, visible in types, propagated by a
  single lightweight operator. The model the research indicates.
- **CoffeeScript** brace-free/whitespace blocks — documented block-boundary
  ambiguity.
- **Rust's borrow checker** — genuine safety, but ownership/lifetimes impose the
  wrong cognitive load for scientists.

## Decision

### Function syntax — brace-free, expression-oriented

```text
# single-expression definition (the common case)
fn normalize(xs) = (xs - xs.mean()) / xs.std()

fn gc(seq) = seq.where(it == "G" or it == "C").len() / seq.length()

# composes with existing if-expressions and dot-chains
fn grade(score) = if score >= 90 then "A" else "B"
```

A function body **is an expression** (consistent with `if … then … else` and
comprehension chains).

**Local bindings — `let … in` (implemented), not indented blocks.** Intermediate
values use `let a = x, b = y in body` (sequential, scoped to `body`):
`fn variance(xs) = let m = xs.mean(), n = xs.count() in xs.map((it - m) ** 2).sum() / n`.
Python-style indentation for multi-statement bodies is deliberately **not**
adopted: Helix relies on **implicit newline-suppression to continue dot-chains
across lines** (`xs\n  .map(...)\n  .sum()`), and significant indentation would
collide with that construct — an indented `.map` line would be ambiguous between
"continue the chain" and "open a block". This is the concrete form of the
CoffeeScript ambiguity this ADR identifies. `let … in` is brace-free,
whitespace-insensitive, and conflict-free, at the cost of being ML-flavored
rather than Python-flavored — a tradeoff the dot-chain design justifies.

### Errors — values, not exceptions

A `Result`-style tagged union with a single lightweight propagation operator
(Rust `?`-style). What can fail is visible; the primary path stays clean.

```text
fn load(path) = io.read_csv(path)?      # propagates a read failure to the caller
```

Crucially, **`missing` (absent data, [ADR 0001](0001-missing-data.md)) and error
(failed operation) are distinct tagged concepts** — never collapsed into one
null. A column holds `missing`; a *computation* returns an error.

### Mutability — value semantics + copy-on-write

Immutable-by-default with explicit `mut` is **already shipped** and retained.
Extend it with copy-on-write / persistent immutable structures (Swift/R-style
value semantics):

```text
xs = [1, 2, 3]
ys = xs           # conceptually a copy; O(1) until one is mutated (COW)
mut zs = xs
zs = zs.map(it + 1)   # xs is untouched — no aliasing surprise
```

Memory safety and zero-copy come from COW + Arrow's immutable buffers
**underneath** — the borrow checker lives in Helix's *implementation*, never in
its surface language.

## Rationale

- Errors-as-values is Hoare's verified prescription and the only model consistent
  with "no surprises" and visible failure.
- Value semantics eliminate pandas' view/copy ambiguity *by construction* — the
  single largest source of unexpected behavior in the incumbent tools.
- Hiding the borrow checker is what allows Helix to provide Rust's safety without
  Rust's learning curve — a core objective of the project.

## Rejected alternatives

- **Exceptions as the primary mechanism** — invisible control flow.
- **Go-style explicit error returns** — verbose; conflicts with clean chains.
- **Exposing ownership/lifetimes to users** — wrong audience.
- **Mutable-by-default reference semantics (Python/pandas)** — aliasing/view-copy
  bugs.
- **Pure significant-whitespace blocks (CoffeeScript)** — boundary ambiguity.

## Consequences

- `fn name(params) = expr` is a small parser addition (a new `Stmt`/decl);
  closures and first-class functions follow naturally since bodies are
  expressions.
- The `?` operator and a `Result` value type extend Phase 1's value enum; the
  interpreter already threads Rust `Result<Value, HelixError>` internally, so the
  surface `Result` maps cleanly.
- COW requires the value representation to track sharing (Phase 1 already uses
  `Rc` for collections — the natural COW substrate via `Rc::make_mut`).

## Open questions

- Is the propagation operator `?` (proven and lightweight, but a symbol)
  acceptable under "avoid symbol-heavy syntax," or should it be a word (e.g.
  `try`)? Current preference is `?`.
- Do user functions allow multiple parameters with the same brace-free form only,
  or eventually a multi-line body — and what delimits it without braces?
- Can errors carry the same educational, caret-annotated quality as the
  interpreter's built-in errors, so that user-raised errors are also instructive?

# ADR 0035 — `where` clauses

- **Status:** Proposed (awaiting owner) — drafted 2026-08-24 from the consolidated
  0.4.0 field review, which ranks this second of everything it asks for.
- **Deciders:** project owner
- **Informed by:** 13 libraries / 124 modules / 20,081 lines of field code; ADR 0003
  (one obvious way); the withdrawn top-level-hoisting change (2026-08-20).

## Context

Two independent frictions in the field corpus point at the same missing form.

**1. Top-level value bindings do not hoist.** A `fn` may be called above its
definition; a top-level value may not be used above its binding. The error that
teaches this rule is good — and the field has now read it in five files
(`http/status`, `llm/errors`, `web/safe`, `web/tokens`, `web/css`), each time for
the same shape: a lookup table that belongs *beside* the one function that reads
it, forced to the top of the file instead. An error read five times for the same
mechanical edit is a design signal.

We tried the direct fix once — hoisting top-level values — and **withdrew it**:
the walker resolves globals at call time, but the VM binds slots at compile time
and `LoadGlobal` has no initialization check, so hoisting would read `Unit` where
the walker raises. Fixing that means a sentinel plus a checked load on one of the
hottest opcodes; it is a real change with a real cost, measured before shipping.

**2. The deep-`let` tail.** `let … in` has a median of one binding in the corpus,
but 8% of heads carry four or more, and the worst (`bio/tree::svg_cladogram`)
carries **nineteen** — a comma-separated preamble with its `in` far below. The
corpus also shows `do { … }` is under-discovered (47 uses against 472 `let`
heads), but `do` is not the answer here: it is for *sequential steps*, and these
are *named sub-expressions*.

## Decision (proposed)

Add `where` as a clause on **function definitions only**:

```
fn class_name(c) = LOOKUP.get(c) ?? "Invalid"
  where LOOKUP = { 1: "Informational", 2: "Success", 3: "Redirection",
                   4: "Client error", 5: "Server error" }
```

Semantics: exactly `fn class_name(c) = let LOOKUP = … in LOOKUP.get(c) ?? "Invalid"`.

- Bindings are evaluated per call, in order, each seeing the ones before it (the
  same sequential-visibility rule `let` already has). No mutual recursion between
  `where` bindings — that keeps it a pure desugar.
- Scope is the function body plus later `where` bindings. Nothing leaks.
- Multiple bindings separate the way `let` heads do.
- **Parser desugar to `let … in`** — zero engine changes, no three-engine drift
  surface, same implementation shape as UFCS and `sort_by`.

Restricting `where` to `fn` definitions (not arbitrary expressions) is the ADR
0003 discipline: on an arbitrary expression, `where` would be a strict second
spelling of `let … in`. On a `fn` definition it is the *only* spelling that puts
the scaffolding after the point — `let` cannot do it, `do` means something else —
and it simultaneously dissolves the top-level-table problem, because the table
stops being top-level at all.

## The ADR 0003 tension, stated honestly

`fn f(x) = e where B` and `fn f(x) = let B in e` compute the same thing, so this
IS a second spelling in the narrow sense. The claim that earns the exception:
the two orderings serve different READERS, not different writers. `let` reads
setup-first (right for short bindings); `where` reads conclusion-first (right for
a table or a long scaffold). The corpus evidence is that when the scaffold is
big, writers hoist it out of the function entirely — to the worst place, the top
of the file — because reading nineteen bindings before the point is worse than
breaking locality. If that trade stops being forced, both frictions disappear.

If the owner weighs ADR 0003 the other way, the fallback is the withdrawn
hoisting change done properly: checked `LoadGlobal` with a measured cost, cycle
detection (`A = B + 1; B = A + 1` refused at check time), and the walker/VM
divergence closed. That fixes friction 1 and leaves friction 2 standing.

## Consequences

- Five field files un-hoist their tables; the nineteen-binding head becomes
  readable without restructuring.
- `helix fmt` needs a layout rule (indent `where` under the definition; one
  binding per line past two).
- The checker sees the desugared `let`, so shadowing/ordering diagnostics come
  free; the one new diagnostic worth adding is "a `where` binding may not use the
  function's own name recursively through itself" — which the let-desugar
  already refuses naturally.
- Doc examples and the syntax guide gain one section; ADR 0003's table of
  declined spellings gains a row explaining why this one was taken (or this ADR
  records the decline).

# ADR 0053 — Positional destructuring: `[a, b] = xs`

- **Status:** **Accepted 2026-09-23; implemented.** `let [a, b] = xs in …` on all three
  engines, `[a, b] = xs` inside `do { }` and after `where`, the statement form with `mut`
  and `export`, a trailing `...rest`, and the checker typing a tuple's parts one by one.
- **Date:** 2026-09-23
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0046](0046-record-destructuring.md) (the record form this mirrors, and
  the desugar it reuses), [ADR 0024](0024-total-runtime.md) (a wrong length is an error, not
  a `missing`), [ADR 0044](0044-postgresql.md) (the batch that answers an Array — the case
  that asked).

## Context

`c.query([q1, q2])` answers an Array of frames in the order the statements were sent, and
the way to take them apart was `page = c.query([…])`, then `page[0]`, `page[1]`. A
positional answer is exactly what a positional pattern is for, and the language half had
one: `a, b = pair` has destructured a tuple or an array as a top-level STATEMENT since the
early days — but only there. Not in a `let`, not in a `do` block, not after `where`, and in
no bracketed spelling anywhere, while records had all four positions since ADR 0046. Asked
whether array destructuring should not be supported here: yes.

## Decision

**A positional pattern is written in brackets, `[a, b]`, and is a binding form wherever the
record pattern `{a, b}` is one.** The value is a tuple or an array; the pattern names every
part, or ends in `...rest` and takes the rest.

```helix
fn f(p) = let [a, b] = p in a * 10 + b
fn g(xs) = do {
  [head, ...tail] = xs
  …
}
fn norm(v) = sqrt(x * x + y * y) where [x, y] = v
[person, posts] = db.query([q1, q2])
mut [lo, hi] = bounds(xs)
```

- **The binder looks like the literal.** `{a, b}` reads a record the way `{a: 1, b: 2}`
  writes one; `[a, b]` reads a sequence the way `[1, 2]` writes one; `[a, ...rest]` takes the
  rest the way `[...xs, 1]` would spread it. It is recognised by LOOKING, as the record form
  is: a `[`, names and commas (a `...name`) up to `]`, then a single `=` — which no array
  expression can be followed by, `==` being its own token.
- **Desugared in the parser, to one node.** The value is bound once under a throwaway name
  (`$arr<N>`) — unless it is already a name, `[a, b] = p`, which is read directly (pure, one
  lookup a read) except when a pattern name is `p` itself, where the first read would replace
  what the second reads from — and each position is read through `Expr::Part { recv, index,
  names, rest }`:
  part `index` of a value that must be a tuple or an array of exactly `names` parts — or of
  at least `names` when the pattern has a rest, the read at `index == names` answering what
  is left. Both engines evaluate it through one function (`part_of`), so they cannot differ
  on the sentence. It is a new node rather than an `Index` so that no pass can treat it as a
  plain index by accident: an index would let `[a, b] = [1, 2, 3]` pass and would say
  "out of bounds" for `[a, b] = [1]`.
- **The wrong length is an error.** ADR 0046 made an absent FIELD `missing`, because a spec
  record's keys are optional by nature. A position is not optional by nature: a pattern of
  two names against three values is a shape mistake, and `xs[5]` on a short array refuses
  too. `...rest` is the spelling for "however many".
- **A tuple's rest is a tuple, an array's an array.** Positions are positions; `a, b = (1,
  "x")` always accepted a tuple, and the bracketed form accepts what the bare one did.
- **The bare statement is the same form.** `a, b = xs` desugars to the same assignments as
  `[a, b] = xs` (the temp, then one `Part` per name, queued through `pending` as the record
  statement form is). `Stmt::Destructure`, its VM op and its checker arm — a second mechanism
  for the same meaning — are retired. Inside a block the bare spelling is refused by name
  ("a pattern inside a block is written in brackets"): in a `let` or a `where` the comma
  separates bindings, and one spelling that works everywhere beats two that work in
  different places.
- **The checker types a tuple's parts one by one** — the precision `a, b = (1, "x")` always
  had: `a` an Int, `b` a String, and a tuple of the wrong length refused before anything
  runs, in the run-time sentence. An array's length is dynamic, so every part is the element
  type and the rest is the array's own. A value the checker can prove has no parts is refused
  there. And with it, `p[0]` on a known tuple is that element's type rather than the join of
  all of them, and `p[2]` on a pair is refused at check time.
- **No nesting, no parentheses.** `[a, [b, c]]` is not a pattern; an element that is itself a
  pair is destructured in turn. `(a, b) = t` is refused with a hint that names the brackets:
  parentheses make a tuple, and one spelling is the point.

## Consequences

- The batch example reads as it should: `[person, posts] = db.query([…])`.
- One desugar and one node for every position. The engines can drift only on `Part`, which
  has one arm in each and one function under both; `arr_destructure` in the corpus pins the
  outputs across the three engines and both DataFrame backends.
- The specializer knows the node: a `Part` of a sequence known from the call site is that
  element (or the literal of the rest), as `xs[i]` already was.
- It costs what it desugars to. On the VM, `let [a, b] = p in a + b` is 172 ns a call
  against 170 for `let a = p[0], b = p[1] in a + b` and 148 for `p[0] + p[1]`; on the walker
  281 against 281 and 183; `let [h, ...t] = xs` is a hair under `let h = xs[0], t =
  xs.drop(1)` on both (279 vs 290; 331 vs 348). The form's cost is its bindings', and a
  `Part` costs an `Index`.
- Sixty-three (program, engine) pairs of the bare statement — `mut`, `export`, a module's
  exported pair, an immutable target, a `fn` name as a target, a wrong length, a wrong type —
  print the same on the previous binary and this one, but for one added help line.

## Alternatives considered

- **Extend the bare `a, b = …` to `let`, `do` and `where`.** In `let` and `where` the comma
  already separates bindings (`let a = 1, b = 2 in`), so the bare form could never go there;
  extending it to `do` alone would leave the same inconsistency it has today.
- **`missing` for a short value, as the record form.** Rejected above: a position is not an
  optional key, and it would make `[a, b] = [1]` a silent `missing` where `xs[1]` refuses.
- **A per-read `Index` plus a length check on the temp.** Two nodes where one does, and the
  rest of a tuple would have needed a `drop` tuples do not have.
- **Nested patterns.** Not in this decision, as they are not in ADR 0046's.

# ADR 0066 — `for` loops, as notation for a fold

- **Status:** Accepted — phase 1 (`for`, compound assignment) is implemented: `src/parser.rs`
  (the scope stack, the fold, every refusal), `src/lexer.rs` and `src/token.rs` (`+=` `-=` `*=`
  `/=`), `src/types.rs` and `src/types/synth.rs` (the two sentences the checker says for a
  loop), `src/fmt.rs`, `src/syntaxdocs.rs`, `src/emit_rust.rs` (the refusal). Phases 2 and 3
  are designs, not commitments. What the implementation settled is under "As implemented".
- **Date:** 2026-11-26
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0004](0004-functions-errors-mutability.md) (immutable values, `mut` at the
  top level only — unchanged by this), [ADR 0053](0053-positional-destructuring.md) (`[a, b] = v`),
  [ADR 0061](0061-markup.md) (the precedent: notation the parser turns into what already
  exists), [ADR 0024](0024-total-runtime-no-host-panics.md).

## Context

Helix has no loop statement. Repetition is a method chain (`map`, `filter`, `reduce`) or a
tail-recursive function, and state that changes across a sequence is threaded through
`reduce`. That is the right model: values never change, a closure made in an iteration keeps
what it saw, and one program means one thing on three engines.

It is also the single largest thing a newcomer has to unlearn, and we have measured it:

- `scripts/stranger.py` generates the files people type before they know Helix. `for x in
  xs:` and its relatives are in every language's column, and the best the toolchain can do
  with them today is a hint that says to rewrite the code as `xs.map(f)`.
- `AGENTS.md` lists it first among the design-level traps: "Deciding this after writing an
  imperative loop means rewriting the program."
- The rewrite is not mechanical for a person. A loop that updates two totals becomes a
  `reduce` over a tuple, with the tuple packed and unpacked by hand:

  ```helix
  r = xs.reduce((0.0, 0), (st, x) => do {
    [total, n] = st
    (total + x, n + 1)
  })
  ```

The model does not need to change for the notation to. A `do` block already lets a name be
bound again on a later line (`n = 0` then `n = n + 1`, each line shadowing the last), so a
loop body that "updates" names is already expressible — as a fold whose state is those
names. Only the packing is missing.

## Decision

### Phase 1 — `for`, and compound assignment

```helix
fn mean(xs) = do {
  total = 0.0
  n = 0
  for x in xs {
    total += x
    n += 1
  }
  total / n
}
```

**`for PATTERN in EXPR { BODY }`** is a statement: it may be a line of a `do` block or of a
loop body, or a top-level statement. It is not an expression and has no value.

- `PATTERN` is a name, or a positional pattern in either spelling the language already has:
  `[a, b]` (as in a block) or `(a, b)` (as a lambda's parameter). `for (k, v) in d.items()`.
- `EXPR` is anything `reduce` accepts as a receiver: an array, a `range`, `d.items()`.
- `BODY` is lines, as in a `do` block, except that it needs no final result: each line is a
  binding (`name = expr`, a pattern binding), a compound assignment, an expression evaluated
  for its effect (`print(x)`), or a nested `for`.

**What the body may change.** A name the body assigns that was ALREADY BOUND where the loop
starts — an earlier line of the enclosing block, a parameter of the enclosing function or
lambda, a name an enclosing loop carries — is *carried*: each iteration starts with the
value the previous one left, and after the loop the name holds the last value. A name first
bound inside the body is local to one iteration and does not exist after the loop. At the
top level the carried names are the top-level names the body assigns; they must be `mut`,
and one that is not gets the existing error (`` `total` is immutable and cannot be
reassigned ``).

**It is a fold, and the parser writes it.** The loop above is read as

```helix
$s1 = xs.reduce((total, n), ($st, $it) => do {
  total = $st[0]
  n = $st[1]
  x = $it
  total = total + x
  n = n + 1
  (total, n)
})
total = $s1[0]
n = $s1[1]
```

One carried name is the state itself (no tuple); none makes the state `()` and the loop is
run for its effects. Nothing after the parser changes: the checker, the three engines, the
JIT, `helix effects` and `helix emit rust` see `reduce`, a lambda and a `do` block, which
they already agree about byte for byte. Consequently:

- Values stay immutable. `total += x` rebinds a name; nothing that held the old value sees
  a new one.
- A closure made in the body captures that iteration's values — the "loop variable capture"
  trap of other languages cannot be written.
- A function called from the body reads a `mut` global as it was before the loop: a
  top-level `for` is one statement, and globals change between statements, never within
  one. This is the rule ADR 0004 already gives every statement.

**`name += expr`** (and `-=`, `*=`, `/=`) is `name = name + expr`, wherever a binding to an
existing name is allowed: a `do` block, a loop body, and the top level for a `mut` name. It
is refused on a name that is not bound, with the plain binding as the hint.

**Keywords.** `for` and `in` at the start of a statement, followed by a pattern and `in`,
begin a loop; `for` stays an ordinary identifier everywhere else (it is an HTML attribute
name in markup, and existing programs may use it), so no working program changes meaning.
A line that starts `for (a, b)` and does not continue with `in` is still a call.

**What it does not do**, each with an error that names the alternative:

- No `break` or `continue`. Cut the sequence first: `for x in xs.take_while(p) { … }`,
  `for x in xs.filter(p) { … }`.
- No assignment inside `if`. `if` is an expression; a conditional update is
  `total = if x > 0 then total + x else total`.
- No loop value. `ys = for …` is refused with "a `for` loop is a statement; to build a
  collection use `xs.map(…)`".

### Diagnostics

- The foreign spellings keep their hints, now pointing at the loop: `for x in xs:` (the
  colon), `for (i = 0; i < n; i++)`, `for i in range(10):`, `for (i in 1:3)`, `foreach`.
- An error raised by the loop's own machinery must speak of the loop. `for i in 10 { … }`
  today would say "an Int has no method `reduce`"; the desugared call carries the loop's
  position and a marker so the checker and the runtime say "`for` needs something to iterate
  — an Array, a `range(…)`, `d.items()` — and this is an Int". No message may show a
  `$`-name (`stranger.py`'s NO-LEAK oracle already forbids it).
- Using a body-local name after the loop gets the unknown-name error with a hint: "it was
  first bound inside the `for` body, so it does not outlive an iteration; bind it before the
  loop to carry it out".

### Formatter, docs, tools

`helix fmt` lays a loop body out as it lays out a `do` block. `helix describe for` and
`helix search loop` answer with this notation and its fold. `docs/reference.md`,
`examples/language/loops.helix` (run on three engines by `vmparity.sh`) and the "Syntax that
trips up agents" list in `AGENTS.md` are updated; the entry there changes from "there are no
loops" to "a `for` loop is a fold: only names bound before it survive it".

### Phase 2 — `while` (design only)

`while COND { BODY }` cannot be a `reduce`: its length is not known. It is a tail-recursive
function, and the parser can write that too — a hidden top-level `fn` whose parameters are
the carried names plus the local names the condition and body read — which the VM already
runs as a loop and the JIT compiles. It needs the parser to know which names in the body are
local, which phase 1's scope tracking provides. It is left out of phase 1 so that phase 1
adds no new evaluation path at all; until it lands, `while` keeps its hint.

### Phase 3 — statement `if` in blocks (design only)

`if c { total += x }` — an `if` whose branches are bodies and whose carried names are the
state — is the same idea applied to a conditional. Considered separately, because it puts a
second meaning on `if`.

### As implemented (phase 1)

What writing it settled, where the text above left a choice or could not be followed:

- **The refusal of a non-sequence is written into the fold.** The engines' "has no method"
  sentence is built from the receiver, the method's name and a position, so a marker on the
  call cannot reach it without a change in each engine. The parser instead guards the fold —

  ```helix
  let $forin = EXPR in
    if type_of($forin) == "Array" then $forin.reduce(…) else raise("`for` needs something to iterate — …")
  ```

  — and the checker says the same sentence, with the same help line, when it knows the
  type. A source that is an array on its face (a literal, `range(…)`, `a..b`) is folded
  directly. A file that binds `type_of` or `raise` to something of its own gets no guard,
  and a non-sequence there is refused in `reduce`'s words.
- **The fold is written as a person would write it.** One carried name and a pattern that
  is a name are the lambda's own parameters, and a body whose last line assigns the carried
  name answers that value: `for x in xs { total += x }` is `xs.reduce(total, (total, x) =>
  total + x)` to the letter, which is also the shape the JIT compiles (the `$st`/`$it` form
  above measured 600 times slower there on a counting loop). Several carried names are read
  out of `$st` as shown above. The `reduce` carries an inert mark (its fallback-function
  slot names a function that cannot exist), which is how `helix emit rust` knows a loop.
- **`missing` is not a sequence.** `missing.reduce(…)` answers `missing`, which would have
  left one carried name `missing` and several an indexing error; the guard refuses it:
  "… and this is `missing`".
- **"Bound where the loop starts" includes a global read from a function.** A loop in a
  function that assigns an immutable global's name carries it, as a local — what a `do`
  line rebinding it makes — rather than treating it as new in every iteration, which would
  have been a silent wrong answer. A `mut` global assigned from a function's loop is the
  existing refusal ("would shadow it, not update it"); only a top-level loop updates one.
- **The loop's variable** read after the loop gets its own hint beside the body-local one.
- **A `where` binding is bound for the body** although it is written after it: the clause's
  names are read ahead.
- **Patterns:** `(x)` is `x`; a bracketed pattern takes a `...rest`, as `[a, ...rest] = v` does.
- **A body's lines end at the end of the line** (a `do` block does not check this).
- **A session (the REPL)** parses each line after the names the earlier lines bound.
- **A body-local name's hint** is carried from the parser to the checker by the position of
  the read, since the checker is what knows the name is unknown.

## Alternatives considered

- **Real mutable locals (`var`) and real loops in the engines.** Three engines and a JIT
  would each implement assignment and iteration and have to agree; closures would capture
  variables, not values; `helix effects` and the model checker would have to reason about
  local state. Everything the fold gives for free would become a proof obligation.
- **`for … with name = init { … }` as an expression that returns its state** (the first
  sketch of this ADR). Explicit, and it needs no scope tracking — but it is not what anyone
  types, the state has to be named twice, and it still has to be unpacked after the loop.
  The carried-names rule gets the same fold from the code people already write.
- **Leave it, and improve the hint.** The hint is already good. The rewrite it asks for is
  the cost.

## Consequences

- The most common first line a newcomer writes works, and means what they expect in every
  case that does not depend on mutation being observable from elsewhere.
- The parser gains a scope stack (names bound so far in each enclosing function, lambda,
  block and loop body). It is used only to decide which assigned names are carried.
- One new way to be surprised: a name first bound in a loop body is gone after it. The
  error says so.
- `helix emit rust` receives a `reduce` whose function is a `do` block with positional
  reads of a tuple. If the emitter does not yet translate that shape, a `for` in a rule is
  refused at emit time with a message that names the loop; rules keep working as written.
- `stranger.py`'s oracle-4 baseline moves (programs that were errors now run); it is
  re-blessed with the change, and the count must not rise.

## How it is verified

- A corpus program per rule above (`tests/corpus/for_*.helix`) with hand-written expected
  output, run on three engines.
- A differential test: for generated loop bodies, the loop and the hand-written `reduce` it
  stands for print the same thing on every engine.
- `helix fmt --check` over every new example; the formatter's "cannot change a program"
  property test extended to loops.
- Each refusal above has a test asserting its message and that no `$`-name appears in it.

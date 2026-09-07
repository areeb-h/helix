# ADR 0050 — A pure call with literal arguments is evaluated when the program loads

- **Status:** **Accepted & implemented 2026-09-07** (0.10.0-to-be). The decision is the user's,
  on the field build's 1.46a.
- **Decision:** a call to one of the program's own functions whose arguments are literals —
  or names bound to literals at the top level — is evaluated ONCE, when the program is
  loaded, in a sandboxed tree-walker, and replaced by its value's literal. So is a call
  through a function-valued field of a record the sandbox already holds — `User.sql({…})`,
  the object API a library exposes. A fold never changes what a program computes, only
  when; a raise the program would meet unconditionally at the top level is reported before
  anything runs, as a type error is.

## Context

`db/model.helix`'s `sql` is pure — `helix effects` reports no authority — and at almost every
real call site its argument is a literal: `User.sql({where: {city: "oslo"}, limit: 10})`.
Measured, the render cost exactly what it cost of a computed value, 4.2 µs against GORM's
1.4, and every spec key the library grew taxed every query that ignored it (two keys, 9.8 %),
because a `where` clause is eager and every binding in the render runs on every call. The
field's ask: fold such a call at compile time, so the render costs nothing at run time, and
so that every check the renderer performs — a column typo, an unknown operator, an
unqualified write, a `limit: "1; drop"` — becomes a `helix check` failure rather than a
run-time raise.

## The design

**What folds.** `f(args…)` where `f` is a top-level `fn` of the program, and `rec.m(args…)`
where `rec` is a top-level name the sandbox holds a Record for; the arguments may mention
literals, other names the sandbox holds, lambdas and pure expressions, and nothing else.
Builtin calls are not folded on their own: they are cheap already, and the producers among
them (`range`, `zeros`) would turn a lazy or large value into a literal.

**Purity is decided by running, not by analysis.** The sandbox is the tree-walker with a
flag. It refuses — and abandons the attempt, leaving the call as written — an impure builtin
(`print`, `emit`, `now`, `sleep`, every reader and writer), any authority whatever the process
was granted (the capability gate answers "no" inside a sandbox), a Python object, a name it
does not hold (a mutable global, a binding it could not itself evaluate, a parameter), a
write to a mutable global, a recursion deeper than 256, and more work than a budget of
2 000 units per attempt and 50 000 per load — a call, a tail hop, a comprehension element
and an array element handed to a method or a builtin each cost one. The budget is small on
purpose: the one cost a fold can add is the walker evaluating, at load, a numeric call the
JIT would have run faster, and at this size that is well under a millisecond, while a
render costs tens of units. The refusal is sticky, so
a `try` inside the evaluated code cannot swallow it. Determinism holds because Helix's
`random` family is hash-seeded and stateless. The static analysis `memoizable_fns` was the
alternative; it marks any function containing a method call impure, which is every render.

**What is written back.** Numbers, strings, booleans, `missing`, and arrays, tuples and
records of those, up to 4 096 nodes. A value with no literal — a record holding a lambda,
a frame, a tensor, a dict — is not written back, but the sandbox HOLDS it, which is how
`User = define({…})` stays a call while `User.sql({…})` below it folds to its string.

**On demand, in place.** A top-level binding is evaluated when an attempt meets its name —
never eagerly — so a program whose calls fold nothing pays for nothing but the walk; the
names such a binding reads in turn are bound the same way, and one the sandbox cannot
evaluate is forgotten, refusing whatever reads it. And a fold rewrites in place: it replaces
one node by its literal and never re-allocates another. The checker's type map names nodes
by address and the compiler reads receiver types from it after the fold, so a lambda body is
folded only while nothing else holds it, and the sandbox's own copies of the program's
functions own their bodies. (The first cut re-allocated a shared body through `make_mut`,
and a DataFrame `join` inside a lambda inside a function lost its receiver type on the VM.)

**Where a raise goes.** A genuine raise during a fold — the callee refusing its argument —
at a position that runs unconditionally at the top level (a top-level binding, a `print`
argument, an array element) is reported at load, where `check` reports a type error, before
any statement runs. Under `if`, `match`, `try`, the right of `and`/`or`/`??`, in a lambda, in
a method's argument, inside a function body: the call stays and raises at run time as before.
The sandbox's own refusals are never errors.

**Where it runs.** After the checker and the receiver-directed rewrite, in every pipeline
that runs, checks or bundles a program. The checker types what the programmer wrote: a fold
never adds precision a call lacked — `launder(true)`, typed `Any` by its annotation, stays
`Any` folded or not — and a type error outranks a raise, since the fold only runs on a
program the checker accepted. Every engine then runs the one program the fold produced.
The first cut ran in `module::load`, before the checker, and eight of the suite's tests
showed why not: a literal is more precise than the call it replaced, so a value laundered
through `Any` was suddenly refused, and a raise pre-empted the type error a reader should
see first.

## Consequences

- The field's render is zero at run time for a literal spec, and its refusals move to
  `helix check`.
- A program whose output preceded a run-time raise from such a call now shows the raise and
  nothing else — the same shape a type error has always had. `MIGRATIONS.md` records it.
- Loading costs what the folds cost, bounded by the budgets; a benchmark's `fib(30)` at the
  top level is abandoned after 2 000 calls, well under a millisecond, and runs natively as
  before. Measured on one binary with the A/B switch `HELIX_NOFOLD=1`: no per-process cost,
  no per-call cost on 200 literal calls, and the corpus's 93 programs check within noise.
- The walker gains one predictable branch on its call, builtin, comprehension and unknown-name
  paths; the other engines nothing.

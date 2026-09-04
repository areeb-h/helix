# Helix syntax & DX — research-grounded improvement proposals

Goal: make Helix **more readable than Python**, **easier for scientists** (often non-expert
programmers), **straightforward for the simple 90%** while keeping the **complex
capabilities** reachable. This doc proposes specific changes, each with *before → after*,
a justification tied to **evidence** (not taste), and an honest statement of the cost.

## The evidence we're designing against

- **Stefik & Siebert, *An Empirical Investigation into Programming Language Syntax* (ACM
  TOCE 2013).** Novices were tested across Ruby, Java, Perl, Python, Quorum, and *Randomo*
  (keywords chosen at random). The damning result: **C-style symbol syntax (Java/Perl) was
  no more accurate for novices than randomly-chosen keywords**, while word-oriented
  languages (Python, Ruby, Quorum) scored significantly higher. **Word > symbol** for
  learnability. <https://neverworkintheory.org/2014/01/29/stefik-siebert-syntax.html>
- **Green & Petre, Cognitive Dimensions of Notations.** A vocabulary for *why* a notation
  is hard: **hidden dependencies** (relationships you can't see), **role-expressiveness**
  (can you tell what each part *does*?), **closeness of mapping** (notation ≈ problem),
  **viscosity** (edits per logical change), **error-proneness**. Crucially: improving one
  dimension usually costs another — there is no free lunch, so every proposal below names
  its cost. <https://en.wikipedia.org/wiki/Cognitive_dimensions_of_notations>
- **R/dplyr non-standard evaluation.** Bare column names are "fast and fluid"
  interactively, but documented to cause **variable shadowing**, **lack of transparency**
  (you can't tell a function uses NSE), and "bang-your-head" pain when programming over
  columns. <https://dplyr.tidyverse.org/articles/programming.html>
- **Working memory (Miller, 7±2).** Positional arguments past ~3 overflow short-term
  memory — the reader must hold "which arg is which." This is the case for named arguments.

## What Helix already gets right (validate; do not churn)

Per the evidence above, several current choices are *good* and should be protected:

- **Word operators** `and` / `or` / `not`, and `if … then … else` — Stefik-favoured over
  `&&`/`||`/`?:`. Keep.
- **Implicit `it`** in comprehensions: `xs.map(it * 2)` already works — lower ceremony than
  Python's `[x*2 for x in xs]` or `map(lambda x: x*2, xs)`.
- **Method chaining** reads top-to-bottom (`df.where(...).select(...).sort(...)`) — high
  closeness-of-mapping to a query.
- **`missing` as a first-class value** (distinct from `NaN`) — removes a whole class of
  silent errors.
- **No semicolons, no braces-for-blocks clutter** — low visual noise.

---

## Proposal 1 — Column references: the big one ✅ *shipped (`@col` chosen)*

> **Decision:** the `@col` sigil below was adopted and implemented. `@age` is now the
> idiomatic way to name a column; bare names still parse (a deprecation window) so no
> existing code breaks. The analysis that led here is kept below.

**Before** Helix used bare column names (dplyr-style non-standard evaluation):
```helix
read_csv("big.csv").group(species).mean(expression)
patients.where(age > 40).select(name, age, diagnosis)
```
This reads beautifully — but it has exactly the two problems the research names:
- **Hidden dependency / shadowing:** is `age` a column or a local variable? You can't tell
  from the code, and if a variable `age` exists, which wins is a surprise (the dplyr hygiene
  problem).
- **Role-expressiveness / tooling:** an IDE can't tell `species` is a *column* (vs a typo,
  vs a variable) without fully modelling every column-verb — and a human reader can't
  either. (Your IDE worry is well-founded.)

Your suggestion — **quoted strings** — fixes safety/tooling but only *matches* pandas, and
adds noise:
```helix
read_csv("big.csv").group("species").mean("expression")   # safe, but "just pandas"
```

**Recommended: a lightweight column sigil — `@column`.** It keeps the dplyr readability
*and* makes the role explicit:
```helix
# After
read_csv("big.csv").group(@species).mean(@expression)
patients.where(@age > 40).select(@name, @age, @diagnosis)
patients.with({adult: @age >= 18, hr_per_decade: @resting_hr / (@age / 10)})
```

Why the sigil wins (cognitive-dimensions scorecard):

| option | closeness of mapping | hidden deps | role-expressive | IDE autocomplete | terse | "better than Python?" |
| --- | --- | --- | --- | --- | --- | --- |
| bare `species` (today) | high | **bad** (col vs var) | **low** | hard (infer context) | high | vs R: ties |
| string `"species"` | med | good | med | easy | low (noisy) | vs pandas: **ties** |
| **sigil `@species`** | high | **good** | **high** | **easy** (complete after `@`) | high | **beats both** |

`@` unambiguously means "a column of the frame in scope": a variable is `age`, a column is
`@age` — they can never be confused (kills the shadowing class of bugs), an LSP completes
column names the instant you type `@` (against the inferred schema), and a reader instantly
sees the role. It stays terse and chainable, so it's *more* readable than pandas strings and
*safer* than R's bare names — the genuine "best of both."

- **Cost (be honest):** one new concept to learn (`@` = column) and a parser change. The
  `@` adds minor visual texture. Alternatives if `@` feels wrong: `:species` (a *symbol*,
  precedent in Julia/Clojure/Ruby) or `$species` (precedent in R's `df$col`); `@` is
  recommended only because it can't collide with records (`{k: v}`) or interpolation.
- **Lower-effort alternative:** keep bare names and build a *column-verb-aware LSP* (it
  knows args of `where`/`group`/`select`/… are column positions, so autocomplete still
  works). This preserves today's syntax but leaves the shadowing/transparency problems
  unsolved. Viable, but the sigil is the better long-term DX call.

---

## Proposal 2 — Named arguments ✅ *shipped (v1: user functions)*

**Before** multi-arg calls were positional; the reader had to remember the order. **After**,
a user function may declare literal **default** values, and calls may pass arguments **by
name** (in any order after the positionals) and omit defaulted parameters:
```helix
fn greet(name, greeting = "Hello", excited = false) = ...
greet("Ada", excited: true)               # name positional; the rest defaulted/named
fn gap(length, open = -5, extend = -1) = open + length * extend
gap(3, open: -10)                          # override one parameter by name
```
- **Why:** role-expressiveness + working-memory (Miller) — past ~3 args, positional calls
  overflow short-term memory and invite silent wrong-order bugs. Python, R, and Swift all
  have this for the same reason; it's table-stakes DX.
- **How:** resolved entirely at parse time. Each call's named arguments are placed by name
  and omitted parameters filled with their defaults against the function's recorded
  signature, producing an ordinary positional call — so the type checker and both engines
  are unchanged, and there is zero run-time cost. Mixing rule: positional first, then named
  (Swift/Python). Defaults are restricted to literal constants (so they can be inserted at
  the call site).
- **v1 scope / follow-ups:** named arguments apply to **user-defined functions**. Builtins
  and methods (e.g. `read_csv(..., delimiter: ";")`, `seq.align(..., open: -10)`) still
  take positional arguments — supporting them needs per-builtin parameter-name metadata, a
  follow-up. Non-literal defaults (evaluated in function scope) are also deferred.
- **Shipped follow-ups (parse-time, same zero-cost model):**
  - **Through module qualification** — `dep.f(x, open: -10)` resolves named arguments and
    fills defaults exactly like a local call. (The loader's namespacing rewrite used to
    run first and hide the target from the resolver; fixed — see
    [ADR 0019](adr/0019-module-system.md).)
  - **Inside interpolation holes** — a call in a `"{ … }"` string
    (`"gap={gap(3, open: -10)}"`) resolves named args and defaults like any other call
    (the parser previously parsed interpolation-hole calls on a path that skipped
    resolution).

---

## Proposal 3 — Range literal `a..b` ✅ *shipped*

**Before:** `range(0, n)`. **After:** `0..n`
```helix
(0..1000000).map(it * it).sum()        # vs range(0, 1000000)
(0..n).filter(it % 2 == 0).count()
```
- **Why:** closeness of mapping (math interval notation) + terseness; `0..n` is instantly
  legible and matches Rust/Kotlin/Swift and Python-slice intuition. Keep `range(...)` as the
  explicit form for a computed step.
- **Status:** implemented as pure front-end sugar — `a..b` desugars to `range(a, b)` at parse
  time, so all three engines *and* the JIT range-fusion path handle it unchanged. Exclusive of
  the upper bound (matches `range`); binds looser than `+`/`-` (so `0..n+1` is `0..(n+1)`) and
  does not chain. (`0..=n` inclusive form left for later if a need appears.)

---

## Proposal 4 — Promote implicit `it` (already works — just lean into it)

`xs.map(it * 2)` and `xs.filter(it > 0)` already parse. This is a genuine readability edge
over Python and should be the *documented default* for one-binder bodies; reserve the
explicit `x => …` lambda for multi-arg or when naming aids clarity.
```helix
nums.filter(it % 2 == 0).map(it * it)      # vs Python: [x*x for x in nums if x%2==0]
```
- **Why:** removes lambda ceremony for the 90% case (role-expressiveness, terseness).
- **Cost:** none (exists) — this is a docs/teaching change, not a language change.

---

## Proposal 5 — A pipe operator `|>` ✅ *resolved differently — UFCS (v0.3.0, completed v0.9.0)*

> **Decision:** no `|>` was added. UFCS shipped instead: a user-defined function is
> callable in method position — `x.f(a)` means `f(x, a)` — so a plain-function pipeline
> chains with the *one* existing method syntax:
> `"data.csv".load().clean().normalize().summarize()`.
>
> **v0.9.0 finished it (ADR 0045): the RECEIVER decides, at run time.** The v0.3.0 rule
> was "when no type owns the name", tested at parse time — a global check made where the
> receiver does not exist. Every good verb name is some type's method, so `where`,
> `select`, `first`, `count`, `all`, `join`, `sort`, `take`, `get`, `sum`, `min`, `max`
> and `unique` were unusable by a user's own library, and a query builder could not be
> written in the language at all. Now a call that fails dispatch retries as the free
> call, and the families the compiler routes by TYPE — the DataFrame column verbs, the
> comprehensions, and `join` — emit both readings behind a receiver test. A type that
> OWNS the name always keeps it, which is what stops a real method's real error from
> being re-run as something else; `PyObject` and `Node` never fall back, which is what
> keeps `np.round(1.5)` a Python call. (See `ufcs_fallback_applies` and
> `RecvClass::holds` in `src/interp/methods/mod.rs` and `src/bytecode/ops.rs`.)
>
> Still resolved at parse time, and so still outside the rule: the parser's own desugars
> (`sort_by`, `min_by`, `take_while`, `zipmap`, `position`, …), which are rewritten
> before any receiver exists. Recorded in `docs/dx-plan.md`.
>
> The original analysis is kept below.

Method chains read top-to-bottom, but *plain functions* nest inside-out:
```helix
# Before
summarize(normalize(clean(load("data.csv"))))
```
**After (with `|>`):**
```helix
"data.csv" |> load |> clean |> normalize |> summarize
```
- **Why:** data-pipeline closeness-of-mapping; beloved in F#/Elixir/R (`%>%`) for exactly
  the science "load → transform → analyze" shape.
- **Cost (honest):** a *second* way to chain (methods already do it) — risks violating "one
  obvious way." Only worth it if plain-function pipelines turn out common; otherwise method
  chaining suffices. **Lower priority than 1–4.**

---

## Proposal 6 — Multi-step expressions: `do { … }` block ✅ *shipped*

A multi-step function body used to nest `let … in` (a `{ … }` alone parses as a
*record*, so it was unavailable):
```helix
fn risk(a, c) = let bmi = weight / (height * height) in let s = bmi * a in s + c
```
That nests awkwardly. **A `do { … }` block shipped** — a sequence of bindings and a
final result expression, desugared at parse time into the `let … in` chain (so both
engines and the type checker are unchanged, zero run-time cost):
```helix
fn risk(a, c) = do {
  bmi = weight / (height * height)
  s = bmi * a
  s + c
}
```
The last expression is the value; earlier lines are `name = expr` bindings. This is
now the idiomatic form for a non-trivial body (and for a lambda body that needs steps,
as in the event-loop server's `filter(c => do { … })`). The `let … in` form still works.

---

## Proposal 7 — Record update / spread `{ ...base, field: value }` ✅ *shipped*

Building a modified copy of a record used to mean re-listing every field, which drops
any field you forget and is pure viscosity (many edits per logical change). **Spread
shipped** — `{ ...base, field: newValue }` copies `base` and overrides/adds the listed
fields, producing a *new* record (Helix values are immutable):
```helix
updated = { ...patient, age: patient.age + 1 }          # one field changed, rest copied
req2    = { ...req, headers: merged, query: parsed }     # add/replace several
```
- **Why:** closeness-of-mapping ("same as before, but …") and low viscosity — the exact
  shape TypeScript/JS spread and Elm/OCaml record-update serve. It was also a real
  correctness lever: a web-lib request object rebuilt field-by-field silently *dropped*
  headers/query; `{ ...req, … }` fixed that class of bug by construction.
- **Semantics:** later keys win; spread is evaluated left-to-right; the result is a
  fresh immutable record. Type-checked structurally like any record literal.
- **Dicts spread too (v0.3.0):** the spread base may be a dict as well as a record —
  its string keys become fields, under the same later-keys-win rule.

---

## Proposal 8 — Function values are callable `(rec.handler)(x)` ✅ *shipped*

A function stored in a record/array field, or a `=>` lambda in a variable, is a
first-class value and is **called with ordinary call syntax**. Since 0.9.1 the field form
needs no parentheses: `rec.f(args)` calls the function held in field `f`.
```helix
route = { path: "/x", handler: req => { status: 200, text: req.path } }
resp  = route.handler(req)            # calls the function in the field
resp  = (route.handler)(req)          # the same call, parenthesised — still valid
```
**Precedence** is what makes this safe, and it is fixed: a *real* Record method
(`get`/`expect`/`has`/`keys`/`values`/`items`) wins over a same-named field, so
`{keys: f}.keys()` is still the key list; then a function-valued field; then a free `fn`
of that name (UFCS). A field holding a non-function is still refused, with the hint to read
it without parentheses. Until 0.9.1 both halves refused `rec.f(args)` outright, with a hint
that said, in its own words, "the object-API spelling `r.go(3)` is what everyone writes
first" — the rule that blocked `User.find(1)` in a library without importing every verb.
This is what makes record-of-handlers dispatch (a router, a plugin table, a strategy
map) expressible in-model. Full rationale and the method-vs-value disambiguation are in
[ADR 0005](adr/0005-syntax-conventions.md); the VM path (`Op::CallValue`,
`Value::VmFunc`, no captured environment) is in [execution-engine.md](execution-engine.md).

---

## `where` clauses (ADR 0035, shipped 2026-08-24)

A function definition may put its scaffolding AFTER the point:

```
fn class_name(c) = LOOKUP.get(c) ?? "Invalid"
  where LOOKUP = [[2, "Success"], [4, "Client error"]].to_dict()
```

- Exactly `fn class_name(c) = let LOOKUP = … in LOOKUP.get(c) ?? "Invalid"` — a
  parser desugar, so the engines cannot drift.
- Multiple bindings separate with commas; later bindings see earlier ones, and
  every binding sees the parameters.
- `fn` definitions only (an arbitrary expression already has `let … in`; the
  ADR states the one-obvious-way argument).
- `where` is NOT a keyword: frames keep `.where(...)`, and a binding named
  `where` still works — the clause is recognized by its exact
  `where NAME = …` shape after a fn body.

## The Result shape (stated 2026-08-24, from the consolidated field review)

`try EXPR` produces THE canonical result record, and libraries should pass it
through rather than invent near-misses of it:

```
try(1 + 1)   =>  {ok: true,  value: 2,       error: missing}
try(1 / 0)   =>  {ok: false, value: missing, error: "division by zero"}
```

Two rules matter and are easy to get wrong from another language's prior:

- **Success carries `error: missing`, not `error: ""`.** A branch on `r.error`
  must test against `missing` (or use `r.ok`); hand-built records that write
  `""` on success will disagree with `try`'s at every library seam.
- `try` binds tighter than a postfix chain: `try(f()).ok` reads `.ok` on `f()`'s
  result. Bind first — `let r = try(f()) in r.ok` (the checker now says exactly
  this).

If a user-level Result type with constructors is ever wanted, that is an ADR,
not a convention — until then, `try`'s record is the shape.

## Anti-proposals — what NOT to do (the evidence forbids it)

- **Don't add C-style symbols** (`&&`, `||`, `?:`, `++`, `x++`) — Stefik shows they perform
  no better than random keywords for novices. Keep the words.
- **Don't go significant-whitespace-only** for blocks (Python's indentation-as-syntax) — it
  raises error-proneness for copy-paste-heavy scientific scripting; explicit beats implicit
  here.
- **Don't overload `.`** for both methods and column access — keep column refs visually
  distinct (Proposal 1) rather than `df.age`.

## Recommended sequencing

1. **Named arguments** (Proposal 2) ✅ *shipped* — highest DX/effort ratio, table-stakes, no
   churn to existing code (v1: user functions + literal defaults).
2. **Range literal `a..b`** (Proposal 3) ✅ *shipped* — tiny, high-legibility win.
3. **Column sigil `@col`** (Proposal 1) ✅ *shipped* — the strategic call, made before an
   ecosystem of code locked in bare names (migration cost compounds — see R's painful NSE
   retrofits).
4. **Document implicit `it`** (Proposal 4) — free.
5. Blocks (6) ✅ *shipped* as `do { … }`; pipe (5) resolved by UFCS in v0.3.0 instead of a
   new operator.

The meta-principle throughout: **progressive disclosure** — the simple case is one obvious,
low-symbol line; the complex case (explicit lambdas, types, `match`, units) stays reachable
but never taxes the simple one. Every change above is a trade-off (cognitive dimensions
guarantees it); the costs are stated so the decision is eyes-open.

## Sources

Stefik & Siebert, *An Empirical Investigation into Programming Language Syntax*, ACM TOCE
2013 · Green & Petre, *Cognitive Dimensions of Notations* (HCI'89; Usability Analysis of
VPLs, 1996) · dplyr "Programming with dplyr" (tidy-eval pitfalls) · Miller, *The Magical
Number Seven, Plus or Minus Two* (1956).

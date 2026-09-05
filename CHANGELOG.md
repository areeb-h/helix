# Changelog

## Unreleased

### Added

- **A default may be a literal container of literals** — `= {}`, `= []`, `= (1, 2)`,
  `= {retries: 3}` — on a `fn` and on a lambda. An options record could not have an empty
  default before, and there was no reading under which `{}` is not a constant. Anything
  not a literal inside is still refused, with the same sentence.

- **Lambda defaults, and function values that keep them.** `(x, n = 10) => x + n` declares
  a trailing default exactly as a `fn` does — a literal constant, defaults last, parsed by
  the same parameter parser — and a call short of the parameters by at most that many is
  padded at run time on every engine. The same padding gives a function VALUE its
  declaration's defaults: `h = g; h(2)` with `fn g(a, b = 5)` used to be refused for
  arity, because only a call BY NAME was filled by the parser. The checker knows the range,
  so `f()` is refused at check time in the runtime's words: `` `f` takes 1 to 2 arguments,
  got 0 ``. Zero cost on the equal-count path — measured, not assumed: 3M tail calls and
  2M calls under `HELIX_NOJIT=1` at 1.000× against a clean build of the previous commit,
  after a first shape that routed the equal case through a `Result`-returning helper had
  cost the tail loop 5–9%. The equal case is the one comparison it always was; only a
  short or long call enters a cold helper. Pinned by
  `a_lambda_declares_defaults_and_a_function_value_keeps_them` and the corpus program
  `lambda_defaults`.

- **PostgreSQL writes: `postgres_execute(url, sql, params?)`, and `postgres_open(url,
  "write")` with `execute(sql, params?)` on the connection** (ADR 0047). The answer is
  always `{affected, rows}`: the count from the server's completion tag, and a frame of
  what the statement returned — empty unless it has a `RETURNING`, which is how an inserted
  id comes back in the same round trip. Parameters bind as values, exactly as in
  `postgres_query`. A write is a different SESSION — the startup packet omits the read-only
  default only for one opened to write — and spends a grant of its own, `db-write`
  (`HELIX_ALLOW_DB=write`, or `db = "write"` in `[capabilities]`), alongside `net`: granting
  the network still keeps a program read-only against every database it can reach. `execute`
  on a read-only connection is refused before a byte is sent, with the spelling that opens a
  writable one. One statement is one transaction; a transaction spanning statements is the
  open item.

  Verified here against a fake wire server that checks what the client sends — the
  read-only parameter present for a query, absent for a write — and answers rows and a tag;
  the gate now builds `--features postgres`, so those tests run in every gate (they were the
  one feature whose tests ran nowhere). Live verification is the field build's.

  ```helix
  db = postgres_open(url, "write")
  db.execute("insert into people (name) values ($1) returning id", ["Ada"]).rows
  postgres_execute(url, "delete from people where age < $1", [18]).affected
  ```

- **Record destructuring: `let {where, limit} = spec in …`** — and `{where, limit} = spec`
  inside a `do { }` block or as a top-level statement, where `mut` and `export` apply per
  field (ADR 0046). One binding per named field; an absent field is
  `missing`, the answer `get` gives, because a spec record's fields are optional by nature.
  The reads are field reads — one symbol scan per name — not `get` dispatches: a renderer
  that read six keys through `Record.get` per call (38% of the call, a field report
  measured) reads them in one line and one scan each. The checker refuses a name a KNOWN
  record cannot have (`record has no field `limt`` — `did you mean `limit`?`), both layers
  refuse a receiver without fields in one sentence, and `{a: x}` is refused with the
  spelling that does what was meant. Pinned on both DataFrame backends by the corpus
  program `rec_destructure`.

  Measured on one binary, interleaved min-of-7, 2M calls, six lookups of which three are
  present: **392 → 358 ns per call (1.10×)**. Honest reading: on a three-field record the
  `get` dispatch itself is ~40 ns, so the 38% a field profile attributed to six of them
  must come from a different shape (larger records, or dict-backed specs) that this
  measurement does not reproduce. The line is the point; the speed is a bonus.

  ```helix
  fn render(spec) = let {where, limit, order} = spec in …
  render({where: "id = $1"})     # limit and order are `missing`
  ```

- **`rec.f(args)` calls a function held in field `f`.** Both halves refused it — with a
  hint that said, in its own words, "the object-API spelling `r.go(3)` is what everyone
  writes first". The language knew what people wanted and declined it; it was the single
  rule blocking `User.find(1)` in a library without importing every verb.

  ```helix
  w = {f: (x) => x + 1}
  w.f(1)        # 2 — used to be: `f` is a field of this record, not a method
  (w.f)(1)      # 2 — still valid, the same call
  ```

  **Precedence is the whole decision, and it was already settled by construction.** The
  five real Record methods are matched *before* the field fallback in both halves, so
  `{get: f}.get(k)` keeps its meaning. The order is: a real method, then a function-valued
  field, then a free `fn` of that name (UFCS) — a field is more specific than a free name,
  so it wins that tie. Each boundary is asserted from both sides, because the failure on
  either side is silent.

  Engines: the walker calls the `FuncVal` through `call_function`; the VM extracts the chunk
  index and upvalues from a `VmFunc` or a `Closure` and pushes a frame — `Op::CallValue`
  step for step, minus the function value on the stack. A capturing lambda is the second
  VM path and is tested as its own case. Arity is checked at run time exactly as for
  `(rec.f)(args)`: the checker's record arm receives no argument types, and checking there
  for one spelling would make the two spellings disagree.

- **`Dict.get(k, default?)`**, the shape `Record.get` has always had. Two sibling types
  answering the same question, and only one of them could say "or this instead" — so
  `?? default` was a workaround for something the other simply had. Reported from the field
  alongside an ORM build.

  **Absence is not a missing value**, on both types: a key that is PRESENT with a `missing`
  value answers `missing`, not the default. Getting that wrong would launder a real
  `missing` in the data into a caller-chosen number, which is the ADR 0001 distinction
  `expect` exists to make loud. Both edges are asserted on both types, so they cannot drift
  apart.

### Changed

- **An arity refusal reads `takes` at every layer, for every kind of function.** User
  functions said `expects` (checker, VM, walker) while builtins said `takes` — except the
  builtins routed through the shared helper, which said `expects` too. One helper now:
  `` `f` takes 2 arguments, got 1 ``, or `` takes 1 to 2 arguments `` with defaults. Licensed
  in `tests/compat/MIGRATIONS.md` (message-only).

- **Range arity reads the same from both halves.** Writing the test for `Dict.get` turned
  up the same drift one family over:

  ```
  r.get("a", 1, 2)   checker:  `get` takes 1 to 2 arguments, got 3
  d.get("a", 1, 2)   runtime:  `get` expects 1 or 2 arguments, got 3
  ```

  The checker has a systematic arity formatter — "takes no arguments" / "takes exactly N"
  / "takes at least N" / "takes A to B" — and the runtime spelled range arity by hand at
  three sites in different words. The systematic form wins; `range` already said "takes 1
  to 3 arguments" from both halves, so it is what the tree mostly spoke.

  Recorded and not fixed here: the arity family has a second split, between a USER
  function ("expects N arguments") and a builtin ("takes N arguments"). Both are internally
  consistent, so no program sees two answers for one refusal — which is the property under
  test. Making them one word is a separate change with its own pins.

- **UFCS is decided by the receiver at every layer.** The parser still rewrote `x.f(a)`
  into `f(x, a)` at parse time — by NAME — whenever `f` was a declared fn no type owned.
  That was the last parse-time decision ADR 0045 left standing, and it is why a record's
  own function-valued field could never win against a same-named free fn: the rewrite
  fired before either engine saw a method. It is gone.

  What replaced it, because removing it alone was not enough: with nothing in its place,
  `range(0, n).map(it.f(1))` measured **25 → 108 ns per element** — the JIT's kernel
  analysis admits a `Call` and not a `Method`, so the comprehension stopped fusing. So the
  decision now lives at the layer that knows the receiver:

  - a pass after the checker (`src/ufcs.rs`) makes the call a call where the receiver's
    type is PROVEN and rules the method reading out — `jit-explain` reports "1 kernel
    site offered, 1 compiled" for both spellings again;
  - both engines decide the rest at run time, same order: real method, then a
    function-valued field, then the declared fn with the receiver first. The VM peeks at
    the receiver before dispatch (no failed dispatch, no error object), scans a per-site
    list of the types that own the name, compares field names as interned symbols, and
    enters through `CallFn`'s own entry — memo, JIT specializations and all.

  Measured on one binary, interleaved min-of-15, method spelling against direct:

  | receiver | ratio (min / median) |
  |---|---|
  | Int, in a range-map | 0.976× / 1.042× |
  | Record | 1.021× / 1.019× |

  `x.f(y)` costs what `f(x, y)` costs. A shadowed name is still not the fn, a type that
  owns the name still keeps its method, and `registry::is_any_method` — the rewrite's only
  caller — is gone with it.

- **One refusal, one sentence.** The checker said `type Int has no method \`nope\`` where
  the runtime said `an Int has no method \`nope\`` — the same rejection reached by two
  routes a caller cannot choose between. A receiver whose type is known refuses in the
  checker; the same receiver through a parameter is `Unknown`, so the checker steps aside
  and the runtime answers. The checker now speaks the runtime's sentence.

  The runtime's form won on evidence rather than taste: the sibling family — `` `f` is an
  Int, not a function `` — already ran `with_article` on both sides, so an article was
  already the house style and `type Int` was the outlier.

  **`docs/dx-plan.md` estimated ~40 pins across 14 files; it was four.** Most of what
  `grep` matched was the sentence that did not move. Unifying onto the majority form is
  what made a change that had been deferred for months cheap.

  **What was actually hard was finding the producers.** Enumerating — forcing the same
  refusal through a literal receiver and a parameter receiver, then diffing — found three
  things grep did not:

  - `types.rs::unknown_method` was one of **three** checker producers. Fixing it left
    `Array` and `String` agreeing while `Int` still diverged through
    `types/synth.rs`'s scalar fallback, and `Record` through a hardcoded string.
  - `interp/comprehensions.rs::not_an_array` is a **runtime** path that spoke the
    **checker's** sentence, so unifying the checker alone would have *inverted* the drift
    on `x.map(it)` rather than closing it.
  - Three more sites built the right words by hand (`a Tensor`, `a DataFrame`,
    `a Connection`) — correct only because someone typed the right article for a
    consonant.

  The enumerator is kept as the guard
  (`a_refusal_reads_the_same_from_the_checker_and_the_runtime`), covering seven receiver
  types across four refusal families. Sabotaged three ways; each producer is caught by a
  different case, which is the property that matters — a guard catching one of three is
  how this drift survived in the first place.

  The article was, accidentally, how you could tell which half refused, and this cycle used
  it twice to diagnose. That is not a reason to keep two sentences: `helix check` answers
  the same question outright, and a diagnostic that works by reading grammar is not one
  anyone can rely on.

### Fixed

- **A function-valued field answers before the comprehension and frame-verb families, on
  every engine.** ADR 0045's order — method, field, free fn — held for `count`, `find`,
  `save`, and not for `all`, `map`, `where`, `filter`, `any`, `reduce`, `select`, `group`,
  `with`, `join`: the compiled families only reached the dynamic method op when a free fn
  of the name was declared, and the walker's comprehension shortcut fired first. So
  `User.all(db)` was refused on all three engines with "a Record has no method `all`", and
  `q.select(1)` was refused by the VM and answered by the walker — a three-engine divergence
  a field build caught. Every family now takes the receiver split whether or not a fn is
  declared, and the walker consults the field before its shortcut. Pinned by the corpus
  program `rec_field_precedence` under both DataFrame backends.
- **`xs.map(mk(0, it).sql)` stays a projection.** The implicit-`it` rewrite that reads a
  bound path (`xs.map(util.double)`) as `util.double(it)` also fired on a receiver that is
  a call, so `.sql` became `.sql(it)` and was refused as "a field, not a method" — on every
  engine and in the checker. Only a path of names is a bound function now.
- **`where {a, b} = spec` destructures** in a `where` clause (ADR 0035 meets ADR 0046), and
  a malformed clause is named as one — with the destructuring shape in its help — rather
  than falling into "expected end of line after statement".
- **`try` carries the help.** `{ok, value, error, help}`: `help` is the error's hint, where
  every actionable spelling lives, or `missing`. No program could read it (field build), so
  no test could assert the guidance existed. The record is one field wider; a program that
  printed a whole `try` record shows it.
- **A capability refusal names the grant that is missing.** A write needs `db-write` and
  `net`; with `HELIX_ALLOW_DB=write` set and the network not granted, the refusal said
  `db-write` — the grant the program had. It says `net` now, with `net`'s help.
- **A `Floats` array's `map` reaches native code for every body the Int-source kernels
  take.** `xs.map(to_int(it))`, `sign`, `floor`/`ceil`/`round`/`trunc`, `it / 2.0`,
  `it * (1.0 / 2.0)`, `min(it, 2.0)`, a `let`, a user `Float` function — over a `Floats`
  array — ran at interpreter speed while `jit-explain` said "compiled" (field build,
  1.31/1.32): the Int-source specialization existed, the Float-source one did not, and a
  `Floats` receiver had only the monomorphic `+ - *` kernel. The mixed family's per-node
  typing now builds with the element a Float — four passes, Int-proven or value-scalar
  captures × a Float or an Int root, the poison out-param carrying `/` and the rounders
  exactly as it does for an Int source. Measured on 2M floats, JIT on against off: `to_int`
  2.1× → 26×, `sign` 2.0× → 25×, `floor` 2.0× → 25×, `it / 2.0` 2.4× → 22×,
  `it * (1.0 / 2.0)` 1.9× → 25× (`abs`, the control, 22–26× throughout). Every shape agrees
  with the walker bit-for-bit and a raising body poisons to the walker's exact error, pinned
  by `float_source_typed_map_kernels_agree_and_engage`, which also asserts the native
  counter moved. Pinning `floor(it / s)` with a Float `s` found a second gap on the way: the
  analysis behind every value-scalar and indexed build had no arm for the four rounders
  while its unindexed twin did, so no such build existed for any rounding body on ANY
  source — `range(…).map(floor(it / s))` declined to the bytecode loop too. It has the arm,
  and the matrix its last cell: an Int source with a runtime Float capture and an Int root
  builds ("mapmiv"), so that range map runs native as well. And a third marshal closes the
  cliff under the commonest map there is: `range(…).map(it * s)` with a Float `s` ran at
  1.0× against 16× for the literal `it * 0.5`, because the Int-proven build declines a Float
  capture at dispatch and the value-scalar analysis must refuse `Int * capture` for a capture
  that might be an Int. Every capture a runtime Float is a proof the dispatch can make, and
  under it a capture promotes exactly where the walker promotes it — so the FLOAT-PROVEN
  builds (both sources, both roots) take `it * s`, `it + s`, `to_int(it * s)`: measured
  1.0× → 16× on 3M elements. An Int capture at the same site still takes the i64 build and
  wraps as the walker wraps.
- **`std(ddof?)` and `var(ddof?)`.** `[1, 2, 3, 4].std(1)` is the sample standard deviation
  (divide by n−1), `std()` the population one it always was; the same for `var`. The walker
  already parsed the argument — only the documented signature `std()` stood in the way, and the
  checker refused `std(1)` as "takes no arguments" (field build, 1.36). `helix doc Array.std`
  and the reference say which one you get.
- **A negative `take`/`drop` count is an error, on every type.** Array clamped it to 0 —
  `[1, 2, 3].take(-1)` was `[]` and `drop(-1)` the whole array, silently — while String raised
  "`take` needs a non-negative count" (field build, 1.33); Bytes clamped too. One sentence
  everywhere now. Zero and past-the-end still clamp, as documented.
- **A quoted key in a record brace.** `{city: "oslo", "age >=": 18}` is a record whose second
  field is spelled with an operator — the query-builder shape the field build's ORM lands on
  (1.27.4) — where it used to be refused as a mixing of the two brace forms, so adding one
  operator re-spelled every key of the clause and turned the record into a Dict (a different
  type, iterated in a different order). The brace stays a record: written order kept, the field
  reachable as `rec.get("age >=")`, printed back quoted when its spelling is not an identifier
  (`{"age >=": 18, city: "oslo"}` re-parses). A bare key in a Dict brace is still refused.
- **The transcendentals and `**` reach native code.** `exp`, `ln`, `log2`, `log10`, `sin`,
  `cos`, `tan`, `asin`, `acos`, `atan`, `sinh`, `cosh`, `tanh`, `cbrt`, `degrees`, `radians`,
  `erf`, `normal_cdf`, `normal_pdf`, `relu`, `sigmoid` and `a ** b` in a `map`, a `reduce`
  or a Float function compiled whole. The coverage doc called them a "permanent exclusion" —
  a kernel's result had to match the host libm bit for bit, and Cranelift has no instruction
  for them. They match by construction: each kernel call reaches an `extern "C"` shim that IS
  the Rust function the walker applies, one function compiled once and executed by both
  engines on the same bits; `**` is the walker's own `powi`/`powf` rule. `Int ** Int` declines
  (an Int unless it overflows, when the walker answers a Float — a kind no typed kernel can
  promise per element), and a NaN result is a value in both engines, never a raise. The
  array-source scalar float reduce (`xs.reduce(0.0, (acc, x) => …)`) lowers through the typed
  pair the tuple form always used, so it takes these too — and its old untyped gate admitted an
  Int-ROOTED body (`(acc, x) => 2`) for a kernel that wrote `2.0` into an accumulator the walker
  turns into an `Int`; the typed gate declines it, and the engines agree. Pinned by
  `transcendentals_and_pow_in_kernels_agree_and_engage` on three engines, every name.
- **A conditional in a map body reaches native code.** `xs.map(if it > 5.0 then it else 0.0)`
  — relu, the commonest activation — was never offered to the JIT (field build, 1.31; the
  coverage doc's listed cliff): the i64 analysis rejects a Float literal, and neither the
  monomorphic `f64` kernel nor the typed analyses had an `if`. Every typed body takes one
  now — `and`/`or` over the six comparisons, both branches of one kind (an `if` whose branches
  differ yields an Int or a Float per element, which no packed buffer can hold, so it declines
  and the walker answers), a NaN meeting an ordering comparison poisoning to the walker's
  "cannot compare", `==` on a NaN staying IEEE. Pinned by
  `a_conditional_in_a_typed_map_body_agrees_and_engages` on three engines.
- **`jit-explain` names each `map` site's specializations, and says when a source kind has
  none.** "compiled" counted any specialization, so a site with an Int-source build and no
  Float-source one read the same as one with both. A row now reads `compiled (i64) — a
  Float source runs the bytecode loop`; the JSON carries `specializations`,
  `serves_int_source` and `serves_float_source`.
- **A bare bound function is the argument of EVERY verb that takes one.** `xs.filter(pos)`
  and `xs.where(pos)` were refused ("`filter` expects a yes/no test, but the expression
  produces a value of type Function") while `xs.all(pos)` was accepted — a field build's
  finding — and the verbs the parser desugars never saw the rule at all: `xs.position(f)`
  answered `missing`, `take_while(f)` everything, `flat_map(f)` and `zipmap(ys, f)` arrays
  of function values, `min_by(f)` a comparison error, exit 0 (a roadmap item had recorded
  the class). One rule now, for `map`/`any`/`all`, `filter`/`where`/`count_where`,
  `flat_map`, `take_while`/`drop_while`/`position`, `sort_by`/`min_by`/`max_by` and
  `zipmap`: a bare name is the function it names — and a record's field or a free fn still
  receives the value it names. A frame's `where`/`filter` keeps reading a bare name as its
  column (`df.where(strong)`), through a parameter or a closure too: the frame reading
  takes the path the wrapper came from, at the one place both engines resolve a column
  expression (`ast_to_colexpr`), which is what retired the parser's exemption for those two
  verbs. Pinned by `a_bound_function_is_the_argument_of_every_verb_that_takes_one` and the
  corpus program `verbs_bound_fn` under both DataFrame backends.
- **A multi-file program resolves the origin too.** The module loader — which renames every
  top-level name to its module and offsets every line into the flat program — never visited
  the origin of a synthesized lambda: `R.all(U)` was "`U` is not defined" in a program with
  an `import`, and only there, reported at a position the offset never reached (the field
  build read it as "the wrong file"), and `L.map(u.twice)` could not name a module's
  function (field build, 1.41). Every walker that rewrites or reads an expression visits the
  origin and the defaults now: the loader, the UFCS pass, the generic visitor (which also
  collects a frame predicate's captures) and the parser's relocation. Pinned by
  `a_bound_origin_resolves_through_the_module_loader`, on three engines, including the
  selective-import UFCS spelling and the refusal's position.
- **`count_where` refuses in its own name.** Its refusals said `filter`, a verb the user
  never wrote: the parser spells `xs.count_where(p)` as a filter followed by a count, and
  the inner node now keeps the verb's name, which every engine reads as a third spelling of
  the filter family (as `where` is) — the same loop, the same fusion, `jit-explain` reporting
  the same two sites — so "`count_where` expects a yes/no test" and "an Int has no method
  `count_where`", on every engine and from the checker (field build, 1.42).
- **A method that does not exist is reported before its argument.** `{a: 1}.nonexistent(it
  * 2)` and `[1].nonexistent(it * 2)` said "`it` is not defined here" (field build, 1.40):
  the checker read the argument before the receiver answered. Array and Record answer first
  now, as String, Tensor and Tuple already did; the refusal flows on to the UFCS fallbacks
  as a value, exactly as theirs does.
- **A bare bound name as a `map`/`any`/`all` argument reaches a function value as the
  value.** The parser reads `xs.map(double)` as `(it) => double(it)` for the array reading
  and used to hand that wrapper to a record's field too: `R.all(U)` gave `all` a function
  where `U` belonged, and the checker refused it as "`U` is a String, not a function"
  (the field build's narrowed finding, after the precedence fix). The wrapper keeps its
  origin now, and a record's field or a free fn via UFCS receives the origin; arrays keep
  the rewrite, and the JIT keeps fusing `xs.map(double)`, typed or through a parameter.
- **A tuple default on a lambda.** `(x, t = (1, 2)) => …` was refused as an unclosed
  tuple: the lambda lookahead ended the default at the first comma, inside the parentheses.
- **`helix doc Connection` lists `execute`.** The write verb was undiscoverable from the
  type; the method table had `query` alone.
- **Two things the new `help` field found on its first day.** The frame's own
  unknown-method arm answered "DataFrame methods: …" (every name) where the general
  builder answers "no similar method — `helix doc DataFrame` lists all DataFrame methods",
  and the two engines reached different arms for `df.map(1)` through a parameter: the
  message agreed, the help did not, and nothing could see it. One wording now. And a parser
  hint recommended `p.0` / `p.1` for a tuple, which is not a spelling Helix has — `p[0]`.
- **A function is equal to itself.** `f == f` was false, `[w].contains(w)` was false,
  `[w, w].unique()` had two elements and `assert_eq(f, f)` failed — every function fell
  through equality to `false`, on all three engines. A function now equals a function of
  the same code with equal captured values: the one definition both engines compute the
  same way (the walker captures free names by value, the VM keeps them as upvalues; a
  top-level `fn` captures nothing on either). So `mk(1) == mk(1)`, `mk(1) != mk(2)`, and
  two lambdas from two sites are never equal. The walker needed the lambda body SHARED
  with the AST (`Rc`) to say "same site" — which also stopped every closure creation from
  deep-copying its body.
- **A default parameter is visible to a call written above its definition.** The parser
  filled defaults while parsing a CALL, from a table that knew only the functions parsed
  so far, so `fn use(x) = g(x)` above `fn g(a, b = 10)` was refused for arity by the
  checker and both engines, and `g(x, b: 5)` was refused as "only supported for
  user-defined functions" — about a function three lines down. Signatures are now
  pre-scanned with the definition's own parameter parser before the first call is parsed,
  inside interpolations too; the lexical `fn`-name scan that only ever fed a since-removed
  rewrite is gone with it.
- **The receiver answers before the arguments are read.** `"s".map(it)` said `it` was
  unbound while `(5).map(it)` said an Int has no `map` — the String, Dna, Tuple and Tensor
  arms of the checker read the arguments before deciding whether the method existed. All
  four now decide first, so the same mistake reads the same way whatever the receiver's
  type.
- **`a Array has no method`.** The runtime's no-method error built its sentence with a
  hardcoded `"a {}"`, while every other runtime mention of a type went through
  `value::with_article`:

  ```
  5      →  an Int has no method `nope`     ✓
  [1]    →  a Array has no method `nope`    ✗
  ```

  `with_article`'s own doc comment names this exact case — "Every other vowel-initial name
  here (`Int`, `Array`) takes 'an'" — so the helper already documented the right answer and
  this path walked past it. One spelling per concept, and the helper is the spelling.

  The guard covers every type the runtime can name, on all three engines, and pins `Unit`
  as the deliberate exception the helper documents: the rule is about **sound**, so it is
  "a Unit" for the same reason it is "a user". Verified red before the fix.

  **The same sentence from the checker was wrong for longer, and pinned as correct.**
  Three runtime sites build "`x` is <article> <T>, not a function" through the helper;
  `types/synth.rs` built it with a literal `"a"`:

  ```
  fn f(x) = x + 1
  f = 5
  print(f(1))        →  error: `f` is a Int, not a function
  ```

  That spelling was the **expected output** in `tests/corpus/m1b_assign_over_fn.expected`,
  so the corpus agreed with it. And a guard for exactly this already existed —
  `src/vm/tests.rs` asserts no message says "a Int" or "a Array" — but every program it
  checks goes through `run_vm`, which cannot reach the checker at all. A guard watching one
  of two producers, with the miss recorded as the answer. Both are fixed, the golden is
  corrected, and the guard now says in as many words what its reach is and where the other
  half is covered.

  Found while sizing the larger **error-family drift** recorded in `docs/dx-plan.md` — the
  checker says `type Array has no method`, the runtime says `an Array has no method`. That
  remains a separate, coordinated change (about 40 pins across 14 files and four corpus
  goldens), and this cycle adds the case for making it twice over: the Tuple bug hid inside
  that gap with the runtime fixed while the checker still refused, and this grammar bug hid
  inside it behind a golden.

- **`to_array` gave a different answer depending on how the binary was built.** Everything
  that was not a Tensor or a tracked Node fell through to the Python bridge, whose
  non-python arm blames the feature for any value at all:

  ```
  to_array([1, 2, 3])
  stock build         →  error: Helix was built without Python support
  --features python   →  [1, 2, 3]        (its own identity arm)
  ```

  Same program, two answers, decided by a build flag — the class this project spends most
  of its guards on — and the message blamed a feature for what was never a Python
  question. An Array is now identity in every build, a Tuple is told about `values()`, and
  whatever is left is named by its TYPE: in a build without python no `PyObject` can
  exist, so nothing reaching there is one.

  The distinction lives in `python::to_array`, where the cfg already is, rather than as a
  second call path at the call site — the first attempt split the caller instead and left
  the bridge uncalled, which `-D warnings` correctly rejected as dead code.

  `python.import` still answers "built without Python support", which is right and has its
  own tests. The new test asserts the negative too: no value the language can classify may
  answer a type question with a build flag.

### Performance

- **A call frame was 72 bytes, and 40 of them were a memo key most calls never use.**
  Every call pushes a `Frame`, and it carried `Option<MemoKey>` inline — the automatic
  cache's key, on every call including the ones that can never memoize. Boxed, the field
  is 8 bytes and the frame is **40**.

  The measurement that found it also answers a question worth stating plainly: **a call is
  free when its arguments are all numeric.** The JIT specializes it away. The cost appears
  the moment one argument is not a number:

  ```
  f(x) = x + 1                    -3.2 ns/iter over the inline expression
  f(x, t)  with a Str argument    91.3
  f(r)     taking a record       131.9
  ```

  So the per-call overhead a String- or Record-threading program pays IS the VM's own call
  path, and the frame was the largest single thing in it. Measured after, min-of-15
  interleaved at load 0.92: a call taking a record **1.067x**, everything else within
  noise, and `scripts/perf-verify.sh` reports no wall or RSS regression across B1..B7 —
  `b3_groupby` 0.04s → **0.03s** with RSS down on three workloads.

  The frame's size is now a **budget** (`vm::frame_size`), asserted rather than printed,
  and its failure message names the fix — the same discipline `Op` already keeps for
  itself. Verified by sabotage: un-boxing the key reports "a call frame grew to 72 bytes
  (budget 40)".

- **Not done, because the measurement said not to: taking a uniquely-held base in
  `{...base, k: v}`.** A record spread copies the base's fields unconditionally, and the
  obvious fix is to take them when the `Rc` is unshared — ADR 0029's take-append-store
  argument, one construct over. Measured, it is not worth it:

  | | ratio |
  |---|---|
  | shared 8-field base | 0.966x |
  | unique 8-field base | 1.170x |
  | fold with a 3-field accumulator | **0.967x** |

  The fold is the case that motivated it, and it does not benefit at all: `reduce` holds
  the accumulator in a local *and* on the stack, so the refcount is never 1. Reverted.
  Recorded here because the idea is a natural one to have twice.

  What the same measurement did establish: a spread costs **~100 ns fixed + ~5.6 ns per
  base field**, and is *cheaper* than rebuilding the same record from a literal (280 ns vs
  573 ns at 32 fields). A field build's 0.64 µs was five or six spreads, not one slow one.

### Added

- **A Tuple answers `count()`, `length()` and `values()`, and `helix doc Tuple` works.**
  It answered nothing at all before — not its length, and not the doc command, which
  replied *"unknown type `Tuple`"*:

  ```
  (1, "a", true).count()   →  error: type Tuple has no method `count`
  helix doc Tuple          →  error: unknown type `Tuple`. Try one of: Array, String, …
  ```

  That is hard to defend for a type the stdlib hands back from `enumerate`, `zip`, `top`,
  `frequencies` and both `items()` methods, and that ADR 0025 orders with `<` and accepts
  in `min_by`. Nothing recorded it as a decision — "tuples own nothing, by design" was an
  inference drawn from the silence, by a field build and by me. The silence was a gap, and
  it cost that build a hand-rolled `count / 2`.

  `values()` is the explicit bridge to the Array surface, named as Record and Dict name the
  same operation. A tuple deliberately gains nothing sequence-shaped of its own: no `map`,
  no `filter`, no `first`. Going through `values()` says at the call site that you are
  treating a fixed-size positional product as a sequence. A **homogeneous** tuple types as
  an `Array` of that element, so `(3, 1, 2).values().sum()` type-checks; a mixed one gives
  `Array<Unknown>` rather than a guess that would pass the checker and fail at run time.

  **The interesting half was the split.** Teaching the runtime alone left this:

  ```
  (1, 2).count()                  →  error: type Tuple has no method `count`
  {a: 1}.items().map(it.count())  →  [2]        ← worked
  ```

  Same `Value::Tuple`, two answers. The second receiver is `it`, whose static type is
  Unknown, so the checker waves it through to the runtime that now answers; a literal tuple
  has type `Tuple`, and the checker's receiver router had no arm for it. The two messages
  say which side spoke — `type Tuple has no method` is the checker, `a Tuple has no method`
  is the runtime — and the regression test asserts both receivers for that reason.

- **`has_feature(name)` — ask before calling, instead of provoking the failure.** ADR 0032
  gates the BODY, not the name: `re_match` in an appliance build still exists,
  type-checks and describes itself, and running it says what to rebuild with. What a
  *program* could not do was ask BEFORE calling, so a library that wanted to degrade
  gracefully had to provoke the error and catch it. A field build carries exactly that as
  a standing workaround.

  ```helix
  fn norm(s) = if has_feature("regex") then s.re_replace("[^a-z]", "") else s.lower()
  ```

  That is the same charge `type_of` was added to remove, in the words of its own doc
  comment: a language that can only discover something by provoking an error charges an
  exception for a question — measured there at 36x a plain lookup.

  Measured across two real builds of the same source: the default answers
  `regex=true, jit=true, native-df=true, appliance=false`; `--no-default-features
  --features appliance` answers `regex=false, jit=false, native-df=true, appliance=true`.

  **An unknown name is an ERROR, not `false`.** A typo answered with `false` would send a
  program down its fallback path forever, on every build, with nothing to see — the exact
  shape of a silent wrong answer this project keeps removing. Every feature Cargo.toml
  defines answers truthfully: `appliance, bio, database, dataframes, default, http, jit,
  managed, mimalloc, native-df, postgres, python, regex`.

  It reads a compile-time constant, so it is pure — `helix effects` reports
  `no authority, reproducible`. Its regression test asserts each answer against the
  compiler's own `cfg!` rather than against a fixed value, so it stays true in an
  appliance build and goes red if an arm is wired to the wrong feature.

- **`join` takes an options record, so the join TYPE can come from a binding.** It could
  only be a trailing string *literal*, so a library had to branch over five constants:

  ```helix
  fn on(l, r, k, how) = l.join(r, k, how)      # error: no column `left`
  fn on(l, r, how)    = l.join(r, @id, how)    # error: no column `left`  <- key PINNED
  ```

  The second line is the diagnosis: with the key pinned there is nothing for `how` to be
  confused with, and it still fails. The type was recognised by its SYNTAX, so every bare
  name in the argument list landed in the key set. Now:

  ```helix
  fn on(l, r, k, how) = l.join(r, k, {how: how})     # inner | left | right | outer | full
  l.join(r, @id, "left")                             # existing sugar, unchanged
  ```

  **Why a record rather than reading the value.** The tempting fix is "the last argument
  is the type if its value is a join kind". It trades a refusal for a wrong answer:
  `l.join(r, k1, k2)` where `k2` is `"left"` and no such column exists is a clean
  "no column `left`" today, and would silently become a left join on `k1` alone — a typo'd
  key quietly changing the join. Positional varargs plus an optional trailing value of the
  same type cannot be disambiguated, so the disambiguation is written down instead. A
  record can never be a key, so it works at any key count, leaves every existing call
  untouched, and extends to future options without another positional slot. It is the
  idiom `http_request({method, url, body?, headers?})` already uses.

  Misplaced options say so (`join options must come last`) rather than falling into the
  generic key error, and an unknown option is refused rather than ignored.

- **A string literal is a join key.** `df.select("price")` has always named a column, but
  `join` was the one name position that refused one, because a string was only matched at
  the trailing index where it means the join type. A string BEFORE the last argument is now
  a key — unambiguous only because the options record marks the type. A lone trailing
  string keeps its existing meaning, so `l.join(r, "id")` still reads `"id"` as the type
  and refuses for want of a key rather than silently changing what shipped.

- **`df.rename(old, new)`** — the same column under a different name, in the same
  position. A frame had no way to say this, and it was the last thing blocking a generic
  relation-attach in a library: aligning a child's foreign key to a parent's key IS a
  rename.

  ```helix
  fn attach(child, parent, fk, pk) = child.rename(fk, pk).join(parent, pk)
  attach(posts, users, "author_id", "id").columns()   # ["post", "id", "name"]
  ```

  Both arguments are ordinary evaluated strings, like `unique`'s and `column`'s, so a
  library passes its own parameters straight through — no new syntax, and none of ADR
  0028's name-position machinery. The `with`-value route could never express this: in an
  EXPRESSION position ADR 0028 makes a binding its *value*, so `f.with({to: from})`
  inserts the literal text `"author_id"` rather than that column's data. A rename is two
  NAME positions, which the language already spells.

  **Renaming onto an occupied name is refused**, with a message that says what it would
  have cost — silently discarding that column is a wrong answer wearing the shape of a
  successful call. Renaming a column to *itself* is a no-op rather than a collision:
  refusing it would make `rename(fk, pk)` fail exactly when the child already used the
  parent's name, which is the case a caller is least able to predict.

  Implemented once, as a **provided** trait method — "copy the column under the new name,
  then project in the original order" — rather than twice, so the two DataFrame backends
  agree here by construction instead of by two implementations that happen to match. The
  cost is two column-set rebuilds where a native relabel would need one; that is worth
  overriding when a measurement asks, and none has.

### Fixed

- **A column name captured from an enclosing scope resolved on one engine and not the
  other.** ADR 0028 says a binding in scope names the column; the VM implements that by
  carrying the call site's LOCALS in the column-verb op, and a lambda's capture is not a
  local of its own frame:

  ```
  fn f(d, lo) = ((x) => x.where(@ts > lo))(d)
  tree-walker: 2        VM / JIT: no column or variable named `lo`
  ```

  The capture was not merely unread — it was never **created**. A column verb's arguments
  are never compiled as expressions, so nothing ever asked to resolve a name inside them,
  so no upvalue was ever registered; the list would have been empty however carefully it
  was consulted. The compiler now offers every bare name in those arguments to
  `resolve_upvalue`, which mints the capture when the enclosing environment has one, and
  both engines consult the same three scopes in the same order: locals, then upvalues,
  then globals — the order a parameter shadowing a capture requires.

  Found by a field build within a day of v0.9.0, because nothing in the corpus put a frame
  predicate inside a lambda. Seven shapes are now covered on all three engines, and an
  enclosing `let` resolves as well, which is the same rule.

- **`with` and `join` now take a column name from a binding.** `select`/`sort`/`group`
  resolve a bare word through the column-name rule — a binding in scope wins, `@name` pins
  the column. ADR 0028 decided that for the positions where a name is READ and left the
  DEFINING position open in as many words ("does the same rule apply to the name being
  DEFINED?"); a join key it never reached at all. Both kept the old behaviour, and both
  failed in the two different ways the ADR was written to prevent:

  ```
  fn rename(f, to) = f.with({to: @author_id})
  rename(df, "id").columns()      # ["author_id", "to"]  -- a WRONG ANSWER, no error

  fn on(l, r, k) = l.join(r, k)
  on(a, b, "id")                  # no column `k` in the left frame  -- a REFUSAL
  ```

  Both are "a library's own parameter names are reserved words in data it has never seen",
  which is the sentence ADR 0028 opens with — and the argument that settled the read
  positions is about the library author's blindness to the caller's schema, which defining
  a column shares exactly. A `with` record's KEY and a join key are name positions, not
  expressions, so they now resolve the way every other name position does, and only a
  `Str` binding counts — a name bound to a number or a frame is a type
  mistake, not a column, and treating it as one would turn that mistake into a silent
  lookup of something that can never exist. All three engines agree across eight shapes.

  All three engines had *agreed* on the old behaviour, so the differential oracle could
  never have found this: unanimous is not the same as correct.

- **A clock test could fail on a host event.** `now()` was asserted to advance across two
  processes, unconditionally, on the reasoning that a WSL2 clock resync could only make the
  advance too small. A resync can step the clock **backward**, and then that assertion fired
  before the retry loop below it — whose condition the same step trips — ever ran:

  ```
  bash `date`      helix now()      advance
  1788318926.990   1788318926.333   -0.660   (bash itself: -0.655)
  ```

  The two agree to 5 ms, so `now()` was reporting the wall clock it was given, correctly.
  The claim is now made against this process's own `SystemTime`, which a resync moves
  identically — so it cannot flake, and it is strictly stronger: it also catches
  milliseconds reported as seconds, a timezone baked into the value, and the wrong epoch,
  none of which "is it bigger than zero" could see. Verified by sabotage: a +3600s and a
  +200s offset both go red, a +30s offset stays green inside the stated window.

- **A `String` or `Bool` argument silently cost up to 46x, by falling off the automatic
  memo cache.** The cache keyed on `Int` and `Float` only, so threading a tag through a
  recursion — an ordinary shape — quietly turned a linear function exponential again:

  ```
  fib(30), identical work, only the ARGUMENT SHAPE differs
  f(n)                     0.006s
  f(n, tag)   tag: Str     0.230s      -> 0.005s
  f(s)        s: Record    0.439s      -> unchanged, and deliberately so
  ```

  A `Str` keys up to 64 bytes (`MEMO_MAX_KEY_BYTES`); past the cap the call is not
  memoized, which is correct and merely uncached — never a truncated key, which would let
  two different strings collide and return a wrong answer. A Record, Array or frame is
  still never a key: those are `Rc`-shared structures of unbounded size, so hashing one
  means walking it on every call and then retaining it for the life of the table.

  **The two halves of the rule are now one definition.** Eligibility was a list at the
  call site (`all(matches!(v, Int | Float))`) while the projection carried an unreachable
  `_ => MemoArg::Int(0)` for everything else. They agreed, so nothing was wrong — but
  nothing held them together, and the failure drift produces is the worst kind available:
  every ineligible value keys as the SAME `Int(0)`, so unrelated calls collide and return
  each other's results, silently, with all three engines agreeing. Eligibility is now
  *defined* as "every argument projects".

  Verified by sabotage: projecting `Str` to a constant returns `1094600 1094600` where the
  answers must differ, and `Bool` likewise `89000 89000`. Removing the length cap stays
  green, correctly — an uncapped key is still a right key, only an unbounded one — and the
  test says so rather than implying coverage it does not have.

- **The performance gate reported PASS when every workload failed to run.**
  `perf-verify.sh` is what enforces "no regression", and it `cd`s to `bench/crosslang`
  before measuring — so a binary path given relative to where the caller stood stopped
  resolving. Every run then failed to the script's own sentinel, and 999/999 is 1.00:

  ```
  b1_scalar    999s->999s (1.00)   1024K->1024K (1.00, +0MB)
  ...
  PASS — candidate has no wall/RSS regression vs baseline
  ```

  Seven dead workloads, reported as a perfect result, by the one gate whose whole job is
  to refuse that. Found by running it. Three fixes, smallest first: both paths are
  absolutized *before* the `cd`, which makes the documented usage work rather than merely
  fail loudly; a preflight runs one workload on each binary and exits 2 if either cannot;
  and the sentinel is treated as a failure wherever it survives instead of as a number to
  divide. Verified by sabotage — the original relative-path call now measures correctly,
  a missing binary and a binary that cannot run a workload both exit 2 with the reason.

### Documentation

- **Float arguments are memoized, and three places said they were not.** The exclusion was
  real once and was replaced rather than kept: floats key by `to_bits`, so bit-identical
  floats share a result, `+0.0` and `-0.0` key separately instead of wrongly sharing, and a
  NaN keys against itself. Measured: `fibf(32.0)` runs in 0.005s against 0.006s for the
  integer `fib(32)`, while the same recursion made non-memoizable takes 0.124s at
  `fibn(28.0)` — 25x longer on a quarter of the work. `caching-and-memory.md` also now
  states the rule that follows from this and was nowhere written down: a **non-number**
  argument is never a key, so a function taking a Record or String spec is not memoized.

## v0.9.0 — 2026-09-01

### Changed

- **A method call is resolved by its receiver, not by its name.** (ADR 0045.) UFCS was
  gated at PARSE time on `registry::is_any_method` — a global test on a name, made at a
  point where the receiver does not exist. Every good verb name is some type's method, so

      where  select  first  count  all  any  join  sort  take  drop
      insert  get  keys  values  filter  map  sum  min  max  unique

  were all unusable by a user's own library: `fn where(q, c)` two lines above `q.where(c)`
  was invisible, and the call died with ``type Record has no method `where` ``. A query
  builder, an ORM, a pipeline DSL — none could be written in Helix, and nothing said why.

  A failed dispatch now retries as `name(recv, args…)` against a declared `fn`, then
  against a builtin. A type that OWNS the name never falls back, so an array keeps its
  comprehension and a DataFrame keeps its column verb — **in the same program**, chosen by
  what the verb is called on:

  ```
  fn where(b, c) = { tbl: b.tbl, conds: b.conds.concat([c]) }

  q("people").where("age > 40").select("name").all()   # the user's verb
  [1, 2, 3].where(it > 1)                              # the comprehension
  frame.where(@age > 40)                               # the column verb
  ```

  The two families the compiler routes by TYPE — `select`/`group`/`with`, and the
  comprehension loops — have no dispatch to fail, so they now emit BOTH readings behind a
  new `Op::ReceiverIs`, the receiver-test opcode `docs/dx-plan.md` said this would need.
  The receiver is compiled once into a hidden local, so its side effects happen once
  whichever branch runs. The tree-walker CALLS that opcode's predicate rather than
  restating it, and a wrong-arity fallback raises from the one `arity_err` both engines
  already share.

  A local, a global, or an alias (`h = id`) of the same name still shadows the function,
  identically on both engines. The parser's own desugars (`sort_by`, `take_while`,
  `zipmap`, `position`, …) still win, which is recorded in `docs/dx-plan.md` rather than
  left to be discovered. Nothing is spent on the success path: the argument clone the old
  fallback made BEFORE dispatch is gone, because `call_method` only borrows them.

  Found and fixed on the way: `Op::DfColumnVerb` raised its own sentence for a
  non-DataFrame receiver ("expected a DataFrame, got Record") where the tree-walker raised
  the ordinary method error — a pre-existing divergence, reachable whenever the checker
  could not pin the receiver down.

- **The native DataFrame engine is the default; polars is now the oracle only.**
  (ADR 0033 Stage 4.) `default` and `bio` pull `native-df`. Polars stays behind the
  `dataframes` feature because an engine cannot be its own evidence —
  `scripts/dfdiff.sh` running every tracked program under both engines is what says
  the replacement means the same thing, so the oracle outlives the default it
  replaced.

  | | with polars | shipped now |
  |---|--:|--:|
  | binary (full features) | 120 MB | **31 MB** |
  | stripped | 77 MB | **20 MB** |
  | crates compiled | 1,566 | **192** |
  | startup (like-for-like) | 4.9 ms | **2.96 ms** |
  | startup (appliance profile) | — | **2.5 ms** |

  **The feature that adds a second engine inverted**: a dual-engine build is
  `--features dataframes` now, not `--features native-df`. A dual build also
  DEFAULTS to native, so a dev binary answers exactly as a shipped one does —
  it defaulted to polars, which would have had every developer and every CI job
  exercising the oracle while every user ran the shipped engine.

  Every verb is faster. At 1.6M rows on materialised frames with every output
  consumed, min-of-7 (polars → native): `group` 20.7 → **5.2 ms**, `join`
  84.6 → **29.9**, `unique("col")` 9.0 → **3.2**, `with` 49.0 → **19.8**, `sort`
  74.5 → **38.1**, `where` 26.0 → **13.6**, `unique` 33.2 → **23.4**.

  `--features python` still requires `pyo3-polars`; it is the one configuration
  where polars reaches a shipped binary.

### Fixed

- **A single `import` line silently disabled two features.** `module::load` namespaces
  every top-level name once a second file is involved (`fn where` is stored as
  `m0$where`), and two lookups asked for the name written in the source instead:

  - the ADR 0045 **UFCS fallback** — a method call site says `where`, because a method name
    is not a top-level name and must not be rewritten. The lookup missed, and "no such
    function" is indistinguishable from "no fallback", so the call died with the pre-UFCS
    error. The feature was present in exactly the files that do not need it and absent from
    every file that does — a library's consumer always has an import.
  - **`fn main`** (ADR 0037) — `climain::find` matched `name == "main"`, so a program that
    took command-line arguments became one that refused them, and D6's refusal of an
    unbindable parameter stopped firing with it.

  A method call site now carries the name its fallback resolves to, filled by the loader
  with the same precedence a free call gets; `climain::find` takes the entry module's
  prefix (matching any `*$main` would have let an imported library's `main` hijack the
  entry point). Neither was reachable by any existing test, because every UFCS corpus
  program and every `fn main` test is a single file — the regression test is a multi-file
  one.

- **`helix effects` leaked the loader's namespaced names.** A multi-file program rewrites
  every top-level name to `m<N>$name`; error messages strip that before display, and the
  report did not — so it printed `m8$version` and `m9$_status`, a spelling that appears in
  no source file and that nobody can grep for. Both the text and `--json` forms show the
  names the source uses now.

- **A `Connection` did not own its method names.** It was in no method table, so
  `type_owns_method` answered false and a user's own `fn query(c, sql)` silently took a
  call meant for the database — matching arities, no error, a program that never reaches
  the server. `helix doc Connection` now answers too.

- **A frame or a group reaching a function as an untyped parameter lost its verbs.**
  Every DataFrame verb is routed by TYPE, and when the checker could not prove the
  receiver the compiler guessed — evaluating the arguments as values, where a frame verb's
  arguments are column names:

  ```
  fn adults(f) = f.where(age > 40)     # bare column names, ADR 0039's own spelling
  walker: 2                            VM / JIT: `age` is not defined
  ```

  A `@column` argument was the only hint that switched the route, and a bare name carries
  none — so the shape that broke is the one a database helper is written in. The same
  guess was in `sort`, `drop_missing`, `drop_nan` and every grouped aggregation
  (`g.mean(v)` → "`v` is not defined"), so the fix covers the family: both readings behind
  `Op::ReceiverIs`, decided on the value. **Twenty cases across the two receivers now agree
  on all three engines, where twelve diverged.**

  One consequence worth naming: the String predicates a query has had since v0.7.0
  (`@name.starts_with(p)`, `re_match`, and a bare `name.starts_with("a")`) now survive
  a receiver the checker cannot prove, so a helper that takes a frame as a parameter
  reads exactly like the one that takes a connection. That is one query surface rather
  than two, and it is what the routing fix buys beyond the bug it closes.

  It costs nothing measurable: an unproven receiver never fused anyway (the chain analysis
  cannot see an array through a parameter), so guard off → on leaves the unproven/pinned
  ratio at 1.335 → 1.341 on 4M elements. `map` is untouched — a frame owns no `map`, so
  there was never a second reading to choose between.

  Six error-wording divergences went with it: a group was refused only after its arguments
  were evaluated, so the message named `v` rather than `sort`; `scan` on a bad receiver
  reported "no method `map`", naming a call that appears nowhere in the source; and four
  sentences were written once per engine. Each is a single constructor now.

- **A split's other branch skipped ordinary method dispatch.** The first version of the
  fix below sent every non-frame receiver straight to the user's function, so a type that
  OWNS the name lost its own method: `Array` owns `join`, and `xs.join(",")` through an
  untyped parameter went to a four-parameter `fn join` — the tree-walker answered and the
  VM raised. Every split now emits the ordinary `Op::Method` with the fallback attached,
  which is the same rule the walker runs, rather than re-deciding it.

- **A user's `fn join` was taken by the DataFrame's `join`, and the engines disagreed.**
  `join` is compiled by type like the column verbs, but with no requirement that an
  argument mention a `@column` — a key may be written bare — so an `Unknown` receiver
  routed to the frame join whatever the arguments were. It took a chain to reach: a record
  literal types as `Record` and declines, while a user verb's unannotated return is
  `Unknown`. The tree-walker dispatched on the runtime value and answered; the VM raised
  ``` `join` takes 1 argument, got 3 ```. It now emits both readings behind
  `Op::ReceiverIs` like the other two families.

- **A doc example could not expect padded output.** The expectation lives in a `##`
  comment, where trailing spaces are invisible and every formatter strips them — so they
  are trimmed from the expectation, and must be. The comparison then trimmed only the END
  of the whole output, which reaches the last line and no other, so a fixed-width column
  anywhere above it could match nothing writable. Both sides are compared line by line now.
  Leading whitespace is still significant: indentation is structure.

- **The doc-example lint could be satisfied by an example that never runs.**
  `helix test` reads `>>>` examples from `##` doc comments only — a plain `#` is prose.
  The lint counted a `>>>` on any comment line, so `# >>> f(21)` cleared the finding and
  executed nowhere. A codebase that comments with `#` throughout would have closed every
  finding that way and finished with a green lint over examples nothing runs, which is the
  one thing the rule exists to prevent. The lint now requires `##`, says so in the message,
  and a test asserts the two AGREE rather than testing either alone.

- **The long-`let` lint had its two constructs the wrong way round.** `do { … }` and
  `let a = …, b = … in …` lower to the same node, while `let … in let … in` is a nest of
  single-binding nodes — so the rule fired on `do` blocks, advising `do { … }`, which they
  already were, and never fired on a chain at any length, which is its actual target. The
  node records which form it was written as, and the rule counts the chain.

- **A password could reach an error message.** `postgres_query`'s URL parser echoed the
  whole raw URL back when it failed to parse. An error is the most widely copied text a
  program produces, and a credential that reaches one has escaped.

- **`read_csv` accepted ragged rows on one engine and refused them on the other.**
  A short row pads with missing and a long row truncates to the header — the
  behaviour `tests/cli.rs` records as policy, because files real pipelines emit
  should stay countable and readable column-wise. The native engine errored
  instead. Making `read_csv` strict is a decision that belongs in an ADR, not a
  side effect of swapping engines.

- **A bad join key did not say which frame was missing it, on the native engine.**
  `validate_join_keys` is the seam's shared diagnostic — "so every backend produces
  identical Helix error messages" — but it had exactly one caller, in
  `backend/polars.rs`. Native fell through to a generic column lookup and dropped
  the only part a join error needs. A `#[cfg_attr(not(feature = "dataframes"),
  allow(dead_code))]` on the validator had been quietly accepting that a
  native-only build compiled it unused.

- **`helix` told you to rebuild with the engine you already had.** The hint for an
  unavailable `HELIX_DF_ENGINE` named a fixed feature; it now names the feature
  that supplies the MISSING engine.

- **A `now()` test failed about one gate run in four.** WSL2 resyncs the guest wall
  clock against the host, so `now()` can advance less than a `sleep` while
  `clock_monotonic` reports it correctly. The test retries rather than skipping,
  because skipping would make a real defect indistinguishable from a host
  correction — a guard that cannot fail.

### Internal

- **Five corpus programs, covering ground the differential had never touched.**
  All three divergences above were invisible for one reason: of 157 corpus files,
  **none read a CSV** and none joined on a bad key. A differential is evidence only
  for what its corpus exercises. `df_ragged_csv`, `df_join_bad_key`,
  `df_unique_keys`, `df_group_keys` and `df_join_dense_edges` take dfdiff from 129
  to 134 programs.

- **`bench/df/`** — a DataFrame benchmark that refuses to measure above load 1.5,
  asserts both engines printed the same thing before reporting any timing, and
  treats a divergence as outranking every number. Every program consumes its result
  through `.column(...)`: a lazy engine answers `.count()` without materialising
  anything, and timed that way polars read 0.11 ms for a sort that costs 74 ms.


### Added

- **PostgreSQL, spoken directly.** (ADR 0044.) `postgres_query(url, sql, params?)` returns
  a DataFrame; `postgres_open(url)` returns a connection that answers
  `c.query(sql, params?)`. Behind `--features postgres`, verified against a live
  **PostgreSQL 19 Beta 3**.

  ```
  db = postgres_open("postgres://user:pw@host/db")
  db.query("select name, age from people where age > $1", [40])
    .where(@age < 55)
    .select(@name)
  ```

  **Zero new dependencies.** The wire protocol has been frozen since 2003 (18 added 3.2,
  19 carries it backward-compatibly, and `libpq` still requests 3.0), and SCRAM-SHA-256
  needs only `sha2`/`hmac`/`base64`/`OsRng` — all already core. `libpq` would have ended
  the binary's no-system-dependency property; a driver crate would have brought an async
  runtime into a synchronous language.

  **TLS is on by default, and the server cannot turn it off.** `libpq` defaults to
  `sslmode=prefer`, and so does Go's `pgx`: if the server answers "no TLS", the session
  continues in plaintext and the password exchange is readable by anyone on the path.
  Helix has two modes rather than six — `verify-full` (the default: TLS, chain to a trusted
  root, hostname checked) and `disable` (plaintext, spelled out by the person who wants
  it). `require` and `verify-ca` are refused *with the reason*, because each is a trap with
  a name; a private or provider CA is a file (`sslrootcert=`), never a switch that turns
  checking off. An unknown `sslmode` value or an unknown parameter is an error, not a
  silent default. Measured cost: **+1.4 ms per connection, nothing per query.**

  **Read-only from the first byte**, sent as a startup parameter rather than a
  `begin transaction read only` — so the session is read-only before a statement could be
  sent, at zero round trips. An `insert` or a `create table` comes back as SQLSTATE 25006
  from the server, not from a client-side blocklist.

  **There is no `close` to forget.** Helix values are reference-counted, so a connection's
  socket shuts when the last handle goes — deterministically, the same rule `Lock` relies
  on. Five queries cost **20.7 ms through five connections and 6.0 ms through one**;
  a `pg_stat_activity` check pins that three connections opened inside a function are gone
  when it returns.

  Parameters are values (`$1`, never string interpolation), `NULL` reads as `missing`,
  unknown column types read as text rather than failing, and the capability label is `net`
  — refused without it, like every other network verb.

- **`[capabilities]` in `helix.toml` is a real authority ceiling.**

  ```toml
  [capabilities]
  fs = "read"     # omitted | "read" | "write" | "all"
  net = "on"      # omitted | "on"
  process = "on"  # omitted | "on"
  ```

  The manifest used to *refuse* this table on purpose: `deny_unknown_fields` was added
  because a `[capabilities]` block that parsed and did nothing "looked like it restricted a
  program's authority and did nothing at all — the worst shape a security control can have".
  This is that block finally meaning something.

  - **Present means enforced.** A table that declares `net` and says nothing about `fs`
    denies both filesystem effects, **with no environment variable anywhere**. A declaration
    that only takes effect when someone remembers to set a variable is not a declaration.
  - **The environment narrows, never widens.** `HELIX_ALLOW_FS=all` against `fs = "read"`
    still denies writes. That is what makes the file worth reading: it states the most the
    program can do on *any* machine, under any deployment. `HELIX_CAP=audit` cannot weaken a
    declared ceiling either — audit *allows* the access it logs, so honouring it would let
    the environment widen authority by spelling a mode.
  - **But it can narrow.** `fs = "all"` declared with `HELIX_ALLOW_FS=read` denies writes, so
    a deployment may hold a program to less than it declared.
  - **Absent means today's behaviour**, so nothing existing breaks. Default-deny would break
    every program, and security that makes a tool unusable gets turned off wholesale.

  A dependency's `[capabilities]` is **not** consulted: a library cannot grant itself
  authority the importing program did not declare. A misspelled grant (`fs_read = "on"`) is
  refused rather than read as an absent one, and an unparseable value (`net =
  "example.com:443"`, which is what ADR 0021's eventual host:port design would suggest) names
  the key and says why.

- **`installing_a_package_executes_no_code`** guards a property that was an accident.
  Resolving a dependency reads bytes, verifies a hash and unpacks an archive — no `setup.py`,
  no `postinstall`, no `build.rs` — so a compromised package cannot act until a program
  imports and calls it. Unlike the unwrap budget it is modelled on, it has **no raise path**;
  and it was verified to *fail* (injecting a `Command::new` reports the file and line), not
  merely to pass.

### Changed

- **A package name is validated where its author can fix it.** The identifier rule was
  enforced on a *consumer's* dependency key while a package's own `name` went unchecked, so a
  package could call itself `my-package` and hand the error to whoever depended on it.
- **A dependency key must agree with the package it points at.** `helix add ui --path ./web`
  wrote a key `ui` pointing at a package named `web`, and since `import ui.x` resolves
  *through the key*, the program imported a package under a name its author never chose.
  Refused now, naming both. A target with no manifest declares no name and so cannot
  disagree — that stays allowed.


- **`helix build` says which runtime a program needs.** `--runtime` made the artifact's
  size a choice; it did not make it an informed one. The only way to learn whether a
  program touched a DataFrame, a genomics reader or the HTTP client was to build against a
  smaller runtime and watch it fail at run time.

  ```
  built standalone executable: prog (6.7 MB)
  needs: http, regex
  ```

  or, when nothing optional is reached, the suggestion **and what else it costs** — because
  the first line alone reads as "costs nothing":

  ```
  needs: no optional feature
    `--no-default-features` would serve this program; that also drops jit and mimalloc,
    which change speed, not answers
  ```

  **The classification was measured, not reasoned about** — every candidate run against a
  `--no-default-features` runtime. Guessing got four wrong: `read_bed` / `dna` / `align`
  need nothing despite living in the genomics module; `read_json` returns Helix values, not
  a frame; **`listen` is ungated, because `http` gates the client, not the server** — which
  is exactly why a minimal runtime still serves HTTP; and `re_replace` looked ungated only
  because the probe called it with the wrong arity, so an arity error masked the gate.

  Getting it backwards matters in one direction: telling someone they do not need
  `dataframes` hands them an artifact that dies on its first frame. So the pass walks the
  exhaustive `visit::walk_stmt` (a new `Expr` variant fails compilation rather than being
  skipped), `every_builtin_declares_its_feature` pins the gated set, and the pass
  over-reports rather than under-reports.

### Changed

- **`build`, `new`, `add`, `sync` and `verify` print one report structure.** Five
  hand-rolled `println!` blocks is five chances to drift; one structure renders a terminal
  an aligned, coloured block and a pipe flat `label: value` lines, from the same rows in
  the same order.

  The plain form's *shape* is part of the contract — one fact per line, no alignment
  padding to strip, and no multi-byte separator in a line a script splits. Rendering it
  found three defects: a 212-byte artifact reported as `0.0 MB` (the same category error as
  an axis tick printed with the value formatter), the rich middle dot leaking into piped
  output, and notes running off the terminal to wrap mid-word under the value column.

  `helix sync` now shows each package beside its hash prefix, since the hash is the point of
  a lockfile.


- **`helix build` bundles a program AND everything it imports.** Any program with an
  `import` was refused outright, so a Helix program with a library could not be shipped at
  all. Nothing in the design required that: the overlay held one source string, while the
  module loader had already collected every module's source. The workaround — inlining by
  hand — pushed a field user into reimplementing Helix's lexer, where they got the `{{`
  doubling convention wrong and desynchronised 15 KB into one token.

  **A bundled program is loaded by the same resolver an interpreted one uses.** `load_file`
  and the whole import ladder are untouched; only the three questions they ask of the world
  — does this path exist, what is its canonical form, what does it contain — are answered
  from the bundle's archive instead of the filesystem. So an import cannot resolve one way
  from source and another way from a bundle, which is a stronger property than two
  implementations that agree today.

  Modules are keyed by their path **relative to the project root**, recorded at the moment
  of resolution rather than derived afterwards from a canonical path — only the resolver
  knows which rung of the ladder matched. Two consequences fall out:

  - Errors inside a bundled dependency name the module (`util.helix:1:32`), not the build
    machine's directory layout, which storing canonical paths would ship inside every
    artifact.
  - **Two files claiming one key is refused, naming both.** A package dependency outranks
    the project root in the ladder, so a dep's `mathlib/go.helix` and a project file reached
    as a sibling at that path are distinct files with one key. The pinned case *interprets
    correctly* — both modules load, printing `2 1` — so silently archiving one of them
    would have shipped an artifact that answers differently from the program it was built
    from.

  `listen(port, shards)` works in a bundle: a shard cannot re-read a path, because a bundled
  program has none, so it re-enters through the archive.

  The overlay is versioned `HLXBND02` and **`HLXBND01` is still read** — `--runtime` means
  the binary that writes an overlay and the one that reads it need not be the same version.

### Fixed

- **A write that fails is an error, not a bug report — and not silence.** The four output
  sinks disagreed about what a failed write means, and both answers were wrong.

  `print` used `println!`, which **panics** on a failed write. `helix run prog.helix >
  /dev/full` reached the user as

  ```text
  error: internal error (.../stdio.rs:1166): failed printing to stdout:
         No space left on device (os error 28)
  help: this is a bug in Helix; please report it with the program that triggered it.
  ```

  with exit 134 and a core dump — telling the author of a correct program to file a Helix
  bug because a disk filled up. ADR 0024 says user input never aborts the host.

  `emit` / `write` / `elog` went the other way and discarded the error, on the stated
  grounds that "errors writing to a closed pipe are the consumer's business". That was true
  when written and **expired when `main.rs` restored `SIGPIPE` to `SIG_DFL`**: a closed pipe
  now kills the process by signal, like any other Unix tool, and never surfaces as an
  `Err`. So that arm could only ever swallow a *genuine* failure — reporting success for
  output that never landed.

  All four now report `could not write to stdout: <cause>` and exit non-zero.

- **The last line a program prints must reach the device.** Rust flushes stdout as the
  process exits and **discards the result**, so a program whose output never landed still
  exited 0. A single `print` to a full device reported success while writing nothing, while
  enough output to overflow the line buffer failed correctly — the worst shape a bug can
  take, working on the big case and lying on the small one. `main` now flushes stdout
  itself and makes a failure there the exit code.

  The flush is on the exit path rather than in `print` deliberately: putting it in `print`
  would buy correctness with a syscall per line forever.

### Changed

- **`print` costs 2.5x less.** Measured on 200,000 lines piped to `/dev/null`, minimum of
  five runs, same binary configuration before and after:

  | | before | after |
  |---|--:|--:|
  | wall time | 0.25 s | **0.10 s** |
  | overhead per call | 1.20 us | **0.45 us** |

  Three removals, no behaviour change:

  - **`RenderOpts::auto()` now detects the environment once.** It ran per `print`: an
    `isatty`, a `TIOCGWINSZ` ioctl, six `env::var` lookups and three string matches — two
    syscalls for every line a program ever wrote. Nothing can change the answer under a
    running program, since no builtin sets an environment variable. Terminal *width* stays
    live, because a terminal can be resized and rich output is human-paced; the piped path,
    which is where a program writes a million lines, never asks.
  - **The capture copy is no longer built when capture is off.**
    `captured(&format!("{s}\n"))` allocated a second copy of every line printed, purely to
    pass it to a function whose first act is to return `false` unless `helix check` armed
    the sink.
  - **One argument takes a direct path.** `join` on a one-element `Vec` allocates a second
    `String` to copy the first into; `print(x)` now hands the rendered value straight back.

### Added

- **`helix build --runtime <path>` — a bundle that does not carry its own interpreter.**
  `helix build` produced a self-contained executable by embedding the runtime, which is the
  right default and the wrong only option: a directory of twenty tools shipped twenty copies
  of the same interpreter. Pointing a bundle at a runtime already on the machine takes it
  from **2.4 GB to 6.7 MB, byte-identical in behaviour**.

  A bundle is refused as a runtime. Nesting one inside another would produce a file that
  looks like a program, runs like a program, and re-enters the loader a second time.

- **`HELIX_PLOT=braille | blocks | ascii` — the glyphs a chart draws with.**

  This exists for the reason `HELIX_BOX=ascii` does, and the measurement came first: every
  row a plot emits is *exactly* the same width, in every glyph set, pinned by
  `plot_rows_are_column_exact`. So a plot that arrives sheared into scattered dots is not a
  bug on this side to hunt — it is the terminal's FONT rendering the braille dots at a
  different advance width from the braille blank (U+2800) they are padded with, and nothing
  here can repair a font. What this side can do is offer a set the font certainly has.

  Resolution falls `2x4` → `2x2` → `1x4` dots per cell. The ASCII ramp picks by the mean row
  of the lit dots rather than collapsing the cell, so a rising curve still rises — a
  fallback that renders but cannot be read would be worse than none.

### Changed

- **An axis tick is a POSITION, and stopped being formatted as a value.** `fmt_num`
  preserves what a reader needs to round-trip a result. That is correct for a printed value
  and wrong for a label, and reusing it made a sine plot's top tick read
  `9.974949866040545` — seventeen significant digits to say "about ten", in a gutter that
  then had to be that wide on **every row of the plot**. Three significant figures is the
  whole requirement: **the gutter went from 21 columns to 8.**

  Whole numbers stay whole (an axis from 0 to 10 says `10`, not `10.0`) and anything outside
  a readable range falls back to an exponent rather than growing the gutter without bound.

  Histogram bucket bounds are deliberately **unchanged**: `1.5–4` describes the data's own
  boundaries, which a reader may need exactly. That is a value, not an axis.

- **A multi-line record starts every value at one column.** The multi-line form is only
  reached when a record is too wide to inline, which is exactly the case where a reader
  scans *down* the values rather than reading across one pair — and `count: 7` above
  `median: 3.0` above `min: 1.5` started them at three different columns for no reason.
  `summary()` is the surface where this showed most, being the most-called introspection in
  the language.

  The pad is measured from the plain key, not the painted one: colouring may have wrapped it
  in escapes that occupy no columns, and padding by the painted width would misalign exactly
  when colour is on.

- **A bar carries its value at its own end, not at a shared right margin.** Right-aligning
  the numbers bought a comparison the chart already makes — length IS the comparison in a
  bar chart — and paid for it by stranding each number up to a bar-width from the bar it
  belongs to. On `[1, 5, 2, 8]` at a wide terminal the `1` sat about a hundred columns from
  its own bar, which is where the association between the two is lost. Adjacent, the numbers
  trace the bars' own profile, so the comparison survives *and* the association returns.

- **A denied capability names a grant that exists.** The refusal pointed at a single
  variable regardless of which effect was denied, so following it verbatim did not lift the
  denial. Each effect now names its own: `HELIX_ALLOW_FS=read`, `=write`, `HELIX_ALLOW_NET`,
  `HELIX_ALLOW_PROCESS`. The process hint additionally says what granting it means — a
  subprocess reaches whatever ITS permissions allow, including the filesystem and network
  you just declined (ADR 0037 D3).

- **`helix` with no arguments and no terminal refuses instead of hanging.** It started an
  interactive session and then waited forever on a pipe that would never carry a keystroke.
  It now exits 2 and names the likeliest cause — a binary built by `helix build`, which is
  stripped and cannot find its entry — while keeping the discovery map (`helix help`,
  `helix doc [Type]`, `helix describe <name>`) that made bare `helix` the place a new user
  lands.

## v0.8.0 — 2026-08-30

### Added

- **`html_escape(s)` — the escaper every server-rendering program was hand-rolling.** There
  was one inside the compiler for `to_html` and no way to reach it, so every program wrote
  its own — which is how a four-of-five escaper spreads.

  One implementation now backs both. It **scans before allocating**: text with nothing to
  escape is returned unchanged, sharing the original string, where the old form ran four
  sequential `replace` passes and allocated four times per cell regardless. And it escapes
  the fifth character (see Changed).

- **`DataFrame.tail(n)` and `DataFrame.slice(offset, len)` — the row window.** A frame had
  `head(n)` and nothing else, so a sorted frame could not be cut into chunks: reclustering a
  store means starting somewhere other than row 0.

  `tail` is **not** sugar over `slice` — expressing it as one needs the row count, which a
  lazy frame does not cheaply have, so `df.tail(5)` would silently materialize a frame you
  were careful to keep lazy.

  Both **clamp rather than refuse**: an offset past the end is an empty frame and a length
  past the end is short. Same rule as `read_at`, for the same reason — that is how a final
  partial chunk reads, and erroring would force the caller to compute the row count first. A
  NEGATIVE offset is refused, because that question already has an answer (`tail`).

  Note `.where(p).slice(0, n)` and `.slice(0, n).where(p)` are different questions — the
  first n matching rows, versus the matching rows among the first n. Both are pinned.

- **`dataframe(dict)` — a schema that is not syntax.** `dataframe()` took a RECORD, and
  record fields are source text, so a column could not be named at run time. A store whose
  chunk schema comes from the data could not build a frame at all.

  ```helix
  dataframe(names.reduce(dict(), (acc, n) => acc.insert(n, column_for(n))))
  ```

  **Column order differs from the record form, deliberately:** a record keeps the order
  written, a dict is sorted, so `dataframe(d)` yields columns in sorted name order. A Dict
  has no insertion order, so inventing one would be a claim about where the frame came from
  that nothing supports. Both are deterministic; `select` fixes an order that matters. A
  non-string key is refused by name rather than stringified, since `1` and `"1"` would
  otherwise merge into one column. See ADR 0043.

- **`Bytes` — a value that holds what a `String` cannot.** ADR 0041's storage substrate named
  its own ceiling: a Helix `Str` is UTF-8 by definition, so `read_at` had to refuse a slice
  that splits a character, and a packed integer, a bitmap, a compressed block or a hash
  digest had no representation at all.

  ```helix
  b = read_bytes_at("db/pages", 4096, 4096)   # a page `read_at` must refuse
  print(b.byte_at(0), b.to_hex(), b.length())
  b.write_at("db/pages", 8192)                # update one page in place
  ```

  **Widening `String` was never the alternative.** If a `Str` could hold arbitrary bytes,
  every string operation would have to answer "what if this is not text" — `upper`, `chars`,
  `split`, every regex verb — and somewhere the answer would be wrong, silently. Two types
  means each one's operations are total.

  The surface mirrors `String` where the operation means the same thing (`length`/`count`,
  `is_empty`, `take`/`drop`, `slice`, `concat`, `write_to`/`append_to`). Where they differ,
  they differ for a reason: `byte_at` is O(1) and answers an `Int` where `char_at` is O(i)
  and answers a string, and **`to_string()` can fail** — it refuses by name rather than
  substituting U+FFFD, which would silently change the data on the way out. In:
  `read_bytes`, `read_bytes_at`, `from_hex`, `from_base64`, `"…".to_bytes()`.

  **Ordered lexicographically by byte** — the order a key index is built on, and the same
  order `to_hex()` produces, so `a < b` and `a.to_hex() < b.to_hex()` always agree. `sort`,
  `min` and `max` accept them, because a type that compares with `<` but cannot be sorted is
  a split of the kind this project treats as a bug.

  **Prints as hex, in full, never truncated.** An elision would hide exactly the byte a
  reader is hunting, and printed output is a frozen format here — a `…` could not be removed
  later without a versioned event.

  Out of the first cut and refused BY NAME rather than silently accepted: a dict key (use
  `to_hex()`, which preserves the ordering), JSON (use `to_base64()` — base64 in JSON would
  not round-trip, since it would come back a `Str`), and DataFrame columns. See ADR 0042.

- **`lock_file` / `try_lock_file` — a lock the KERNEL holds, so it releases when its holder
  dies.** `create_new` is atomic and is the right answer for a content-addressed write, but
  as a lock it has the one flaw that matters: the file is still there after the holder
  crashes, and the next process cannot tell a live writer from a corpse. Every remedy at
  that level is a guess — a PID that may have been reused, a timestamp that may be a long
  pause, a heartbeat that is one more thing to get wrong.

  A kernel lock lives on an open file description, so it is released by `release()`, by the
  handle being dropped, by the process exiting, **and by SIGKILL**. Measured both ways in
  one test: after `kill -9`, the kernel lock is free and the lock file still reports busy.

  ```helix
  l = try_lock_file("db/.lock")        # `missing` if another process has it
  if l.is_missing() then ... else ...  # `== missing` PROPAGATES (ADR 0001) — use is_missing
  ```

  `try_lock_file` answering `missing` is an ANSWER, not a failure: a store that reports
  "another process has this open" beats one that hangs with no output. A `Lock` answers
  `release()` (idempotent, and says whether *this* call released it), `held()` and `path()`.

  **Advisory on every platform** — they exclude other lock takers, not other writers. Named
  here rather than discovered during a corruption.

- **A durable-storage substrate: `rename`, `fsync`, `sync_dir`, `create_new`, `file_size`,
  `read_at`, `write_at`, `truncate`, `remove_dir`.** A correct storage engine could not be
  written in Helix, and a field report building a versioned store established exactly why:
  the filesystem surface had no `rename`, so write-temp-then-rename was inexpressible, and
  no `fsync`, so "written" meant "reached the page cache" — which a power loss discards.

  These land as ONE set rather than the three that were blocking, because a partial
  durability story is worse than none: a program that calls `fsync` and skips `sync_dir`
  believes it committed and, after a crash, did not. A durable commit is exactly
  `write` → `fsync` → `rename` → `sync_dir`.

  ```helix
  "seq=1".write_to(tmp)
  fsync(tmp)              # the CONTENTS are on the device
  rename(tmp, head)       # the commit, atomic — no reader sees a half-written file
  sync_dir(dir)           # the NAME is on the device
  ```

  `rename` refuses to cross a filesystem rather than degrading to copy-then-delete, because
  that copy reintroduces the window it exists to remove. `create_new` is atomic where
  `file_exists` then `write_to` races, which makes it both a lock and a safe
  content-addressed write. `read_at`/`write_at` make access O(page) instead of O(file), so
  an index lookup no longer pays for the whole dataset. `remove_dir` is empty-only and
  never recursive — a recursive delete is one typo from removing a tree nobody named.

  **`sync_dir` answers `false` where the platform cannot flush a directory** (Windows
  exposes no way through the standard library) rather than `true`. A durability claim that
  cannot be kept is the shape of lie that loses data on exactly one platform, so the answer
  is testable instead.

  All nine are capability-gated, and `fs-read` does not imply `fs-write`. The honest limit:
  a Helix `Str` is UTF-8, so `read_at` refuses a slice that splits a character rather than
  substituting U+FFFD — this supports a text-structured store, and arbitrary binary needs a
  `Bytes` type the language does not yet have. See ADR 0041.

- **`[workspace]` in `helix.toml` — a directory can be a package without being the module
  root.** `helix.toml` was carrying two meanings at once: *this is a distributable package*
  (what `helix add <name> --path <dir>` consumes) and *in-project imports are anchored
  here*. Import resolution stops at the NEAREST manifest walking up, so a repo with a
  manifest per package made each package its own module root, and `import ui.parse`
  written inside `ui/` resolved to `ui/ui/parse.helix`. A manifest at the repo root did not
  help, because the nested one still won.

  ```toml
  [workspace]
  members = ["ui", "web", "nn"]
  ```

  The root anchors; the members stay packages. Nothing changes for a project that does not
  opt in, and a package a workspace does not list is untouched — one vendored inside an
  unrelated workspace is not that workspace's business. Two refusals rather than silence: a
  member that does not exist is named (left quiet it would self-anchor that package, which
  is the failure this ends), and a member's own `[dependencies]` is refused rather than
  resolved against a manifest that is no longer the root.

  Reported from the field with a three-way measured table, after a fix from this side that
  did not work. See ADR 0040.

- **`helix check --lint` examines everything the entry point imports.** It read only the
  file it was handed, so in any project with a library — which is every project with a
  library — `helix check --lint app.helix` printed `ok` and said nothing about the code the
  app is mostly made of. A field report measured what that cost: an O(n^2) accumulation
  lived in an imported training loop for a whole release cycle, found only by copying the
  tree and linting the copy file by file. The loader already holds every module's source —
  it must, to render an error against the right file — so the traversal was there for the
  taking. Notes name the module they came from, and a module reached twice is reported
  once.

### Changed

- **`to_html` now escapes the apostrophe.** The internal escaper handled `&`, `<`, `>` and
  `"` and left `'` alone — and inside a single-quoted attribute (`<a title='...'>`) an
  unescaped apostrophe closes the attribute, so everything after it is markup. Output
  containing `'` changes: it is now `&#39;`. If you diff generated HTML, that is the
  difference.

  `&#39;` rather than `&apos;` because the named form is XML and HTML5 only and is undefined
  in HTML4.

### Fixed

- **A fold that accumulates an ARRAY in a record field is no longer quadratic.** ADR 0029
  makes amortized-linear accumulation a language guarantee, and this was the shape it did
  not reach — the one `mut` being top-level only *forces*, since a fold carrying two values
  must carry them in a record. Measured on a release build with startup subtracted, at
  n=160,000: **2591 ms → 36 ms**, and the class changed from quadratic (27.9x per 4x the
  input) to linear (3.3x).

  `Op::ConcatIntoLocal` makes a bare accumulator linear by taking the value out of its
  local slot, which leaves the `Rc` unique so the append extends in place. Through a record
  field there is no slot: reading `acc.xs` clones the `Rc` while the record still holds one,
  so every step copied the whole accumulator. `concat` on a shared receiver now returns a
  view of an APPEND-ONLY buffer instead. Sharing is safe by construction rather than by
  analysis — the buffer only grows and each value freezes its own length, so a value reads
  only a prefix that is already settled and two values cannot observe each other. Appending
  is O(1) for the newest view; extending an older one copies, which is O(n) exactly when the
  program really did branch.

  It is a property of `concat`, not a recognised syntax, so `a.b.xs.concat(e)`,
  `{...a, xs: a.xs.concat(e)}` and `step(a, i).concat(e)` are all linear too.

  **Still quadratic, and now named precisely:** a DICT in a record field (16.6x per 4x,
  **71 s** at n=128,000 — `insert` clones the whole map per step) and a string built by
  interpolation in a record field (10.1x). `helix check --lint` was narrowed to the dict
  case rather than deleted; keeping the array note would be a checker contradicting the
  runtime, which is how a checker gets ignored.

- **A failed import now says where imports are anchored, and what chose that anchor.**
  `cannot find module `ui.parse`` with *expected … under the project root* was true and
  unusable: the root is the entire answer and nothing printed it. It now names the
  directory and whether a `helix.toml` set it or the entry file's own directory did, and
  calls out the doubled-segment case by name — `ui` being both the first segment and the
  root's own name is a precise signal, not something to leave to deduction. This replaces a
  three-experiment investigation with one command.

- **`sum`, `mean`, `var` and `std` now say they use compensated summation.** The Float
  paths are Kahan-Babuska-Neumaier, which makes them a DIFFERENT operation from adding the
  elements left to right rather than merely a faster one — and that was documented nowhere,
  in a language whose point is numerical work. The consequence is not theoretical: a field
  report replaced a collected array plus `.mean()` with a hand-rolled running sum to make an
  accumulation linear, and concluded it was bit-identical. Measured across six realistic
  training-loss shapes, five differ in the last bits, including a 313-step decaying loss.
  `sum() / count()` IS `mean()` exactly; `reduce(0.0, (a, x) => a + x)` is the naive sum and
  is not.

- **An unknown key in `helix.toml` now names the running build's version.** Unknown keys are
  refused rather than ignored — a silently discarded section looks like it took effect,
  which is how a `[capabilities]` block once appeared to restrict authority and did
  nothing. But *unknown field* reads as "your manifest is malformed" when the cause is
  often "your manifest is newer than your binary", and there was no way to tell those
  apart. `[workspace]` is the first key with that problem: it is refused by 0.7.0.

## v0.7.0 — 2026-08-28

### Added

- **Regular expressions on String** — `re_match`, `re_find`, `re_find_all`, `re_replace`,
  `re_captures`, `re_split`.

  **A regex cannot hang your program, and that is the reason for the engine rather than a
  side effect of it.** The engine is finite-automata based, so matching is linear in the
  input and no pattern/input pair blows up. The classic catastrophic-backtracking case
  finishes here in under a millisecond and hangs Python. ADR 0024 says user input must
  never abort the host; a backtracking engine would contradict that exactly where it
  matters most, for a language that serves HTTP. The price is backreferences and
  lookaround — which are what make backtracking necessary — and the error says so.

  The names say `re_` because `contains`, `replace`, `split` and `index_of` already exist
  and take LITERAL text. Whether `.` means "any character" or "a dot" should not be
  something a reader infers from which overload they picked.

  One trap this surfaced is caught rather than documented: Helix strings interpolate
  `{...}`, so `"([0-9]{4})"` silently becomes `([0-9]4)`. `helix check` refuses it and
  names raw strings as the fix.

- **`helix search <words>` — find a capability by what it DOES**, not by a name you
  already have. It searches names, signatures, docs and notes, plus the LANGUAGE FORMS and
  the ENVIRONMENT, because neither has a name to look up.

  Every word must match, so saying more narrows rather than empties; a term matches at a
  word boundary, so `raw` no longer answers with `d`raw`n at random`; and a small synonym
  table maps the word a reader arrives with to the word the catalog uses — `regex` finds
  entries that say "regular expression", and the listing says out loud when it widened a
  query. `helix describe match` and `helix describe HELIX_CAP` now answer too.

- **The environment is documented and discoverable** — fifteen `HELIX_*` variables in
  `helix search`, `helix describe` and the reference, including the CAPABILITY SANDBOX.

  A field report established the sandbox is complete and enforcing, and that the only way
  to discover it was to grep the compiler: `helix search sandbox` answered nothing. A
  security feature nobody can find is one nobody uses. A test walks `src/` for every
  `HELIX_*` the source reads and fails unless each is documented or declared internal with
  a reason, so the catalog is complete by construction rather than by diligence.

- **`HELIX_ALLOW_PROCESS=on|all`** grants subprocess authority under the sandbox. Until
  now `process` was hardcoded ungrantable, so `run` was denied under every active mode
  with no way to allow it — turning the sandbox on broke every program that shells out.

- **A server can close a connection**: `conn.close()` now works on an accepted
  connection, not only on an `http_stream`. See Fixed for why this was urgent.

- **A request knows who sent it and which HTTP it is** — `request()` carries `peer` and
  `version` alongside `method`, `path`, `query`, `headers` and `body`.

  `peer` is `{address, port}`, a record rather than `"1.2.3.4:5678"` so rate limiting can
  group by address without re-parsing — which is where the naive split meets IPv6. It is
  called `peer` and not `client_ip` because behind a reverse proxy it IS the proxy, and
  `X-Forwarded-For` means something only when the proxy is one you run. Both were present
  in the socket and discarded; the address was literally bound to `_peer`.

  `version` is `"1.0"`/`"1.1"`/`"2.0"`, and an unrecognisable request line answers `"1.0"`
  deliberately: 1.0 means close unless asked otherwise, so the guess cannot leak a
  connection.

- **A response body can be sent in pieces** — `conn.stream(response)`, then `send` for
  each piece, then `close`.

  ```helix
  c.stream({status: 200, html: shell})
  c.send(slow_part)
  c.close()
  ```

  A document slow to produce could not be flushed as it was produced: the first byte
  waited for the last. `send` frames the next piece by what you opened — an SSE event
  after `sse()`, a body chunk after `stream()` — so there is one write verb rather than
  two differing only in framing. The framing follows the CLIENT: HTTP/1.1 gets
  `Transfer-Encoding: chunked`, HTTP/1.0 gets `Connection: close` and raw bytes, which is
  the only correct answer for 1.0 and is decidable only because the request now carries
  `version`. `close` writes the terminating chunk — without it a client waits for an end
  that never comes, which presents as a hang rather than as truncation.

- **`helix check --lint` names a quadratic accumulation.** ADR 0029 guarantees
  amortized-linear accumulation, and the guarantee stops at a record field: measured over
  8x the input, a bare accumulator is 3.8x and `reduce({xs: [], k: 0}, …)` is 22.8x. That
  shape is not niche — `mut` is top-level only, so a fold carrying two values carries them
  in a record, which is what AGENTS.md teaches. The lint states the class and the measured
  ratios; ADR 0026 says a performance cliff is a diagnostic or a fix and never silence.

- **A DataFrame query can ask String questions** (ADR 0039): `starts_with`, `ends_with`,
  `contains` — all literal text — and `re_match` for a regular expression.

  ```helix
  df.where(@gene.re_match("""^BRCA"""))
  df.with({hit: @tissue.starts_with("b")})
  ```

  For a language whose ground is VCF, FASTQ, GFF and CSV, this is the first query anyone
  types, and until now every one of them answered *"this expression isn't supported inside
  a DataFrame query yet"* — while pointing at line 1 of the program, because that arm alone
  hardcoded its position.

  It was never *impossible*: `to_json().parse_json()` -> filter -> `to_dataframe()` closes
  the loop. It cost **2,174 ms** on 200k rows where the same regex over one column costs
  **57 ms**, the difference being a whole frame serialized to JSON text and rebuilt.

  Both backends evaluate these through **Helix's own scalar kernel**, not polars' string
  namespace — polars' `contains` is a regex where Helix's is literal text, and its
  non-strict form answers an all-null column for a bad pattern instead of raising. One
  probe of that kernel, before any row, settles arity, argument type, an invalid pattern
  and the no-regex build, identically on both sides and with no row number, because a type
  error is not a cell error.

  `missing` propagates, as everywhere: `missing.starts_with("h")` is `missing`, not
  `false`. That rule is only *visible* under `with` — `where` drops the row either way —
  which is exactly why a corpus program pins the column and not just the count.

  The pattern must be constant for the query (a literal or a variable, not another
  column), which is what makes "compiled once per query, never per row" a property of the
  shape rather than a promise.

- **Helix can read a SQLite database, and a query is a DataFrame** (ADR 0038, Stage 1;
  `--features db`).

  ```helix
  users = sqlite_query("app.db", "select name, age from users where age > ?", [30])
  print(users.where(@age > 40).group(@city).mean(@age))
  ```

  Rows-of-records would land *next to* the frame surface instead of joining it; a frame
  plugs into `where`/`group`/`sort`/`join`/`write_csv` — everything a `read_csv` result
  can do. It is built through the ADR 0012 seam, so it works on the native backend as
  well as polars. SQL `NULL` becomes `missing`, and a column's type is discovered from
  its rows (SQLite types values, not columns), widening Int → Float → Str.

  **Parameters bind as values, and there is no string-building form** — the same call
  ADR 0037 made for subprocesses. A parameter carrying `x' or 1=1 --` matches a user
  literally so named, which is to say nothing.

  **It opens READ-ONLY, which is what makes the `fs-read` capability label true** rather
  than convenient: `delete from users` is refused by SQLite itself. A typo in the path
  also fails instead of silently creating an empty database. Writing gets its own verb
  and its own `fs-write` label (Stage 2).

  Feature-gated with the *body* gated, not the name: without `--features db` the builtin
  still exists, type-checks and describes itself, and running it says what to rebuild
  with. Measured cost on the appliance profile: **+1.9 MB (15%)**, from bundled SQLite C
  source — so the binary keeps its no-system-dependency property.
- **A Helix program can be a tool: `fn main` IS the command line** (ADR 0037 D1).
  `helix run tool.helix --threads 8` used to run the program and **discard the arguments
  in silence** — the worst of the three possible behaviours, because the command looks
  like it worked.

  The binding rule is not new. It is the rule Helix already uses at a call site, so
  `tool 10 3`, `tool --a 10 --b 3` and `tool --b 3 --a 10` agree exactly as `go(10, 3)`,
  `go(a: 10, b: 3)` and `go(b: 3, a: 10)` do. Every parameter is nameable or positional,
  a parameter with a default may be omitted, and a `Bool` defaulting to `false` also takes
  the bare `--verbose`. `--name=value` works too.

  `--help` is generated from the declaration and the `##` doc comment above `main`, and is
  answered **without running the program** — a script's top level is its program, so
  running it to print help would run the tool.

  Every refusal names the thing: a missing required parameter, an unknown option, and a
  bad conversion (`--threads eight` → *"`--threads` expects an Int, but got `eight`"*). A
  program that declares no `main` now **refuses** arguments instead of ignoring them.
  `helix check` refuses a `main` whose parameter cannot come from a string (`Array`,
  `Tensor`, `DataFrame`, `Dna`) before it ever runs.

  It is implemented as a **desugar**: argv becomes literal expressions and a `main(…)`
  call is appended to the program, so the type checker validates the call like any other
  and all three engines run identical code — no new evaluator path to keep in agreement.
- **`assert_error(try expr, "substring"?)`** — assert that something FAILED, and that
  it said why. The idiom it replaces (`r = try f()` then
  `assert(r.error.contains("…"))`) checks the right thing but, on failure, prints
  `assertion failed` and nothing else — not the message it got, not the value it got
  instead. This shows both:

  ```
  assertion failed: expected an error containing `overflow`, but it said: division by zero
  assertion failed: expected an error, but it succeeded with 2
  ```

  In a language that pins error text as part of its contract, the message is the thing
  under test, so an assertion about it has to show it. Takes the record `try` already
  produces rather than a callback, and counts as an assertion (so a file whose only
  check is `assert_error` does not trip the runner's "asserted nothing" rule).
- **`helix test --engines` — every test becomes a differential test.** After a test file
  passes, it is re-run under the bytecode VM and the tree-walker, and any difference in
  exit status, stdout or stderr fails the run.

  No other test runner can offer this, because no other language ships three
  implementations of itself that must agree byte-for-byte. `pytest`, `jest` and
  `cargo test` can each tell you a test passed; none can tell you it passes *the same
  way* under three independent evaluators. That agreement is Helix's entire correctness
  story — and until now only the compiler's own suite could reach it, while a user's
  tests ran on one engine. That is the same shape ADR 0036 spent a release paying for on
  the DataFrame backends: an axis the tests could not see.

  It also catches a **non-deterministic test** for free, because a test that is not a
  pure function of its input disagrees with itself across runs. The report says so
  rather than blaming the engines: it cannot tell the two causes apart, so it names both
  and tells you how to (run it twice on one engine).

  Opt-in: it costs two extra child processes per file (measured 7 ms → 37 ms for a
  one-file suite), which is cheap in CI and not worth paying on every local save. Each
  engine runs in a CHILD process — three in-process runs would share the JIT, the memo
  tables and the module line map, and a differential oracle that contaminates its own
  control column proves nothing.
- **`helix jit-explain <script>`** — which numeric kernel sites the compiler offered the
  JIT, where they are, and which got native code. `--json` for tools.

  `AGENTS.md` has listed silent JIT fallback as a footgun since the JIT shipped, with an
  eligibility diagnostic recorded as *planned*: the answer stays correct and the program
  gets much slower, so the only symptom is a wall-clock number with nothing to compare it
  against. Now a reader can ask.

  It is careful about what it blames. Three states are kept apart — the JIT switched off
  (`HELIX_NOJIT=1` or no feature), the JIT enabled but with no codegen for this target
  (x86-64 Linux only today, which covers neither aarch64 release build nor macOS), and a
  genuine per-site refusal. Only the last prints `DECLINED`; reporting it for the other
  two would tell every Apple-Silicon reader their loops are shaped wrong. It compiles but
  does **not** run the program, so asking the question never executes anything.

  Two families are reported, because the JIT has two: *kernel sites* (a `map`/`filter`/
  `reduce`/`scan` body, reached through a `TryJit*` op) and *whole functions* entered by
  name, which is how a tail-recursive numeric function becomes a native loop. A sweep of
  the tracked tree found **24 compiled functions** in that second family, including
  `bench/kernels/k2_mandelbrot.helix`, whose native code is three functions and zero
  kernel sites.

  It does not yet say *why* a shape was refused: the eligibility predicates in
  `jit::analysis` answer `bool`, not a reason, and a plausible-sounding guess would send
  the reader to rewrite the wrong thing.
- **`helix check --json`** — diagnostics as data: `{ok, checked, failed, files: [{file,
  ok, diagnostics: [{severity, file, line, col, message, hint, rendered}]}]}`. Tools
  were scraping caret-annotated text with regexes to recover a line number.

  It keeps `rendered` — byte-identical to the human output — rather than replacing prose
  with an error code. That is deliberate and measured: putting 14 mistakes an agent
  plausibly makes through `helix check` found **eleven whose help text names the exact
  fix** (`to_json(x)` answers *"`to_json` is a method: `x.to_json()`"*; a C-style body
  answers *"`fn f(x) = x + 1`"*), so the prose is the part that repairs the mistake and
  a machine format that dropped it would be a downgrade.

  The load path is what actually needed fixing: parse errors — the most common failure —
  were rendered to a `String` inside the module loader, so no caller could recover a line,
  a column or a hint. `module::Diag` now carries both halves, and `module::load` is a
  thin wrapper over it, so every existing caller is unchanged. `--lint` notes come
  through as `severity: "note"` and, as in the human output, change neither `ok` nor the
  exit code.
- **`helix describe <Type>`** — one receiver type's whole method table as JSON: per
  method the signature, doc, example, expected output, notes and capability effect, plus
  the universal methods. DataFrame's entry is ~6% of the size of the full catalog, so the
  question you have *before* you know any names is answerable without reading 120 KB.
  `helix doc <Type>` prints the same table for a human.

### Changed

- **A build between releases no longer claims to BE the last release.** The tree now
  carries a marker naming the release it is working toward, so `helix --version` on a
  `main` build reads `0.7.1-dev` rather than the version that shipped (`scripts/post-release.sh`, ritual
  step 7). The next release is a minor by the policy in `docs/RELEASING.md` — this
  section carries a `### Changed` entry — so the marker names `0.7.0`.

- **The toolchain floor accepts and orders a `-dev` marker.**

  `scripts/release.sh` bumped the version at release time and nothing moved it again, so
  between releases a build from `main` reported the version it had just SHIPPED. A field
  report found the consequence precisely: `now()` landed eight commits after the v0.6.0
  tag, both the released binary and a main build reported `helix 0.6.0`, and a project
  needing `now()` could not say so. `helix = ">=0.6.0"` is satisfied by the very binary
  that lacks it — so the user met "`now` is not a known function" at run time instead of
  the one clear sentence the manifest check exists to give.

  `0.6.0 < 0.6.1-dev < 0.6.1`, so a manifest can now say `">=0.6.1-dev"` and mean "newer
  than the 0.6.0 release". The rank is a fourth component rather than a stripped suffix:
  stripping alone would make a dev build compare EQUAL to the release it has not become,
  and so claim to satisfy `>=0.6.1`.

  `-dev` is the only pre-release spelling accepted. `-rc1` and `-alpha.2` order against
  each other by convention alone, and a version that cannot be compared is not a version.

  **What it does not buy**, stated plainly: it is a monotone counter, not a feature probe.
  Every commit in a release window reports the same string, so the floor says "newer than
  the last release", never "has `now`". For an addition, `helix describe now` is the
  precise instrument and already exits non-zero for a name this build lacks. The marker
  earns its keep on what nothing can probe — a semantics change, which
  `tests/compat/MIGRATIONS.md` records this project already shipping once.

  **Transitional wart**: an older binary meeting `">=0.6.1-dev"` complains that it "must
  be a minimum version" — a syntax error — rather than "your binary is too old". That is
  why the parser ships one release ahead of the first marker.

### Fixed

- **SECURITY: three requests could kill a server.** `curl --http1.0 -H 'Connection: close'`
  — no auth, no body, no volume, using a header any HTTP/1.0 client sends by default.

  `Net` had fifteen methods and the only `close()` was the outbound client's, so a server
  had no way to hang up at all. An accept loop calling `close()` on an accepted connection
  — the obvious spelling, and the only one — was refused, the raise unwound the loop, and
  the process died. On a sharded server it was worse than a crash: shards died one at a
  time while the server kept answering, so it read as healthy until it was not.

  Found by a field report typing one curl on a hunch. The regression test binds a real
  socket and sends the exact attack over TCP, because no gate that never opens a socket
  can execute an accept loop.

- **SECURITY: the capability sandbox failed open on a typo.** `HELIX_CAP` fell into a
  catch-all meaning `off`, so `HELIX_CAP=enfroce` in a Dockerfile or a systemd unit
  silently ran the program fully authorised. A control that fails open on a misspelling is
  bad; one that does it quietly is worse, because the program works and nothing prompts a
  second look. An unrecognised mode is now refused with exit 2. `HELIX_CAP=` stays `off`,
  because that is how a shell unsets an inherited variable.

- **A capability grant that did not parse was silently denied.** Silent is the problem,
  not the denial: ADR 0021 describes net authority as a host:port allowlist, which is the
  eventual design and not what phase 1 parses — so a reader following it writes
  `HELIX_ALLOW_NET=example.com:443`, believes they granted access, and meets "capability
  denied" from a program they authorised with nothing pointing at the variable. `./data`,
  `rw` and `yes` were the same trap. All refused at startup now.

- **`\"` inside an interpolation hole.** A hole admits two spellings of the same nested
  string and `\"` meant opposite things in them, so `"x{"a\"b"}y"` closed the string at
  the escape, re-opened it at the real close, and ran to end of input as *"unterminated
  `{` interpolation"*. A nested string now owns its own escapes; both spellings work.

- **A malformed version in Helix's own `Cargo.toml` could abort a user's program.** The
  toolchain-floor check carried `parse_semver(...).expect(...)` on the hot path, and under
  `panic = "abort"` that meant an invariant about THIS crate's build was asserted at run
  time inside every user's binary — on every `helix run` in a project declaring a floor.
  ADR 0024 says a total runtime never aborts the host. The invariant moved to the gate
  (`the_crate_version_is_a_version`); an unparseable own-version now simply leaves the
  floor unenforced instead of stopping everything.

- **`helix new` has a test, and it immediately caught a bug in this work.** It had none
  anywhere. It writes a manifest declaring a toolchain floor, and the assertion is the
  round trip: the binary that wrote it must be able to open it.

  A first draft derived a LOWER floor from the marker so older binaries could read the
  manifest. On a `0.7.0-dev` tree that computed `0.7.0` — a version that has not shipped
  and that the writing binary does not satisfy — because it assumed a marker is always a
  patch marker. **Deriving was the error, not the arithmetic.** A project scaffolded by a
  0.7.0-dev binary that declares `>=0.6.0` invites a 0.6.0 binary to open it and fail
  later on whatever the author writes: the silent wrong answer this project treats as its
  worst failure. The scaffold now names the version that wrote it. The cost is that a
  pre-marker binary reports that floor as a syntax complaint rather than "your binary is
  too old" — loud and imprecise beats quiet and wrong.

- **`helix jit-explain` reports `file:line`, not a position in nothing.** The line a
  kernel site carries is a position in the MERGED module space — every imported file
  concatenated — so on a multi-module program it named no file the reader has open. A
  field report measured it: `app.helix` is 298 lines and the tool reported compiled sites
  at 1539, 2179 and 2345, real positions in a 2,443-line merged program of `app` + `ui/`
  + `web/`. For a tool whose stated job is "which kernels compiled, **and where**", that
  was the job half done. Sites now read `web/limit.helix:67`, and `--json` keeps the
  merged position alongside so downstream correlation still works. Single-file programs
  keep the bare `line:col` — the file is the argument you just typed.

- **`scripts/release.sh` survives a marker.** Measured before the fix: `patch` died with
  `bash: dev: unbound variable`, and `minor` was correct only by accident (its branch
  never reads the patch component, so the marker was silently discarded). `0.6.1-dev` +
  `patch` is now `0.6.1` — stripping the marker IS the bump; incrementing as well would
  skip a version.

- **A tag that disagrees with `Cargo.toml` is refused before anything is published.**
  `release.yml` never read the version, and its per-platform smoke steps run
  `helix version` while asserting nothing, so a forgotten marker would have published six
  green assets all reporting `0.6.1-dev`. The only check that would have noticed runs at
  ritual step 6 — after the publish.

- **An unsupported expression in a DataFrame query now points at itself.** The refusal
  hardcoded position `0, 0`, alone among its sibling arms, so the error a reader is most
  likely to meet in a query underlined line 1 — in the one place they most need to be told
  where. It also now names the String tests among what a query supports.


- **`helix test` no longer wanders into build output.** In this repository it collected
  four *failing* `*_test.helix` files out of `target/` — scratch from earlier builds —
  and reported them among the results, so the runner's own output was untrustworthy in
  the project that ships it. Any tree holding a `node_modules` or `__pycache__` had the
  same shape.

  Only unambiguously machine-generated names are skipped (`target`, `node_modules`,
  `__pycache__`); `dist`, `build` and `venv` are deliberately **not**, because each is
  plausibly somebody's own directory. The asymmetry is the reason for the short list:
  running an extra test is visible noise, while hiding a real one is silence — the exact
  failure this project already paid for once. So the skip is **reported** ("did not
  descend into …, name one explicitly to run tests inside it"), and naming a directory
  explicitly still runs it, because the check applies when *descending*, never to the
  root you asked for.

- **A Float now survives `to_json` → `parse_json` bit-identically.** It did not:
  `(-0.21453773034276893).to_json().parse_json()` answered `-0.2145377303427689` —
  one ULP away, and `!=` its own source literal. A model checkpoint written and read
  back was not the model that was saved.

  Neither half of the round trip was where the fault looked like it was.
  Serialization was always correct (`to_json` emits the shortest round-trip
  spelling), and Helix's own parser was always correct (`to_float` on that same
  17-digit string is exact). The loss was on the JSON *read* path: `serde_json` was
  declared without its `float_roundtrip` feature, so it parsed with a fast
  best-effort algorithm that is permitted to land a bit away. `0.1`, `0.2`, `0.3`,
  `pi` and `e` all round-tripped fine, which is why a hand-picked battery of "hard"
  values would have reported success — `src/json.rs` now proves the property over
  **20,000 random f64 bit patterns**, including subnormals and the extremes, and
  `tests/cli.rs` proves it survives the language surface on all three engines.

  Measured cost of the exact parser: **1.04×** on 300,000 floats (5 MB of JSON),
  26 ms → 27 ms, min-of-7.

## v0.6.0 — 2026-08-25

### Changed — one semantics: frames, arrays and scalars answer the same question (ADR 0036)

**This release changes answers.** ADR 0034 stated the doctrine — *a column expression
means exactly what the same expression means on scalars* — and then recorded three
deltas against the polars backend and deferred closing them. A release sweep found
**fifteen**, five of which were recorded nowhere — and running these notes against the
built artifact found a **sixteenth**. [ADR
0036](docs/adr/0036-one-semantics.md) closes all of them and replaces the delta list
with a rule: a divergence is a bug in whichever side disagrees with the language.

Every change below was verified on both DataFrame backends and all three engines.
`scripts/dfdiff.sh` runs every tracked program under both engines and reports **0
undeclared divergences**.

**Arithmetic in a frame now matches arithmetic on scalars.** On the polars backend
(the default):

| expression | was | now |
|---|---|---|
| `@a / 10` on `41` | `4` (integer division) | `4.1` |
| `@b / 10` on `[41, 38]` | `[4.1000000000000005, …]` | `[4.1, 3.8]` |
| `@a % -3` on `7` | `-2` (floored) | `1` (euclidean) |
| `@a // 2` | refused inside a query | `-4` (euclidean) |
| Int `@a / 0` | `missing` | error naming the row |
| Float `@a / 0.0` | `inf` | error naming the row |
| `0.0 / 0.0` | `NaN` | error naming the row |
| `@s + "y"` on a String column | `"xy"` | error, as on scalars |

The second row is the subtlest and affected every division by a constant in every
query: polars rewrites division-by-a-constant into multiplication by the reciprocal,
and `41.0 * 0.1` is not `41.0 / 10.0`. It only triggers at two rows or more, so a
one-row test reports agreement.

The **last** row is the one that got away and is worth knowing about. `+` `-` `*` `**`
lowered straight to polars' own operators — and polars' `+` on two `str` columns
concatenates — so `@s + "y"` answered `"xy"` with exit 0 while `"x" + "y"` was refused
on scalars and inside `map`. Every gate in the repo was green over it, for one reason:
no tracked program adds to a String column, so no differential run ever evaluated the
expression. It was caught by running this table against the release binary, which is
now part of the release ritual.

**Two of these change WHICH ROWS a query returns**, not how a number prints:
`where(@x / @y == 2)` on `x=[4,5], y=[2,2]` was 2 rows and is now 1; `where(@x / @y > 0)`
over a zero divisor was a silently shorter frame and is now an error.

**`.where(a).where(b)` is sequential** — the frame you filtered is the frame you filter
again. polars fused adjacent filters and evaluated both over every row, which was
invisible until a predicate could raise: `.where(not is_nan(@v)).where(@v > 2.0)` raised
on rows the first filter had already removed, so the guard the error tells you to write
did not work. Measured cost of the fix: 1.00x on a limited query, 1.04x on a streaming
write, 1.09x on a full scan.

**A NaN is a failed computation, not absent data**, and nothing converts one into the
other now:

| expression | was | now |
|---|---|---|
| `[1.0, nan, 3.0].max()` | `missing` | `NaN` |
| `[1.0, nan, 3.0].spread()` | **`2.0`** — a wrong number | `NaN` |
| `group(@g).max(@v)` over a NaN | `1.0` (skipped) | `NaN` |
| `[1.0, nan, 3.0].argmin()` | `missing` | `1` (the NaN's index) |
| `nan > 2.0`, `where(@v > 2.0)` | `true`, row kept | error + the guard to write |
| `[3.0, sqrt(-1.0), 1.0].sort()` | `[NaN, 1.0, 3.0]` | `[1.0, 3.0, NaN]` |
| `[nan, nan].unique()` | `[NaN, NaN]` | `[NaN]` |
| `1.0 % 0.0` | `NaN` | error |

`spread` was the worst: not missing, not NaN, but a plausible and confidently wrong
number in the stats surface, because it folded with Rust's `f64::min`/`f64::max` —
IEEE-754-2008 `minNum`, which ignores a NaN by design and was **removed** in 754-2019.

**Sorting places every NaN last, sign-independently.** The old rule ordered by sign bit,
which is unobservable from Helix and put the same printed value at both ends of one
sorted array: `[3.0, sqrt(-1.0), abs(sqrt(-1.0)), 1.0].sort()` was
`[NaN, 1.0, 3.0, NaN]`. `-0.0 < 0.0` is kept and now holds in a frame too.

**`==` stays IEEE** — `nan == nan` is `false` at every depth. Keys are a *separate*
relation in which all NaNs are one identity, which is what makes `unique` idempotent
and a hash join implementable. Both relations are now named and cannot leak into each
other. This knowingly reverses one clause of ADR 0001's 2026-07-17 amendment; the
argument is that the clause was never implemented — arrays obeyed it, frames did not.

### Added

- **`nan`** joins `pi`/`e`/`inf` as a literal. If your program binds `nan` as a
  variable name, rename it (`mut nan = …` still shadows, as for any constant).
- **`.drop_nan()`** on arrays and DataFrames — the single visible opt-out from NaN
  propagation, parallel to `.drop_missing()`. `xs.drop_nan().max()` is the `nanmax`
  spelling. The two verbs remove different things: neither touches the other's value.
- **`is_nan()` and `is_finite()` work inside a DataFrame query**, in both the
  free-function and method spellings. Until now they were a parse-time refusal on a
  column, which made the runtime's own advice — "guard it first with `is_nan(x)`" —
  impossible to follow where it was most needed.
- **`HELIX_DF_ENGINE` is validated.** Naming an engine the build does not have was
  silently ignored, so `HELIX_DF_ENGINE=native` on a released binary gave you polars
  with no diagnostic — and the two disagreed. It now refuses by name and says what the
  build contains.

### Testing — the gates that were not gating

- **The dual-engine DataFrame campaign now runs.** 28 tests in
  `src/backend/native/tests.rs`, including every `mod against_the_oracle` comparison
  against the polars oracle, executed in **no gate at all**: `native-df` is not a
  default feature, `scripts/gate.sh` ran a bare `cargo test`, and CI's only `native-df`
  step was a `clippy` without `--all-targets`, so the test targets were never compiled.
  They were written, reviewed, committed, and run by nothing — while `docs/testing.md`
  told readers they ran through the gate. The gate now runs them in their own target
  directory; CI runs the full `native-df` suite on a compile it was already paying for.
- **Version-compatibility baselines** (`tests/compat/`): what a released version
  actually computed — exit, stdout, stderr for 119 deterministic programs — captured by
  `scripts/capture-compat.sh` and **never rewritten**. Every other gate in this repo
  compares the tree against itself and so proves only consistency; this is the only one
  that can answer "does the program I wrote six months ago still compute the same
  number?". There is deliberately no environment variable that blesses a drift: an
  intentional change is recorded in `tests/compat/MIGRATIONS.md` with its reason, and
  that file accumulates into a checkable list of every user-visible behavior change.
  This matters now specifically — ADR 0033 Stage 4, ADR 0034's arithmetic deltas, the
  row-order-to-Neumaier switch, and the 0.6.0 polars tightenings are all queued to
  change printed numbers, and nothing recorded what v0.5 did.

## v0.5.1 — 2026-08-24

### Fixed

- **Int↔Float comparison is exact above 2^53** on every engine and every path
  (`==`, ordering, `min`/`max`, `sort`, `unique`, `frequencies`): the widening
  collapse (`i64 as f64`) made `[2^53+1, (2^53).0].max()` answer the strictly
  smaller number depending on order, and called two different numbers equal —
  equality is transitive again (`interp::ops::int_float_cmp`).
- **`helix check` catches guaranteed rebind errors**: `x = 1` then `x = 2`,
  `pi = 3`, and `mut f = ...` over a reached `fn` all checked "ok" and died at
  run time — the checker now refuses them with the runtime's exact wording,
  while every legal shadowing idiom (mut rebind chains, re-declaring `mut`,
  `mut` before a same-named `fn`, duplicate `fn`, `mut e = ...`) stays legal.
- **`helix check` catches method arity** for String/DNA/Array/Record receivers,
  fed by the same signature strings `helix doc` prints (`"ATG".upper(1, 2)`
  checked "ok" and died at run time).
- **Module files can no longer shadow the seeded constants**: `export pi = 3`
  (or `fn pi()`) in a module silently rebound pi module-wide, where the same
  line in a single-file program refuses; the loader now refuses identically
  (`mut pi = ...` remains an explicit, legal shadow).
- **`import m.{f}` keeps f's parameter defaults** — the selective spelling
  silently dropped what the qualified call kept, so `greet("ada")` was an arity
  error. Named arguments on a selective import now get an honest error naming
  the situation instead of calling `f` a builtin.
- **Native frame fixes**, pinned by dual-backend tests: CSV round-trips the
  empty string (`""` is a string, a bare empty field is missing — both
  directions were lossy); integer-looking CSV fields too big for i64 stay text
  instead of rounding through 1e20; `-0.0`/`0.0` collapse as one group/join/
  unique key; a join whose key dtypes differ refuses instead of answering an
  empty frame; `group(...).min/max` on a String column answers lexically.
- **`respond` validates `status`**: `status: 9999` wrote a protocol-invalid
  wire line and a non-Int status silently became an empty 200, discarding the
  payload — both now refuse with `status must be an integer between 100 and
  599`. A present-but-wrong `jar:` on `http_request` is a teaching error
  instead of a silent cookieless request. Duplicate `df.select(@a, @a)` refuses
  in Helix's own words on the polars backend too (its error recommended
  `.alias(...)`, an API Helix does not have).
- **`[].norm()` is `+0.0`** (Rust's empty float sum is `-0.0`, and
  `sqrt(-0.0)` is `-0.0` — a negative empty-vector norm).
- **Message and doc polish**: `parse_cookies` docs say what it returns (a Dict,
  last value wins); the `to_dataframe([])` hint no longer recommends a refused
  idiom; record field-list hints print in canonical sorted order; the static
  slice help names tensors; `helix test --json` emits a JSON document even for
  a missing path; multi-line doc-test failures keep their indent.

## v0.5.0 — 2026-08-23

### Performance — the native engine wins its own matrix (ADR 0033 Stages 2–3)

- **Native is faster than the polars backend on all 16 verbs** of the 5M-row
  matrix on the dev box (min-of-3, every result cell-compared against the polars
  backend). Ratios are native's time as a fraction of the polars backend's:
  filters ~0.01x, with-arithmetic 0.14x, groups 0.36–0.48x, join 0.85x, unique
  0.58x, sort + parquet write 0.53x. One machine, one workload, and the
  comparison is against our own polars backend's use of a lazy query engine —
  not a universal "faster than polars" claim.
- **The crossover happened at the 1M anchor first** (ADR 0033 Stage 3), then the
  full matrix fell in four passes: dictionary-encoded string columns, hand-built
  parquet pages, lazy per-column decode, and page-level predicate pushdown (the
  predicate runs once per distinct dictionary value, not once per row). polars
  remains the default backend and the oracle — Stage 4 (flipping the default) is
  deliberately not taken.

### Added — the native engine reads and writes parquet, and CSV goes parallel (ADR 0033 Stage 2)

- **A native parquet reader and writer** (zstd): the appliance build reads and
  writes the sibling engine's files. The writer hand-rolls the RLE codec and
  page-level IO (write_parquet 0.38x); the reader decodes columns lazily with
  deferred gathers — `count()` reads the footer only, a full-materialize read
  lands at 0.01x, a filtered scan at 0.58x.
- **CSV is parallel in both directions**: write_csv 0.39x, read_csv 0.84x
  against the polars backend on the same matrix.

### Added — the consolidated field review, answered (2026-08-24)

- **The differentiable surface closes over the elementary family.** Every routed
  unary (tan, asin, acos, atan, sinh, cosh, log2, log10, cbrt, degrees, radians,
  erf, normal_cdf, normal_pdf) joins relu/sigmoid/tanh/exp/ln/sqrt/sin/cos/abs on
  the tape; `max`/`min`/`clamp`/`hypot` carry gradients (ties route to the FIRST
  argument — the same convention relu's kink sets, so `max(a, b)` and the field
  idiom `a + relu(b - a)` agree everywhere); Array `.max()`/`.min()` fold tracked
  elements; **unary minus works on a tracked value** (`-v` is `0.0 - v`); and the
  method and free spellings agree — `v.to_array()` and `v.tan()` fall through to
  the free builtins, while tape-owned names keep the tape's errors. What still
  refuses (floor/ceil/trunc/round/sign) refuses HONESTLY: the error names the op
  and the way out (`value_of`), never "expected a number, found a Node".
- **An unrecognized field in an `http_request`/`http_stream` record is a hard
  error** naming the field and listing the ones read — `cookies:` (for `jar:`)
  had silently produced a cookie-less session; a typo'd `timeout_ms` left a
  request with no total deadline. The helix.toml unknown-key rule, one layer down.
- **`headers(pairs)`** constructs the case-insensitive Headers type (wire order,
  repeats kept, injection-validated) so test doubles can be the type live
  responses carry. **`url_decode_lenient`** never raises on any string (malformed
  `%` stays literal) — the server-edge twin of the strict decoder.
  **`url_encode(s, set)`** names RFC 3986's grammars ("segment", "query",
  "fragment", "userinfo"). **`to_dataframe(rows)`** builds a frame natively from
  an array of records (previously python-gated with no bridge at all).
  **`flat_map`/`count_where`** (parser desugars), String **`replace_first`** and
  **`last_index_of`** (character index, like `index_of`). `dna` is idempotent.
- **`helix describe <name>`** answers about ONE name — with a signature, a doc
  sentence, ONE executed example, a `differentiable` flag, and a `notes` channel
  for semantic surprises (last-wins, raises-on, seed-threaded); `helix doc <Type>`
  lists per-method signatures and doc lines. Every example with an output is run
  by the gate, so the documentation cannot drift. `helix test` states its version
  first, on every path.
- **Errors that teach, per the review's evidence**: a failed format spec names
  the `{{`/`}}` escape (the CSS/JSON brace trap); `try(f()).ok` explains that
  `try` binds tighter and how to bind first; a doc-example failure with a `...`
  continuation states the one-line rule.

### Added — tooling, organization, and the generated reference (2026-08-24)

- **`where` clauses** (ADR 0035): `fn f(c) = LOOKUP.get(c) ?? "…" where LOOKUP = …`
  — the scaffolding after the point, desugared to `let … in` at parse time (the
  engines cannot drift). fn definitions only; `where` remains an ordinary name.
- **`helix test --json`** — one JSON document (version, totals, per-event
  file/line/code/expected/got) with exit codes identical to the prose mode.
- **`helix check --lint`** — advisory notes for the field corpus's real traps
  (`reduce(dict(), …)` with the last-wins warning, `0 - x` now that unary
  minus is universal, `export fn` without an executable doc example). Never
  changes the exit code.
- **`helix doc --markdown` and docs/reference.md** — the full stdlib reference
  (357 names: signature, doc line, notes, executed example) generated from the
  docs table; a gate test regenerates and byte-compares it, so the committed
  reference cannot go stale.
- **A tracked exponent differentiates**: the full `a ** b` node (d/db =
  a^b·ln a) replaces the refusal; a non-positive base under a tracked exponent
  still refuses, saying why. The receiver lift is gated on the tape's own
  method names, and a tracked value in a plain op's error is called "a tracked
  value" with a pointer at describe's `differentiable` flag.
- **The codebase reorganized**: `interp/builtins` (11 topical modules) and
  `interp/methods` (per-type files) replace the two fattest files in the tree —
  verbatim moves, gate-proven behavior-identical. `scripts/release.sh` +
  docs/RELEASING.md codify the versioning policy and release ritual;
  `scripts/gate.sh` prints per-phase timings and gains a loudly-labeled
  `GATE_QUICK=1` iteration loop.

### Changed

- **Records print in canonical sorted field order.** `==` always ignored order
  and `to_json` always sorted; now the printer agrees, so a doc example
  documents the value rather than the construction route — `{name: "gc",
  arguments: 1}` and its `parse_json` twin both print
  `{arguments: 1, name: "gc"}`. A program that depended on insertion-order
  printing must sort its expectations once.
- **The frozen frame footer singularizes**: a one-row frame prints `(1 row)`.
  This is a versioned change to the frozen format (spec rule 5), made now,
  before the plural could ossify as a permanent cosmetic wart.

### Fixed

- **A tz-aware timestamp no longer aborts the process**: the polars backend
  prints tz-aware datetimes as UTC text instead of panicking mid-print.

## v0.4.0 — 2026-08-23

**The trust release.** Everything here is about what a program can rely on: an HTTP
client that is secure by default (ADR 0031, all four steps), frames whose semantics
are the language's own (ADR 0033/0034), printed output no environment variable can
bend, binaries that start on a 2022 Linux and fit on a small one, and a method call
that can never capture the wrong thing. The minor version moves because
`print(df)`'s bytes changed — everything else is additive or stricter.

### Security — the HTTP client earns its defaults (ADR 0031)

- **Header injection is refused before the wire**, both directions: a CR/LF (or any
  C0 control) in a header name or value is a positioned error at construction —
  request and response alike.
- **`Headers` is a type**, not a Dict: case-insensitive lookup (`get`/`get_all`/
  `has`/`keys`/`values`/`items`), wire order preserved, repeated headers kept — an
  HTTP/2-style lowercase response and an HTTP/1.1 one read identically.
- **Per-request `total_ms`/`connect_ms`/`read_ms`/`max_body`** — every limit that
  trips is an error naming itself, never a truncated body pretending completeness.
- **Redirects enforce the boundary rules**: `Authorization`/`Cookie`/
  `Proxy-Authorization` stripped when the origin changes (not configurable),
  `https` never silently downgrades, non-http(s) schemes refused, ten hops max,
  and the chain comes back as data (`redirects`). QUERY keeps its method and body
  through 301/302 per RFC 10008.
- **A cookie jar** (`cookie_jar()`), explicit program-held state threaded per
  request — never ambient. Supercookies are structurally refused via the Public
  Suffix List (`Domain=.co.uk` falls back to host-only); `Secure` never crosses
  plain http; `Max-Age` beats `Expires`; expired cookies evict on read and write.

### Added — the appliance profile (ADR 0032)

`--no-default-features --features appliance` builds a **9.3 MB** helix (was
51.8 MB) with the FULL language surface: every DataFrame, genomics, and JIT verb
still exists, type-checks, and describes itself — running one names the feature to
rebuild with. Feature gates: `dataframes` (polars), `bio` (noodles + needletail),
`jit` (cranelift; bytecode is identical either way — the flag changes speed, never
output), `native-df` (the new engine below, included in appliance). Defaults
unchanged: `cargo install helix` is still the full flagship.

### Changed — printed DataFrames have a Helix-owned format (ADR 0033, Stage 0)

`print(df)` and `"{df}"` interpolation now emit Helix's own frozen table text
instead of the engine's (polars') display:

```text
region  samples    af
------  -------  ----
east         12   0.5
(1 rows)
```

Why it changed: the old text was whatever the DataFrame engine printed — it varied
with `POLARS_FMT_*` environment variables (the same program, different bytes, based
on the caller's environment) and would have changed again with any engine change.
The new format is deterministic, environment-insensitive, engine-independent, and
identical across all three engines; cells format exactly as the language's own
scalars (`2.0` floats, `missing`, ungrouped integers). Scripts that parsed the old
box-drawn table must switch to `write_csv`/`to_json` (which were always the stable
interfaces) or the new format. Interactive rich rendering is unchanged.

### Added — a native DataFrame engine (ADR 0033 Stage 1, ADR 0034)

Builds without polars can now carry working DataFrames: `--features native-df`
brings an eager, deterministic engine whose column expressions run through the
interpreter's own scalar kernel — a frame expression means exactly what the same
expression means on scalars, including euclidean `%`, true division, and
division-by-zero as a positioned error naming the row. Aggregations implement
the missing-propagation doctrine directly; joins and groups are deterministic by
construction. The appliance profile now includes it: full frame pipelines
(read_csv/where/with/sort/group/join/write_csv) in a 9.3 MB binary. The default
build still uses polars; a dual build (`--features native-df` on top of default)
runs both engines and is differential-tested verb by verb.

### Fixed

- **UFCS could capture a Python attribute call**: `np.round(1.5)` was rewritten to
  `round(np, 1.5)` because dynamic-dispatch receivers resolve attributes at run
  time. The parser rewrite now covers user-defined functions only; builtin
  chaining is restored by a runtime fallback that fires only after method dispatch
  fails on the receiver — provably additive, and excluded for PyObject/Node/
  DataFrame/GroupBy receivers.
- **The event-loop server's request headers** had drifted to a case-sensitive Dict
  on the concurrent path while the blocking path got the `Headers` type —
  `get("content-type")` missed at 55k req/s. One shared parser now serves both.
- **Release binaries start on 2022-era Linux again**: the gnu artifacts silently
  inherited the CI runner's glibc 2.39 floor (locking out Ubuntu 22.04, Debian 12,
  and every RHEL); both gnu builds are now pinned to a 2.35 floor, the workflow
  asserts the artifact matches what the installer advertises, and `install.sh`
  auto-selects the static musl build on musl distros and any glibc below the floor.

### Performance

- **Serving**: `listen(port, shards)` composed with the event loop measures
  317–336k req/s on a 6-core box (~2x a node 24 cluster at equal workers), p50
  0.29 ms — near-linear scaling; the previously recorded ~90k ceiling was a host
  power-management measurement artifact, now documented.
- **Old hardware**: serving 100 connections fits under a hard 20 MB memory cap at
  full throughput; a 40%-throttled core sustains 21k req/s. Keep-alive buffers
  shrink after large bodies (was: 300 idle connections could pin 300 MB), inbound
  reads gained a 64 MiB per-shard budget mirroring the SSE side, and every eval
  thread's stack reservation dropped 1 GiB -> 128 MiB (measured ~1 KiB/frame;
  strict-overcommit VPSes can actually spawn shards now; `HELIX_STACK_MB`
  overrides).

## v0.3.0 — 2026-08-20

**The language-surface release.** A 13-library / 117-module / 15,260-line review of
v0.2.7 arrived with its claims already probed against the released binary, and this is
the answer to it — plus the tensor bridge the nn build was blocked on, and the pieces a
web library could not be written without. The minor version moves because programs can
now say things they could not say before, not because anything they said has changed.

### Added — the language

- **A function can be called in method position.** `x.f(a)` means `f(x, a)` when `f` is
  no type's method, so your own functions chain like built-in ones:
  `{w: 2, h: 3}.scaled(2).area()`. This removes the method-vs-function split rather than
  documenting it — the review's complaint was that `to_array(t)` is a function,
  `a.matmul(b)` is a method, and you learn which one error at a time. It is also what
  people usually want from classes: methods on your own types, with no new kind of
  entity, no inheritance, and no mutable object identity. Strictly additive by
  construction: the rewrite is gated on the name belonging to no type, so no call that
  resolved before can change meaning, and a misspelled method still gets the method
  error with its did-you-mean.
- **Range patterns in `match`** — `200..300 => "success"`. Half-open, `lo <= x < hi`,
  the convention `range(lo, hi)` and `xs[lo:hi]` already use, so adjacent bands tile
  exactly: nothing lands in two of them and nothing falls between. A range asks about
  magnitude, so `2.5` matches `0..5`; literal patterns keep their existing strictness.
  An impossible range (`5..0`, `3..3`) is refused where it is written.
- **Dict literals** — `{"Content-Type": "application/json"}`. A quoted key makes a brace
  a Dict; a bare name still makes it a Record. A record field is a NAME, so a map whose
  keys are not names — every HTTP header, every JSON object, every table transcribed
  from a document — could previously only be written
  `[("Content-Type", "…")].to_dict()`, a constructor shaped like a fold.
- **Block comments** — `#[ … ]#`, spanning lines, and **nesting**, so a region that
  already contains one can be commented out whole.
- **The scalars→tensor bridge.** `tensor([[w11, w12], [w21, w22]])` over tracked
  scalars builds a tracked tensor, so a trainable layer's weights can be ordinary
  variables and its forward pass an ordinary BLAS `matmul`. `t[i]` and `t[a:b]` on a
  tracked tensor stay on the tape, and `shape`/`count`/`ndim` read the value.

### Added — the library

- `s.split_once(sep)` → `(before, after)` or `missing`. Splitting at the FIRST separator
  is the commonest parsing step there is and had no direct spelling; the idiom it
  replaces recovered the tail by arithmetic on the first part's length.
- `s.index_of(needle)` → the first match's **character** index, the unit every other
  String method counts in, so it feeds straight back into `drop`/`take`/`s[a:b]`.
- `xs.windows(n)` and `xs.chunks(n)` — `Dna` had `windows`; an array had to hand-roll it.
- `s.concat(t)` — the sibling of `Array.concat`.
- `url_encode` / `url_decode` (RFC 3986). Without them a query string cannot be built
  correctly, and the hand-rolled version cannot be right: percent-encoding is defined
  over UTF-8 **bytes**, so `café` must become `caf%C3%A9`.
- `parse_cookies` and `parse_set_cookie`. Builtins rather than string code because the
  naive version is wrong in a way that looks right — an `Expires` attribute contains a
  comma, so splitting `Set-Cookie` on `,` tears the date in half.
- `to_dict` now takes a pair **however it is written**: `(k, v)` or a two-element array.
  That is why the review found seventeen `reduce(dict(), …)` folds — tables transcribed
  as arrays of arrays could not use the verb that exists.
- `{...dict, k: v}` — a Dict spreads into a record, the request-builder shape.

### Fixed

- **Three shapes that could produce a silently wrong program**, all now refused at check
  time: `try(() => f())` reported SUCCESS (building a closure cannot fail, so error
  handling written that way never fired); `fn relu(x) = relu(x)` defined infinite
  recursion under a shadowed builtin; and a top-level value used above its binding said
  only "not defined", with no hint that a `fn` may be used above its definition and a
  value may not.
- **HTTP connections are reused.** `get`/`post`/`request` each built a fresh agent, and
  the agent IS the connection pool — every call opened a new TCP connection and redid
  the TLS handshake. For a loop against one API host that handshake dominated the
  request.
- **The recursion-depth error names the cause**: the cap only ever binds a NON-tail
  shape, since tail and mutual-tail recursion run to millions of levels, and the message
  never said so.
- **`helix fmt`** was rendering `xs[0:2]` as `xs[0: 2]`, `match x { … }` as
  `match x {…}`, and `try (a / b)` as `try(a / b)` — three cases of a rule written for
  one construct meeting a token it also owns elsewhere. The last is the spelling the
  language's own precedence error recommends.

### Notes

- Everything here is pinned on all three engines. The release was preceded by a
  cross-feature sweep — dict literals inside match arms, UFCS inside interpolation,
  block comments between match arms, ranges beside guards — with zero divergences.
- The HTTP client's remaining gaps are known and specified rather than half-built: a
  cookie jar, method-preserving redirects (which RFC 10008 requires for QUERY),
  per-request timeouts, and case-insensitive header lookup. The QUERY method itself
  already works through `http_request`, verified end to end.

## v0.2.7 — 2026-08-19

**The stabilization release.** Before opening the next feature tier, everything the
last six releases added was put under an adversarial sweep: six agents, ~450 probe
programs, each run on all three engines and against the released v0.2.6 binary. This
release is what the sweep found — two criticals in v0.2.6's own `let` widening, three
silent-wrong-gradient shapes in the autodiff tape, a nondeterministic join, and three
ways `helix test` could misreport what it ran. No new features; every fix is pinned.

### Fixed — the JIT

- **A `let` in a float reduce body no longer aborts the process or silently prints
  `inf`.** v0.2.6 admitted `let` into the kernel's eligibility analyses and its codegen
  but not into the predicate that decides whether a kernel is built with its *poison
  cell* — the mechanism that carries a raised error out of a compiled loop. So `let`
  bodies were built without one. A user function called in a binding's initializer
  (`let d = sq(i * 1.0) in a + d`) then reached codegen that cannot represent it and
  aborted: exit 134, uncatchable by `try`, where both interpreters print `14.0`. A
  division by zero under a `let` was worse — the JIT printed `inf` and exited 0 where
  both interpreters raise `division by zero` and exit 1: a wrong answer *and* a wrong
  exit code from the flagship engine. Both were the same missing case, which now
  recurses into every binding initializer and the body. The kernel still engages
  (~30× on the 200k×64 correlation shape); the poison cell costs nothing measurable.

### Fixed — autodiff

- **A tracked value refuses mismatched shapes exactly where a plain one does.**
  `variable(tensor([1.0, 2.0])) + tensor([1.0, 2.0, 3.0])` silently returned the left
  operand unchanged — no addition performed, no error raised — and asking for a
  gradient through it aborted the process (exit 134, uncatchable) inside the backward
  pass. Both symptoms were one defensive fallback answering for a user's shape mistake;
  the tape now raises the same `cannot broadcast tensors of shape [2] and [3]` the
  plain expression raises. Legitimate broadcasting (the bias-add shape) is unaffected.
- **A tracked exponent is refused rather than silently frozen.** `gradient(2.0 ** x, x)`
  returned `0.0` — the exponent was read as a constant and dropped from the graph —
  where the true derivative is `2^x · ln 2`. It now reports "a tracked value can only be
  raised to a constant scalar power", the error already documented for tensor
  exponents. A constant exponent (`x ** 2.0`) is unaffected; a differentiable `a ** b`
  is a feature, not a silent zero.
- **A variable that does not feed the loss has gradient zero.** The backward pass zeroes
  only the nodes the loss reaches, so a variable left over from an earlier `gradient(…)`
  call reported *that* call's accumulation instead: in a training loop, one parameter's
  gradient could be another loss's. Nodes now carry the identity of the pass that
  touched them, and a variable from any other pass reads zero.
- **`gradient(x ** 0, x)` is `0.0` at `x = 0`** (was `NaN`).

### Fixed — DataFrames

- **Grouped aggregation after a join is deterministic.** Join output order was decided
  per plan execution, and `.column()` re-executes the plan — so two column reads of one
  grouped-after-join frame could pair keys from one ordering with values from another,
  at exit 0, with no warning. In the sweep's 500-group probe roughly 490 of 500 rows
  silently mispaired; even a two-group frame tore in 16 of 40 runs. Joins now pin
  reading order, the same guarantee `.sort()` already makes. The `.sort(key)` and
  `.cache()` workarounds are no longer needed.

### Fixed — `helix test`

- **The file walk terminates on symlinked directories.** A directory symlink pointing
  into its own tree made the runner count one test file 41 times and report success;
  two such links made it recurse until killed. Both collectors now share one walker
  that remembers where it has been.
- **Overlapping roots count each test once.** `helix test dir dir/module.helix` re-ran
  and re-counted that module's doc examples, reporting more passing tests than exist.
  Doc sources are now gathered across all roots and deduplicated by canonical path, as
  the test-file list already was — that list's adjacent-only deduplication had missed
  interleaved walks and differently-spelled paths too.
- **A doc example whose last line is `print(…)` can pass.** The harness wraps an
  example's final line in `print(…)` when the example documents output, so one that
  already printed emitted its value and then `()`. The failure report showed
  `expected: 3` against `got: 3` — identical to the eye, the real difference invisible.

### Notes

- Every fix above is pinned by tests that run on all three engines. The sweep's
  remaining findings are recorded in [`docs/dx-plan.md`](docs/dx-plan.md) with their
  mechanisms rather than fixed in haste: a grouped `i64` sum that wraps where the array
  path promotes, the frame/array disagreement on sorting `missing`, backend error text
  that names no Helix concept, and two name-resolution gaps around imported module
  names.
- The sweep's four other agents found no cross-engine divergence at all across 126
  fold fast-path programs, 87 CLI-surface programs, and the aggregate/eligibility
  families — the three-engine oracle held everywhere it was not explicitly broken by
  the two defects above.

## v0.2.6 — 2026-08-16

The reduce-eligibility family completed, and the autodiff aggregates closed.

### Performance

- **`let` in a float reduce body takes the JIT kernel** — the last live reduce trap
  (~19–23× in the field, measured 33× kernel engagement here: 51 ms vs 1,706 ms
  interpreted on the 200k×64-tap correlation shape). The old guidance — "write the
  subexpression twice; the native loop with a redundant op beats the interpreted one
  with CSE" — is dead: bindings are scoped in the analyses and the codegen exactly as
  the walker scopes them, sequential visibility included, with nested shadowing
  restoring on scope exit. Rebinding the accumulator or counter, and indices that
  mention a local, decline to the general path and stay bit-identical.

### Added

- **`.mean()` and `.product()` carry gradients** on arrays of tracked values, joining
  v0.2.5's `.sum()` — `mean` differentiates as fold-add over a divide (gradient exactly
  1/n), `product` accumulates repeated-element gradients (`[a, b, a]` gives d/da = 2ab).
  `.max()`/`.min()` remain unsupported on tracked arrays deliberately: a tie's
  subgradient needs a design decision, not a guess.
- **Pinned: `variable(tensor(…))` differentiates through `matmul`** — tensor-aware
  autodiff has existed in the tape and is now under test on all three engines
  (`gradient(w.matmul(w).sum(), w)` returns the analytic gradient exactly). A trainable
  layer's forward pass can be a real BLAS `matmul` when its parameters are created as a
  tensor variable. The scalars→tensor bridge (`tensor([w, …])` from tracked scalars)
  remains open.

## v0.2.5 — 2026-08-16

Two field reports from the llm/nn library builds, answered.

### Performance

- **A reduce's initial value may be any expression** — the last silent JIT cliff. A
  non-literal init (`reduce(a0, …)`, the natural ODE-integrator spelling) never
  compiled: identical body, identical answer, 21–53× slower. Four compile-time gates
  required a `Float` *literal* as a stand-in for a check the dispatch already makes on
  the runtime value — so a non-literal init now enters the float kernel family and the
  runtime decides. Parameter init at 100M iterations: **3,117 → 64 ms**. An init that
  turns out to be an `Int` falls back to the bytecode loop with a bit-identical answer.

### Added

- **The autodiff surface the nn library needed**: `sin`, `cos`, and `abs` gain
  derivative arms on the tape (`abs` uses the same subgradient convention at its kink
  that `relu` always has); `.sum()` on an array of tracked values folds on the tape, so
  the two spellings of a sum carry gradients alike instead of silently forking by
  capability; and `to_array(tensor)` flattens **natively** (row-major) — it was
  Python-gated, which put a feature wall exactly between the BLAS tensor path and the
  autodiff tape on a stock binary.

## v0.2.4 — 2026-08-16

**The linear-accumulation release** — [ADR 0029](docs/adr/0029-linear-accumulation.md)
implemented in full: building a collection one element at a time in a fold is
amortized-linear **on every engine, for every spelling** — arrays, dicts, and strings —
or it declines to the copy path and stays correct. The v0.2.3 field report measured the
released walker at the O(n²) signature (×2.1 → ×2.8 → ×5.2 ratios); this release is the
answer.

### Performance

- **The tree-walker's fold is linear** — the last engine with the append wall. The VM's
  take-append-store discipline transplanted: 262k array appends **6,768 → 23 ms**
  (294×), 64k dict inserts **17,689 → 16 ms** (1,100×). The walker's reduce also stops
  boxing packed receivers up front just to iterate them.
- **The string-interpolation fold is linear on all three engines** — previously
  quadratic everywhere (`13.6–14.6×` per 4×n; no engine had a fast path). The new
  `AppendStrIntoLocal` op and its walker twin render the tail first, so every fallible
  step happens with the accumulator untouched; a non-string init still formats exactly
  as before; a format spec on the accumulator hole still takes the general path (it
  re-pads, so append would be wrong); growth is fallible (a refused reservation reports
  instead of aborting). After: 4×n costs ~1.8×.
- **Duplicate fold binders `(a, a)` never take the fast paths** (shipped as a
  correctness guard in v0.2.2; the fast paths added here inherit it).

Every fix is pinned two ways: a complexity-class test (n vs 4n in one process — a
quadratic regression fails the gate rather than waiting for a field report) and a
semantics table byte-identical across all three engines *and* against the previous
release — self-referencing arguments, shared inits, scan's snapshot history, mid-fold
error restore, and the string fold's format-spec and non-string-init edges.

## v0.2.3 — 2026-08-15

Two fixes, both found by re-verifying v0.2.2's claims against the installed release —
the survivors of that audit.

### Fixed

- **The module-namespace guard now reaches string interpolation.** v0.2.2 fixed
  `print(mod.position(…))` but `emit("{mod.position(…)}")` still failed for all seven
  comprehension-lowered names — and the interpolated form is the idiomatic print, so the
  fix had missed exactly where users hit the bug first. An interpolation hole is parsed
  by a fresh parser; the import-namespace set now rides into it the same way function
  signatures always did.
- **`helix fmt` no longer indents a column-0 comment into the previous function's
  body.** Trigger: a nested lambda wrapped across lines — its closing brackets sit at
  line *end*, and the indent tracker only unwound *leading* closers, leaving a dead
  step for the next flush-left line to inherit. Dead steps are now discarded per line;
  all 54 example files still reformat to byte-identity.

## v0.2.2 — 2026-08-15

**The discoverability release** — plus the two deepest performance fixes since the
append wall, and four wrong answers removed. Everything below was driven by field
reports from the language's heaviest users (the #19 review and the physics-library
build); every fix is pinned by a test confirmed to fail on v0.2.1 first.

### Fixed

- **`helix test a.helix b.helix` ran only the first file, silently** ("running 1 test
  file") while `helix check` accepted many — anyone verifying two modules in one command
  believed both passed. Every path argument is now a root; files and directories mix.
- **Duplicate fold binders `(a, a)` diverged across engines**: the accumulator fast path
  matched by binder name, so `[[1],[2]].reduce([], (a, a) => a.concat([9]))` answered
  `[9, 9]` on the VM/JIT and `[2, 9]` on the walker — silent, exit 0. All engines now
  agree on the correct last-write-wins answer.
- **Qualified module calls beat comprehension sugar**: seven receiver-blind parse-time
  desugars (`position`, `sort_by`, `take_while`, `drop_while`, `min_by`, `max_by`,
  `zipmap`) intercepted `mymod.position(a, b, c, d)` and rejected a module's own 4-arg
  export. The parser now knows import namespaces; methods on them resolve to the module.
- **`unique()` on a large packed array could abort the process** — the general method
  dispatch boxed the whole buffer first (80M ints → 1.9 GB before one comparison). Now a
  packed fast path with fallible growth: refuses cleanly where memory genuinely runs out,
  and is 15–25% faster where it doesn't.
- **`helix test <file>` on a documented module** now answers what the directory run
  answers, instead of passing its examples and failing the file for asserting nothing in
  one output.

### Performance

- **The i64 map kernel admits affine indices**: `a[2*i]` 486 → 27 ms at 10M (16×), the
  3-point stencil `a[i] + a[i+1] + a[i+2]` 1871 → 31 ms at 20M (**50×**), measured
  against the v0.2.1 release binary. Values bit-identical on all three engines,
  including out-of-bounds and negative-index behaviour.
- **Array-of-strings accumulation is linear**: the fold spelling
  `lines.reduce([], (acc, s) => acc.concat([s]))` went **235.8 s → 61 ms** at 256k
  pieces (a `Values` arm in `concat_in_place`, guarded by a non-numeric witness that
  makes representation change impossible by construction).

### Added

- **`helix.toml` is a real manifest**: `description`, `authors`, `license`,
  `repository`, `keywords` — and `helix = ">=X.Y.Z"`, an **enforced toolchain floor**:
  an older binary opening the project reports *"this project requires Helix >= X, and
  this binary is Y"* once, instead of failing sixty confusing ways on unknown syntax.
  `version` must be comparable MAJOR.MINOR.PATCH. `helix new` writes the full template.
- **`helix describe` carries signatures** derived by probing the checker's own tables —
  accepted arities and per-arity return types (`round`: 1 arg → `Int`, 2 → `Float`),
  `null` where the checker genuinely does not constrain them, never fabricated.
- **`helix doc <name>` reverse lookup**: a method or builtin by name answers with every
  owner type, its effect, and an example receiver — the question users actually arrive
  with, previously answered by "error: unknown type".
- **`expect(k)` on Dict and Record** — the loud lookup: raises at the miss (with a
  one-edit did-you-mean over the collection's own keys) where `get`/`d[k]` keep
  ADR 0001's propagating `missing`.
- **Queries can name missingness**: `where(@v.is_missing())`, its `not` negation, and
  `drop_missing()` on DataFrames. `where(@v == missing)` still selects nothing — that is
  ADR 0001 semantics, and now there is an honest spelling for the real question.
- **The map at the door**: bare `helix` points at `helix help` / `helix doc` /
  `helix describe`; `AGENTS.md` at the repo root carries the commands, the three-engine
  correctness model, and the wrong-answer footguns.
- **Errors teach**: `prefix_sum` steers to `cumsum`/`scan`; an undefined name in a
  string hole explains interpolation and the `{{ }}` escape; `fn` inside `do {}` names
  the rule; unknown methods point at `helix doc <Type>` instead of dumping 79 names;
  reassigning `e`/`pi`/`inf` says what the constant is instead of suggesting a shadow.

## v0.2.1 — 2026-08-15

**The trust release.** No new features and no breaking API: seven wrong answers removed,
every one pinned by a regression test that was first confirmed to fail against the v0.2.0
binary, on all three engines. Three were release blockers found by a ~4,000-program stress
fleet within a day of v0.2.0 shipping; the deepest were on the DataFrame/Polars seam, not
in the language core.

### Fixed

- **Grouped `f64` `sum`/`mean` was nondeterministic** — the same pure expression could give
  a different answer across runs (and even twice within one program), because Polars'
  partitioned group-by merges intra-group values in a scheduling-dependent order. Grouped
  aggregations are now built so they structurally cannot take that path; the regression
  test evaluates the same query 30× in one process. This restores ADR 0020 reproducibility
  and un-blinds the three-engine differential oracle.
- **Grouped aggregations skipped `missing`** (ADR 0001 inverted): a group containing an
  unknown reported a number, and an all-`missing` group reported `sum` `0.0` —
  indistinguishable from a genuine zero. They now propagate `missing` per group, matching
  the array and whole-column paths. `count` is the deliberate exception: it counts rows,
  as `[1.0, missing].count()` always has.
- **Oversized collection materialization could abort the host** (SIGABRT, exit 134, not
  catchable) — the one ADR 0024 violation found. Three causes, all fixed: the tree-walker
  materialized packed arrays up front just to iterate them; the packed builder arms could
  overshoot their budget on `Vec` doubling; and the byte budget was checked only after the
  push. All three engines now refuse with the same catchable error, in the same words.
- **`.sum()`/`.mean()` returned `NaN` where IEEE-754 returns `±inf`** — Neumaier
  compensation went non-finite with the running sum. The compensation (and its accuracy on
  finite data) is kept; the non-finite case now answers what python3 and NumPy answer.
- **`erf` was the only math builtin not computed to double precision** (~1.4e-7 absolute
  error, discontinuous at 0, unbounded relative error near 0). It now matches python3's
  `math.erf` bit-for-bit across the tested grid, and `normal_cdf` routes through `erfc`,
  fixing the left tail — the p-value case — which no `erf` accuracy alone could fix.
- **DNA IUPAC arithmetic**: `gc_content` counted `S` ("G or C" — GC by definition) as
  non-GC, so `dna("S")` read 0.0 and `dna("GCS")` read *lower* than `dna("GC")`. The
  policy, now uniform and documented: a base participates iff its GC-ness is certain
  (`S` is GC, `W` is not; `N R Y K M B D H V` are excluded from numerator and denominator
  alike — the rule `N` always had). A sequence with no classifiable base answers `missing`,
  never a fabricated `0.0`, and that propagates through `mean_gc`, which had been averaging
  all-`N` sequences in as `0.0`.
- **`try 1 + 1` produced a true but useless error** ("got a Record", with no record in
  sight — it parses as `(try 1) + 1`). The error now explains that `try` binds tighter
  than the operator and shows the parenthesized fix. Ordinary record operands keep the
  ordinary message.

### Hardened

- **`sort` tie order is now a contract, not an accident**: DataFrame `sort` pins
  `maintain_order`. No misbehaviour was ever reproduced (300k rows, int and string keys),
  but unstable tie order is unspecified and every `.column()` re-executes the lazy plan,
  so the pin makes the stability ADR 0020 assumes survive any Polars upgrade.

## v0.2.0 — 2026-08-13

**The consistency release.** Every breaking change this project has decided is in this one
version, so upgrading is one deliberate step instead of a drip. Each break below names the
ADR that argued it; each was landed with its blast radius measured first, and the ordering
changes are specified cell-by-cell by `tests/ordering_matrix.rs` (247 pinned
expression/answer pairs, identical on all three engines).

The theme is one sentence: **a library author must be able to write correct code without
knowing anything about the caller** — not their schema, not their manifest, not which
builtins a future Helix adds.

### Breaking

- **One order, one domain** ([ADR 0025](docs/adr/0025-ordering.md), all four questions).
  `sort`, `argsort`, `sort_by`, `min`/`max`, `min_by`/`max_by` and `argmin`/`argmax` now
  agree about what can be ordered and what `missing` means:
  - `argsort` (and therefore `sort_by`) adopts `sort`'s policy: an array with `missing`
    **errors** with the same message and hint, and DNA orders. Before, `xs.sort()` and
    `xs.sort_by(it)` disagreed with each other.
  - `min`/`max` widen to `sort`'s type domain: strings and DNA now reduce
    (`["b","a"].min()` is `"a"`), so `min() == sort().first()` holds everywhere. The
    reduction policy is unchanged: `missing`/NaN still propagate as `missing`.
  - `min_by`/`max_by` and the **method** `argmin`/`argmax` adopt the reduction policy:
    `missing`/NaN propagate instead of raising leaked internals (`` `if` condition is
    `missing` ``, `index 0 is out of bounds`), and the empty array gets a named error.
    `xs.argmax()` and `argmax(xs)` now give the same answer to the same question.
  - Signed-zero ties on `argmin`/`argmax`/`min_by` remain IEEE first-wins, now documented
    with runnable examples in `examples/language/ordering.helix` (the gate executes them
    on all three engines, so the documentation cannot drift).
- **In a DataFrame query, a bare name means the binding, not the caller's column**
  ([ADR 0028](docs/adr/0028-query-name-resolution.md)). `fn above(frame, cutoff) =
  frame.where(@value > cutoff)` no longer changes meaning when the caller's data happens
  to have a `cutoff` column. `@name` still pins the column side; a bare name with no
  binding in scope is still a column. This was the last known silent wrong answer.
- **A top-level `fn` is file-scoped** ([ADR 0027](docs/adr/0027-builtin-shadowing.md)).
  It is callable above its own definition, and shadowing a builtin is retroactive for the
  whole file — a name means one thing per file. This removed a silent three-engine
  divergence, and it is what lets future Helix releases add builtins without breaking
  published libraries that already use those names.

### Fixed

- **A consumer's dependency can no longer capture a library's private import.** A file's
  own sibling now wins over a dependency key, so installing a package named `helpers`
  cannot rewire another library's `import helpers`. The failure was silent (exit 0,
  `check` ok, all three engines agreeing) and is now pinned by the one test able to see it.
- **Growing an array or dict in a fold is linear.** 256k `acc.concat([x])` appends went
  6.49s → 0.04s (158×) and 4M run in 0.25s; `Dict.insert` follows. The accumulator is
  taken, extended and stored as one VM instruction, so nothing observes it mid-move.
- **A `reduce` body can call a user function with a float signature** — 1.84s → 0.05s at
  n=20M (the kernel used to decline outright). A `/0` or NaN comparison inside the callee
  still raises the exact interpreter error.
- Mutual recursion and forward references compile and run on all three engines, with
  mutual tail calls constant-space to 1M frames.

### Added

- `raise(message[, help])` — a library can reject an argument in its own words, with a
  `help:` line, instead of `assertion failed: …`. Caught by `try` like any error.
- `source_path(rel)` — resolves against the directory of the file the call is written in,
  so a package can ship and read its own data regardless of the process working directory.
- `chars()` — a string as an array of characters (Unicode scalars). The previous
  spelling, `replace("", "\t").split("\t")`, was both undiscoverable and wrong.
- `helix test` runs your `## >>>` doc examples on all three engines and fails a test file
  that asserts nothing. A facade re-export (`export f = lib.f`) keeps defaults and named
  arguments. `helix verify` hashes a dependency's data files, not just its source.

### Upgrading

If your code never shadows a builtin, never names a `min_by` parameter after a DataFrame
column, and never sorts arrays containing `missing`, v0.1.1 programs run unchanged. The
repository's own corpus, examples and benchmarks needed **zero** edits beyond the ordering
test matrix itself; the one fixture that deliberately pinned the old shadowing behaviour
was regenerated.

## v0.1.1 — 2026-08-11

**The first installable release.** `v0.1.0` is published but nobody can install from it, and
this release exists to replace it rather than to add anything.

### What was wrong with v0.1.0

Probed against the live release: `helix-x86_64-unknown-linux-gnu.tar.gz` → **404**,
`SHA256SUMS` → **404**. Four of six platforms uploaded, and the installers refuse to install
what they cannot verify, so even those four were unreachable. Three independent causes:

- The profile-guided Linux build died in its training step on
  `bench/crosslang/b3_groupby.helix`, which still called `io.read_csv(…)` — a spelling
  ADR-0017 removed. **Nine** benchmark programs had rotted the same way; nothing in CI ever
  compiled them, because every gate the project had *runs* its programs and these need a
  250 MB generated fixture first.
- Six build jobs each called `action-gh-release`, which creates-if-missing, so six of them
  raced to create one release. The musl job died mid-upload.
- The checksum job was skipped for a failed dependency, and the incomplete release went
  public anyway.

### Fixed

- **The release is built as a draft and published last.** One job creates it, every build
  uploads into it, and publishing is gated on all six platforms plus `SHA256SUMS` being
  present. A failure now leaves an invisible, re-runnable draft instead of a broken public
  artifact.
- **Every build smoke-tests the binary it produced** where the runner can execute it, and
  the musl job asserts the artifact is genuinely static — the one machine that would
  otherwise discover it is the air-gapped one with no way to fix it.
- **`workflow_dispatch` takes a `dry_run` input** (default on) that builds and smoke-tests
  all six platforms while writing nothing, so the pipeline can be checked without publishing
  something to check it with.
- The nine stale benchmark programs are repaired and verified by running them.

### New

- **`helix check <script>…`** — type-check without running or writing anything. Takes many
  paths in one process; `scripts/checkall.sh` covers all 85 tracked programs in ~0.03 s and
  runs in CI, which is what closes the gap that let the benchmarks rot. It never executes
  the program, and it is honestly *only* a type check: code that checks clean can still fail
  at run time.

### Toolchain and hygiene

- All five CI jobs now block. Clippy is at zero warnings with `-D warnings` (no `#[allow]`
  suppressions); MSRV 1.96 is verified rather than asserted; `cargo audit` is green, with two
  advisories fixed by upgrade (crossbeam-epoch, quinn-proto) and four that have no reachable
  fix documented in `.cargo/audit.toml`, each with the crate that blocks it.
- `CONTRIBUTING.md` added.
- The Docker build context went from 1.3 GB to 9 MB — `.dockerignore` was not excluding
  `website/node_modules`, `website/.next` or the generated benchmark fixtures.

**No language changes.** A `v0.1.0` program runs identically on `v0.1.1`.

## v0.1.0 — 2026-08-11

First tagged release. Incomplete — see above; use `v0.1.1`.

# ADR 0045 — A method call is resolved by its receiver, not by its name

- **Status:** **Accepted 2026-08-31; implemented.** UFCS decides on the receiver at run
  time, on all three engines, with the type checker mirroring the same rule. A user's own
  `fn where` now answers `q.where(c)` on their record while an array keeps its
  comprehension and a DataFrame keeps its column verb — in the same program.
- **Date:** 2026-08-31
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0017](0017-methods-and-functions.md) (methods on data, free
  functions for the rest — this is what finally makes the two spellings meet),
  [ADR 0027](0027-builtin-shadowing.md) (a shadow is file-scoped and retroactive, which
  is why the checker uses the hoisted `fn` set and why an alias must decline),
  [ADR 0012](0012-dataframe-backend-seam.md) (the column verbs this had to not disturb),
  [ADR 0001](0001-missing-data.md) (`missing` keeps propagating through a comprehension),
  [ADR 0026](0026-library-performance-boundary.md) (the success path must not pay for
  this), [ADR 0024](0024-total-runtime-no-host-panics.md) (the new opcode is total).

## Context

Helix has had UFCS since v0.3.0: a function called in method position, so a user's own
verb chains like a built-in one. It was gated at **parse time** on
`registry::is_any_method(name)` — "no type has a method by this name". That gate is a
global test on a *name*, made at a point in the pipeline where the *receiver* does not
exist.

The consequence was not a corner case. These are the names a fluent library is made of,
and every one of them is some type's method:

    where  select  first  count  all  any  join  sort  take  drop
    insert  get  keys  values  filter  map  sum  min  max  unique

So `fn where(q, c)` sitting two lines above `q.where(c)` was invisible, and the call died
with ``type Record has no method `where` `` — a function in scope, named in the source,
reported as absent. A query builder, an ORM, a pipeline DSL, a matrix wrapper: none of
them could be written in the language, and the reason was never stated anywhere. The
fallback direction that *did* work covered only names nothing else claimed, which is the
set of names a library author would not pick.

`docs/dx-plan.md` had already named the fix — "decide on the RECEIVER at run time" —
after a related bug: the parse-time gate silently rewrote `np.round(1.5)` into
`round(np, 1.5)`, which type-checked clean, because a **PyObject** resolves its attributes
at run time and appears in no static table. That was fixed by narrowing UFCS to
user-defined functions, and the proper fix was deferred on the grounds that the VM "has no
way to invoke a function from inside `Op::CallMethod`". Half of it then shipped: a
builtin-named method that fails dispatch retries as a builtin, on all three engines. This
ADR finishes it.

## Decision

**The receiver decides, at run time, in every engine.**

### D1 — A failed dispatch retries as a free call

`x.f(a)` dispatches on `x`'s type as before. If that **fails**, and `x`'s type does not
own `f`, the call retries as `f(x, a)` — a declared `fn` first, a builtin of the same name
second. If neither exists, the original method error stands, with its did-you-mean intact.

`ufcs_fallback_applies` is what makes retrying safe: a type that OWNS the name never falls
back, so a DataFrame's `where` is still the frame verb and a real method's real error is
never re-run as something else.

**The success path pays nothing.** The previous shape cloned the arguments *before*
dispatch so a retry could still reach them, and then paid for an `is_builtin_name` lookup
to keep that clone off hot loops. But `call_method` only borrows: the retry can move what
is still there. Both the clone and the gate moved inside the error arm, which a working
method call never reaches.

### D2 — The two type-directed routes get a receiver test

Three families are compiled by *type*, not by name:

- `select`/`group`/`with` → DataFrame column-verb ops, whose args are `@column` syntax.
- `where`/`filter`/`map`/`reduce`/`scan`/`any`/`all` → inline comprehension loops, whose
  args are bodies with an implicit `it`.
- `join` → its own op, because it mixes an evaluated frame operand with by-name keys.

`join` needs the split MORE than the others, and was the one left without it at first. The
column-verb and aggregation routes fire for an `Unknown` receiver only when an argument
mentions a `@column`, which can mean nothing but a frame operation; `join`'s route has no
such gate, because a key may be written bare (`samples.join(meta, sample_id)`). So a user's
`fn join` was taken by the frame route — and only in a CHAIN, since a record literal types
as `Record` and declines, while a user verb's unannotated return is `Unknown` and does not.
That was an engine divergence: the tree-walker dispatched on the runtime record and
answered, the VM raised an arity error for a join it was never asked to do.

Neither has a dispatch to fail, so D1 cannot reach them — and `where` is the verb an ORM
needs most. Both were sound only on the assumption that the checker rejects every other
receiver, which stops being true the moment a user's own `fn where` is a real reading.

A call site with two readings now emits **both**, behind `Op::ReceiverIs(class)` — the
"peek-the-receiver test opcode" the deferral predicted. `RecvClass::Frame` is
`DataFrame | GroupBy | Missing`; `RecvClass::Iterable` is `Array | Missing`. `Missing` is
inside both on purpose: the existing routes carry ADR 0001's propagation, so sending
`missing` down them preserves it rather than re-deciding it.

**The other branch is the ORDINARY method path, not a call to the function.** The question
at a split site is not "frame or function" but "frame, or whatever this receiver normally
means" — and for a type that OWNS the name, that is its own method. `Op::Method` already
encodes the whole rule (dispatch; retry as a free call only on failure; never for an owning
type), and it is the same rule the tree-walker's other branch runs, so emitting it is how
the two stay one rule rather than two.

Emitting a direct call instead was the first version, and it was wrong: `Array` owns
`join`, so `xs.join(",")` through an untyped parameter went to a user's four-parameter
`fn join` — the walker answered `a,b` and the VM raised an arity error. `join` was where it
showed, because `join` is the only split-family name a non-frame type owns; the same
mistake was latent in the other two.

The receiver is compiled **once**, into a hidden local both branches reload — so its side
effects happen once whichever branch runs. Fusion is declined at a split site: a fused
chain rewrites several stages into one native loop and cannot be half-taken. Stages below
the split still fuse.

### D3 — What qualifies is a declared `fn`, unshadowed — and all three engines say so identically

The compiler resolves the fallback slot with `resolve`'s own precedence, made read-only
and narrowed to `NameRef::Func`; the walker asks `lookup` and then requires
`FuncVal::decl_name == name`. Those draw the same line, which is the point:

- a **local** or a **parameter** of that name shadows the function → both decline;
- a **global** holding a function value shadows it → both decline;
- an **alias** (`h = id`) is a global whose `decl_name` is `id`, not `h` → both decline.

A wrong-arity fallback reports the FUNCTION's arity, from the one `interp::arity_err`
both engines already use — so the sentence cannot drift.

### D4 — The call site carries the name its fallback resolves to

A method call site records the FREE FUNCTION its fallback would call, as that name
actually resolves — not as it is written.

The two are not the same name. `module::load` namespaces every top-level name the moment a
second file is involved: `fn where` is stored as `m0$where`, and references to it are
rewritten to match. A **method** name is not a top-level name and must stay as written, or
it would match no type's table. Resolving the fallback from the method name therefore
missed in every multi-file program — and `None` from that lookup is indistinguishable from
"this program declares no such function", so the call compiled to a plain dispatch and died
with the pre-UFCS error.

That put the feature in exactly the files that do not need it and left it out of every file
that does: **a library's consumer always has an import — that is what makes it a
consumer.** Half a fluent API kept working, which is what made it easy to miss: `limit`,
`offset`, `bump` — names no type owns — take the parse-time rewrite, which produces an
`Expr::Call` the loader already resolved correctly.

The loader fills the field with `Expr::Call`'s own precedence: a local binding shadows
outright, this module's own definition wins over an import, and a name that is neither is
left alone. Trailing defaults are NOT filled on this path, where a free call fills them —
the arguments belong to the method reading too, and a split call site emits both. A
selectively-imported verb with omitted defaults therefore works qualified and not in method
position; recorded here rather than left to differ silently.

### D5 — The checker mirrors the rule, so it cannot reject what runs

`synth_method`'s error path retries as `synth_call` under the same guards, using the
**hoisted** `fn` set (ADR 0027: a top-level `fn` is file-scoped, so `q.where(c)` above
`fn where` runs, and a reached-so-far set would reject exactly that program). Shadowing is
read from `env`, which already answers it.

## What this cost, stated plainly

- **The parser's desugars still win.** `sort_by`, `min_by`, `max_by`, `argmin`, `argmax`,
  `take_while`, `drop_while`, `zipmap`, `flat_map`, `count_where`, and `position` are
  rewritten at parse time into other method chains, before any receiver exists. A user's
  `fn sort_by` is therefore still unreachable in method position. These are not natural
  fluent-library verbs, which is why the line falls here rather than nowhere — but it is a
  line, and it is recorded in `docs/dx-plan.md` rather than left to be discovered.
- **Split call sites emit both readings**, so they are larger and decline fusion. They
  exist only where a program declares a `fn` named after a comprehension or a column verb.
- **Comprehension syntax against a non-iterable receiver** now reports "`it` is not
  defined here" (with the comprehension hint) instead of the method error, because the
  UFCS reading is the only one left. That is the more accurate message.

## Consequences

- A fluent library is writable in Helix. `q.where(...).select(...).all()` on a record is
  an ordinary program, and it chains across lines with no continuation characters.
- **A pre-existing engine divergence was found and fixed on the way.**
  `Op::DfColumnVerb` raised its own sentence for a non-DataFrame receiver ("expected a
  DataFrame, got Record") where the tree-walker raised the ordinary method error ("a
  Record has no method `select`") — reachable whenever the checker cannot pin the receiver
  down and the value turns out not to be a frame. It now calls the walker's own two
  constructors instead of restating them.
- **A whole class of program the corpus could not see.** `ufcs_receiver_decides.helix`
  passes verbatim and fails at line 28 with one `import` line prepended — and no corpus
  program has an import, so the corpus was blind to every multi-file program by
  construction. Found by a field build using the feature the way it was meant to be used:
  from a library. The regression test is now a MULTI-FILE one
  (`an_import_does_not_disable_ufcs_or_fn_main`), because a single-file test of a
  namespacing bug is a test that cannot fail.
- **The same root cause had a second symptom, filed and open since before this change.**
  `climain::find` matched `name == "main"`, so a program's `fn main` was silently disabled
  by one import too — it stopped taking command-line arguments, and ADR 0037 D6's refusal
  of an unbindable parameter stopped firing with it. Fixed alongside, by giving the lookup
  the entry module's prefix rather than letting it guess: matching any `*$main` would have
  let an imported library's `main` hijack the entry point.
- **A third route, found the same way as the first two.** `join`'s missing receiver test
  was found by a field build chaining a real query builder, not by any test here — and the
  test that now covers it CANNOT live beside the other UFCS parity tests, because those
  compile with no TypeMap, so `recv_type` is `None`, the route never fires, and the bug is
  invisible. It needs the whole pipeline, which means the CLI.
- **The guards were sabotaged before being trusted.** Compiling the receiver into both
  branches, and dropping `Missing` from `Iterable`, each turn the corpus program red —
  the first as an outright VM-vs-tree-walker divergence. The first version of the
  once-only check did NOT go red, because it watched only the branch that never received
  the duplicate; it watches both sides now. A guard that has never failed is a claim.

## Addendum 2026-09-02 — the last parse-time decision, removed

The Decision above ruled that a method call is resolved by its receiver at run time, and
the implementation left one exception standing: the parser still rewrote `x.f(a)` into
`f(x, a)` whenever `f` was a declared fn that no type owned. That was correct for every
program it could see and wrong for the one it could not — a record whose own field `f`
holds a function could never win against a free `fn f` — and it was blind to a PyObject
receiver, which the parser's own comment recorded as "the narrow residue".

That rewrite is gone. What replaced it, layer by layer:

- **The parser decides nothing.** Every method call is a method node.
- **After the checker** (`src/ufcs.rs`): where the receiver's type is PROVEN and rules the
  method reading out — not `Unknown`, not a `Record` (a field of that name may hold a
  function), not a frame, and not a type that owns the name as a real method — the call
  becomes the free call it is. The same rewrite, made where the receiver is actually known.
  This is what keeps the JIT fusing: its kernel analysis admits a `Call` and not a
  `Method`, and `range(0, n).map(it.f(1))` measured **25 → 108 ns per element** with the
  rewrite removed and nothing in its place. `helix jit-explain` now reports "1 kernel site
  offered, 1 compiled" for both spellings.
- **At run time**, both engines, same order: a real method of the receiver's type; else a
  function-valued field; else the declared fn with the receiver as its first argument. The
  VM decides by *peeking* at the receiver before dispatch — no failed dispatch, no error
  object built — scans a per-site list of the types that own the name (no hashing),
  compares field names as interned symbols (one `u32` per field, the way `Record` was
  designed), and enters through the same entry a direct call takes. Measured on one
  binary, interleaved min-of-15: Int receiver **0.976×**, Record receiver **1.021×** —
  `x.f(y)` costs what `f(x, y)` costs.

Precedence is fixed and pinned from both sides: `{keys: f}.keys()` is still the key list,
a field wins over a same-named free fn, and a shadowed name is not the fn. Tests:
`ufcs_is_decided_by_the_receiver_at_every_layer` and
`a_function_valued_field_is_callable_with_method_syntax`.

## Addendum 2026-09-04 — the order holds for the compiled families too

The Decision's order — a real method, then a function-valued field, then a free `fn` —
was implemented in the dynamic method op and reached by the compiled families (comprehension
verbs, frame verbs, `join`) only when a free `fn` of the name was declared: the receiver
split that hands a non-matching receiver to that op was taken on that condition alone. So
`q.count(1)` reached a record's field while `q.all(1)` did not, and `q.select(1)` was
refused by the VM and answered by the walker, whose own comprehension shortcut skipped the
field in a different way. A field build writing `User.all(db)` found it.

Every family now takes the split whether or not a fn is declared, and the walker consults
a function-valued field — on a record whose type does not own the name — before its
comprehension shortcut. One rule, stated once per engine, pinned by the corpus program
`rec_field_precedence` under both DataFrame backends.

The same rule reached the arguments the same day. `xs.map(double)` is read as
`(it) => double(it)` for the array reading, and that wrapper used to be handed to a
record's `map` field as well — a decision made before the receiver existed, again. The
synthesized lambda now carries the path it came from, and the readings that hand the
argument to a function value (the split's field branch, a free fn via UFCS) use the path.
The array reading is untouched, so the JIT fuses `xs.map(double)` exactly as before,
typed or through a parameter — pinned by `jit-explain` in the test.


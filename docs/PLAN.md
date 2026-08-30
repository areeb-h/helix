# The plan after v0.8.0

Everything open, sequenced. `docs/STATE.md` records what landed and the raw queue; this is
the order to do it in and **why that order**.

Two rules this plan follows:

- **Correctness before capability.** A wrong answer outranks a missing feature, always.
- **Nothing ships on a claim that was written before it was measured.** That failure has cost
  this project real time repeatedly — a documented "trades speed for schema-freedom" that was
  backwards, a stale server answering for a new binary, an `awk` misread that would have
  reported an index as free. Every phase below states what must be measured, not asserted.

---

## Landed since the v0.8.0 tag

On `main`, after the tagged commit. Each tested; each was wrong rather than missing.

- **A capability denial now names a grant that exists** (Phase 0.2). It offered
  `[capabilities]`, which the manifest parser refuses, and `--allow-…`, which has never
  existed — while omitting `HELIX_ALLOW_*`, the one that works. The test asserts the hint is
  *followable*, not merely present.
- **`helix` with no arguments and no terminal refuses**, instead of opening a session that
  can read nothing. This is what hung a field user's shell, and what turned a `strip`ped
  build artifact into an interactive session with **exit code 0** — "your program is gone"
  presenting as "your program printed nothing". Now exit 2, naming the stripped-bundle cause.
  Explicit `helix repl` fed by a pipe still works; only the implicit form refuses.
- **`helix build --runtime <path>`**, and a built program is refused as a runtime rather than
  nested.

### The binary-size finding, corrected

A field report measured a 120 MB artifact for a 187 KB program and I accepted "that is the
runtime". It is not.

| runtime | size |
|---|--:|
| gate profile, all features + debug info | 120 MB (2.4 GB in plain debug) |
| release, all features | ~63 MB |
| **release, `--no-default-features`** | **6.7 MB** |
| the Go binary serving the same page, for scale | 12 MB |

**A Helix web server fits in 6.7 MB — smaller than the Go binary it was being compared
against.** Verified that the minimal runtime is not a toy: string interpolation, records,
`map`/`filter`/`reduce`, `read_text`/`write_to`, `html_escape`, `Bytes`, the storage verbs,
**and it serves HTTP**. `regex` and `DataFrame` refuse with "this build has no X support",
which is ADR 0032's gate-the-body pattern working.

The bug was never size: `helix build` copied whatever binary was invoked and gave no way to
say otherwise. `--runtime` takes the same program from 2.4 GB to 6.7 MB with byte-identical
output.

---

## What will bite us later

Measured or verified, not speculated. Each of these gets worse with time rather than better,
which is why they are listed before the phases rather than after.

### The differential oracle is vacuous for new features

`vmparity` and the corpus check that three engines agree. **For a storage or `Bytes`
program, `jit-explain` reports `0 kernel sites offered to the JIT, 0 compiled`** — all three
engines take the same interpreted path, so agreement proves the code is deterministic, not
that it is right.

This project's most-trusted guard therefore has coverage **inversely related to how new a
feature is**: everything gets interpreted first, and the JIT only ever learns numeric shapes.
Everything added in 0.8.0 — nine storage verbs, locks, `Bytes`, three frame verbs — is
outside it.

**What follows for how we work.** For a shared-path builtin the real checks are the corpus
golden and the hand-authored claims program, and saying "vmparity is green" about such a
feature is saying nothing. Where a feature *does* have engine-specific paths (a packed array
fast path, a JIT kernel), the oracle is the point and should be cited. Do not let the phrase
"gate green" flatten those two cases into one.

#### A second, worse case: SHARED code, fully exercised, still unseeable

Vacuity is the easy half — nothing runs, so nothing is compared. The harder half is a path
that all three engines *do* execute and the oracle still cannot check, because they execute
**the same function**.

The `select` / `sort` / `group` shadowing bug was exactly this. `arg_as_column_name` lives in
`dataframe_ops.rs`, which the walker and the VM share **deliberately**, so that they cannot
diverge. Three engines running one wrong function agree perfectly, at exit 0, and the bug
survived every differential run this project has. The codebase had already written the
epitaph for the earlier instance of it, in `backend/mod.rs`: *"exit 0, `helix check` ok, and
all three engines agree because all three are equally wrong."*

#### Make verification evidence explicit

**Engine agreement is not a scalar guarantee.** "walker ✓ VM ✓ JIT ✓" is read as *three
implementations corroborated this*, and sometimes the truth is *three entry points called the
same faulty function*. Those are worlds apart, and today they print the same green.

Five levels, strongest first, which must never be presented as equivalent:

| level | evidence | example |
|---|---|---|
| **independent parity** | engines reach the answer through materially different code | a JIT kernel vs its bytecode fallback |
| **backend differential** | polars vs native-df — *only if the operation forks before them* | frame arithmetic, sort stability |
| **shared-semantic parity** | engines differ, then meet in shared machinery | every column verb, via `dataframe_ops.rs` |
| **vacuous parity** | no engine-distinct path exists at all | storage, `Bytes` — `0 kernel sites` |
| **external expectation** | an answer authored by a human, not produced by the code | corpus goldens, the release claims |

The last row is not the weakest. For anything at level 3 or below it is **the only real
evidence**, which is why it belongs at the top of a feature's test plan rather than the bottom.

#### The rule this yields

> **A component cannot be evidence for semantics that it defines.**

Same principle as refusing to bless a golden from the implementation that produced it. It has
a sharp practical consequence: when a semantic operation lives *above* the engine fork,
engine parity is not its primary test, and neither is the backend differential — the primary
test has to originate outside the shared implementation.

#### The measured case

The `select` / `sort` / `group` shadowing bug is the permanent fixture for this class, and it
was invisible to **both** differential mechanisms at once:

- **Engine parity: blind.** `arg_as_column_name` lives in `dataframe_ops.rs`, shared by the
  walker and the VM *deliberately* so they cannot diverge. Three engines, one wrong function.
- **Backend differential: blind.** `column_name_args` computes the names and only then calls
  `lf.select(&names)`, so the fork happens *after* the mistake — polars and native-df receive
  identical wrong names and agree.
- **Corpus golden: caught it.** A human wrote the expected column down.

The codebase had already written the epitaph for the earlier instance, in `backend/mod.rs`:
*"exit 0, `helix check` ok, and all three engines agree because all three are equally wrong."*

Sharing the implementation remains right — two hand-written column resolvers would drift —
but what it buys is *no divergence*, not *no bug*.

**Worth building**, roughly in order of value per effort:

1. `helix test --engines --explain-oracle`, reporting which level each program reached.
   `jit-explain` already knows the kernel-site count, so vacuous parity is mechanically
   detectable today; shared-semantic needs a note per verb, or a call-graph pass.
2. **Mutation testing at the semantic chokepoints.** Perturb a shared helper — return the
   wrong column, invert a predicate, flip null handling — and require that *some* test fails.
   A surviving mutant is an evidence hole, named precisely. `arg_as_column_name` is exactly
   the shape this finds, and it would have found it.
3. **Property/metamorphic tests** where no second implementation exists: `decode(encode(b))
   == b`, `sort(sort(x)) == sort(x)`, `df.slice(0, n).count() <= n`. These are independent of
   the implementation without needing a rival one, which is what levels 3–4 lack.

#### Related: expose the transitive effect closure

`capability::effect_of` already classifies every builtin, and
`no_ungated_effectful_builtins` already forces that classification exhaustively — it walks
`BUILTINS`, skips those the registry marks `pure`, skips the gated ones, and requires anything
left to sit in a `harmless` allowlist with a written justification. **The bottom layer exists
and is guarded.**

What is missing is propagation. Closing effects over the call graph and reporting them from
`helix describe --json` would give:

    report
    effects: fs.read (via read_csv), clock (via now)
    deterministic: no — clock, through report -> now

One piece of information, several features: capability auditing, reproducibility reasoning,
caching eligibility, deployment manifests, and agent introspection. It is not a Koka-sized
effect system; it propagates what Helix already knows.

**Do not fold this into `check`.** That contract — *never rejects a runnable program* — is
what makes the edit/run loop usable, and a grumpier type checker is not the ask. A stricter
facility should answer a *different question* (`helix effects`, or a `verify` with explicitly
selected properties), not the same question more harshly.

### `Type::Unknown` grows every time a type is added

`Dict`, `Net`, `Bytes`, `Lock` are all `Unknown` to the checker. Each addition makes the
checker weaker, and the pressure runs the wrong way: adding a variant is a large change,
typing it `Unknown` is a one-liner, and the cost lands on users much later.

**The policy that stops the drift:** a new runtime type gets a writable name in the SAME
change that introduces it. `Bytes` shipped without one (ADR 0042, deliberately) — that is
the last one that should.

### There is no deprecation mechanism, and Phase 0.1 needs one

`select`/`sort`/`group` resolving a bare name to the column rather than the binding is
**wrong and breaking to fix**. With no way to warn first, the only options are break silently
in a minor or never fix it. Neither is acceptable for a change whose current behaviour
returns the wrong column with no error.

**This should land BEFORE Phase 0.1**, not after: a warning path (`helix check` naming the
sites, the runtime warning once per program) lets the fix arrive as a deprecation and then a
change, rather than as a surprise. It is also the thing every later semantic fix will need —
Phase 5.3 and any capability tightening included.

### The manifest is on a compatibility treadmill

`Manifest` is `#[serde(deny_unknown_fields)]`, deliberately — a silently discarded
`[capabilities]` block once looked like it restricted authority and did nothing. The
consequence is that **every new manifest key is a hard error on every older binary**.
`[workspace]` already forces `helix = ">=0.8.0"`; `[capabilities]` will force `>=0.9.0`.

Refusing loudly is right. Refusing with "unknown field `capabilities`" is not, because the
reader cannot tell "malformed" from "newer than your binary". The unknown-key error now names
the running version, which helps — but a manifest cannot yet say *"this key needs 0.9"* and
have an old binary explain itself. Worth designing before there are five such keys.

### The printed format is frozen, and `Bytes` prints without bound

`b"<hex>"` in full, never truncated — a decision I made and shipped in 0.8.0. A one-megabyte
`Bytes` prints two megabytes of hex. The reasoning stands (an elision hides exactly the byte
a reader is hunting, and printed output is a frozen format so a `…` could not be removed
later) but the consequence is real and is now a versioned event to change.

If it needs revisiting, the change is a **minor**, and the honest framing is that this was
chosen for legibility over safety rather than that the risk was unforeseen.

### Improving an error message costs more every release

Changing one sentence — the `sort`/`min`/`max` domain wording — required updating **41
pinned copies**, 37 of them in `tests/ordering_matrix.rs`. That friction grows with the test
suite, and it pushes toward leaving bad messages alone.

The pins are right (a message is a contract, and this project treats diagnostics as a
feature). What is missing is a single place to state a message so a test can reference it
rather than transcribe it.

### `run` is an authority escape hatch, and that is inherent

Granting `process` grants whatever the child can reach — the filesystem and network you just
declined. ADR 0037 D3 says so. It cannot be fixed inside Helix, only named: a program that
declares `process = "on"` has, in practice, declared everything.

**Do not let `[capabilities]` imply otherwise.** The documentation for `process` should say
plainly that it is a boundary exit, not a confinement.

### Corpus goldens accumulate

129 dfdiff programs, 89 checked files, and every one is a pin. That is the point — but a
legitimate semantic change now means reading a large golden diff and deciding, program by
program, which changes were intended. The corpus README already says "read the diff"; at some
size that stops being advice and starts being a full day.

No action yet. Worth watching, and worth preferring one program that pins a behaviour
precisely over three that pin it incidentally.

---

## Phase 0 — the two things that are wrong right now

Both are small. Both are wrong in the "looks fine, isn't" way, which is the worst kind to
leave.

### 0.1 `select` / `sort` / `group` silently take the column over the binding

ADR 0028 decided a binding in scope beats a same-named column, for `where`/`filter`/`with`.
It did not cover the column-name positions, and the bug is there — **including a silent
form.** With `w = "v"` over a frame that also has a column `w`:

| written | answers | should be |
|---|---|---|
| `D.sort(w)` | sorted by **w** | by `v` |
| `D.select(w)` | `["w"]` | `["v"]` |
| `D.group(w).sum(@n)` | 3 groups | 2 |

So `fn top(frame, key) = frame.select(key)` returns the wrong column on any frame with a
column called `key`. No error.

**This needs the deprecation path first** (see "What will bite us later"): the current
behaviour returns the wrong column with no error, so shipping the fix as a silent semantic
change trades one quiet wrong answer for another.

Three questions, to be decided **together** — deciding them apart is how a language grows a
fourth rule nobody can predict:

1. Does a binding win here, as in 0028? Consistent, and **breaking**.
2. Should a String be accepted (`df.sort("v")`), given `df.column("v")` already takes one?
   Not breaking.
3. Why does a bare ident resolve differently inside a function than at top level? That looks
   like an inconsistency, not a decision.

Pinned by `tests/corpus/df_column_name_shadowing.helix`, whose golden is **expected** to
change here.

### 0.2 The capability denial names two grants, neither of which exists

    capability denied: `write_to` needs `fs-write` authority, which is not granted
    help: grant it in `[capabilities]` in helix.toml, or run with the matching `--allow-…`

`[capabilities]` is refused by the manifest parser. No `--allow-*` flag exists. The only
working mechanism — `HELIX_ALLOW_FS` and friends — is not mentioned. A user who turns the
sandbox on has **no path forward from the message**, which on a security surface teaches
people to turn it off.

The hint fix is minutes and does not wait for Phase 2.

---

## Phase 1 — shipping: multi-module `helix build`

**Why first among the features:** it is the only thing that makes a Helix program
deliverable. Any program with a library cannot be bundled at all, and the workaround —
inlining by hand — pushed a field user into **reimplementing Helix's lexer**, where they got
the `{{` doubling convention wrong and desynchronised 15 KB into one token. Nobody should
have to rebuild the front end to ship a program.

The blocker is shallow, which is the good news. The overlay appended to the runtime is
`[name][source][name_len][src_len][MAGIC]` — **one** source string. Nothing in the design
requires that; `module::load` already collects every module's source in `loaded.spans`.

**The change.** Make the overlay a small archive of `(logical path, source)` pairs, and give
the loader a source provider that resolves imports from that map instead of the filesystem.
Same resolution rules, different backing store — so a bundled program and an interpreted one
cannot diverge on how imports resolve.

#### The design, settled: a flat relative-path archive

The open question was the KEY. `load_file` touches the filesystem in exactly two places --
`path.canonicalize()` and `fs::read_to_string(&canon)` -- so the seam is clean, but canonical
paths are absolute on the BUILD machine and meaningless on the target.

Resolution tries the importing file's own directory, then package dependencies, then the
project root and `HELIX_PATH` / `<exe_dir>`, and takes the first hit. So the key must be a
path **relative to the project root, preserving directory structure** -- not a flat name.
At run time the virtual project root is `""`, so every join reproduces the same key:

    archive: { "sub/main.helix": …, "sub/util.helix": …, "util.helix": …, "std/json.helix": … }

Sibling imports then fall out for free: `sub/main.helix` importing `util` joins its own
virtual directory to `sub/util.helix`, distinct from a root-level `util.helix`. A bundled
program and an interpreted one run the SAME resolver over a different backing store, which is
the property worth having -- not "two implementations that agree today".

**Collisions are possible, and must be refused rather than reasoned away.** An earlier draft
of this note claimed the build "already resolved them, so they cannot collide". That is true
of any single import and false of the key space: a package dependency beats the project root
in the ladder, so a dep module resolving `mathlib/go.helix` and a project file at
`<root>/mathlib/go.helix` reached from elsewhere are two different files with one key. Rare,
but silently shipping whichever landed last is exactly the class of bug this project refuses.
`helix build` detects a duplicate key and fails, naming both real paths.

**Overlay v2.** `[payload][payload_len u64][MAGIC]`, payload being a length-prefixed list of
`(path, source)` plus the entry index. The reader keeps accepting `HLXBND01` (one source, no
imports): `--runtime` lets a bundle be built against a DIFFERENT helix binary, so a v1
runtime can be handed a v2 overlay and should say so rather than misparse it.

**What this deletes.** The `multi_module` refusal in `bundle::build`, and with it the reason
a field user reimplemented Helix's lexer to inline modules by hand.

### 1.2 `helix build` should say which runtime a program needs

`--runtime` makes the size a choice; it does not make it an INFORMED one. Today you must
already know that your program touches no DataFrame, no genomics reader and no JIT kernel —
and the only way to find out is to build with a smaller runtime and see what breaks at run
time.

The information is already in the tree. `helix build` walks the AST, and every gated feature
already refuses by name at run time (`this build has no regex support`). What is missing is
the static half: a `feature_of(builtin_or_method) -> Option<&str>` table and a pass that
reports it.

    $ helix build site.helix -o site
    built site (6.7 MB)
    uses: fs, net
    unused, and linkable out: dataframes, bio, jit, regex, http
      a runtime built --no-default-features would serve this program

That turns "which runtime?" from guesswork into a line of output, and it composes with 1.1 —
a bundled multi-module program can be reported the same way.

Note what it must NOT do: pick the runtime automatically. The build has one binary to copy
and cannot produce a smaller one; guessing and silently substituting would be worse than
saying nothing.

#### Landed

`helix build` now prints what a program reaches:

    built standalone executable: prog (6.7 MB)
    needs: http, regex

or, when nothing optional is touched:

    needs no optional feature — a runtime built `--no-default-features` would serve this program
      (that also drops jit and mimalloc, which change speed, not answers)

The second line exists because the first is true and, alone, reads as "costs nothing".

**The classification was measured, not reasoned about** — each name run against a
`--no-default-features` runtime and classified by whether it answered "this build has
no …". Guessing got four wrong, recorded in `registry::feature_of`:

| looks like | actually |
|---|---|
| `read_bed`, `dna`, `align` are genomics | need no feature |
| `read_json` is a DataFrame reader | returns Helix values |
| `listen` needs `http` | **`http` gates the CLIENT** — a server needs nothing, which is why the 6.7 MB runtime serves HTTP |
| `re_replace` is ungated | the probe called it with the wrong arity, and the arity error masked the gate |

That last row is the shape to watch when re-deriving the table: a name that fails for its
own reasons before reaching the gate reads as "available".

`every_builtin_declares_its_feature` pins the exact gated set, so a new builtin cannot be
added without deciding which runtime it needs. The pass itself walks `visit::walk_stmt`,
whose exhaustive match fails compilation when an `Expr` variant is added, rather than
silently skipping it. It can over-report (a user function named `read_csv` counts), which
is the safe direction: over-reporting leaves someone on a runtime that works.

#### Startup, measured — and `--runtime` is a latency argument too

Min of 20 runs, both runtimes built at the same commit so only features differ:

| | |
|---|--:|
| `/bin/true` (bare process spawn) | 1 ms |
| **helix, minimal runtime (6.7 MB)** | **2 ms** |
| **helix, full runtime (62.5 MB)** | **4 ms** |
| `python3 -c 'print("hi")'` | 12 ms |
| `node -e 'console.log(1)'` | 20 ms |

Two things follow.

**There is nothing left to win on startup.** 2 ms is one millisecond above a bare process
spawn, and 6x faster than CPython. Any future "make Helix start faster" work would be
optimising the kernel's `execve`.

**The big runtime costs 2 ms more to start — double.** `--runtime` was justified on disk
size; it is also a latency argument, and that is the one that matters for a tool invoked in
a loop from a shell script. Worth saying in the docs beside the size number, which is the
one everybody quotes.

### 1.4 `--runtime` takes any file, and says nothing

Found while making 1.2's test fast: `--runtime` copies whatever it is handed. A stub of
39 bytes builds "successfully" and produces an artifact that cannot run — which is what
makes the test 0.11s instead of 272s, and is also a trap for anyone who mistypes a path.

The check has an obvious shape: a real runtime contains the overlay magic as a constant
in its own `.rodata`, since it carries the reader. Grepping the candidate for `HLXBND0`
distinguishes a helix binary from `/etc/passwd` without executing it. Not done yet, and
the test above would need its stub to carry those bytes.

### 1.3 `build` does not build, and the name says otherwise

187,282 bytes marginal against a 187,293-byte bundle — **eleven bytes apart**, with this
repo's comments recoverable by `strings`. It embeds SOURCE, resolves modules, and appends.
Measured effect on throughput: 2,628 against 2,489 req/s, and 68 against 65 ms to first
response. Both inside noise, which is exactly right for the same interpreter running the same
program having skipped only module resolution.

That is a genuinely useful thing — one file to copy, nothing installed — and it is not what
"build" leads a reader to expect. Either the docs state it plainly at the top, or the verb
changes. `helix emit-hbc` is the path that really compiles, and its v0 core is
`Int`/`Float`/`Bool` arithmetic, frame locals, `if`/`while` and direct/tail calls — it refuses
arrays and indexing, so a web application is far outside it.

**Must be measured, not assumed:**

- A bundled multi-module program produces byte-identical output to the interpreted one, on
  all three engines. This is the differential oracle applied to the bundler.
- The artifact is genuinely self-contained (no `helix` on PATH) — already true for single
  files, must stay true.
- Size. A field measurement puts the runtime at 51 MB installed / 63 MB release / 120 MB
  gate, with the program itself marginal. Report the release number, not the gate one.

---

## Phase 2 — the manifest says what a program may do

This is the security work, and it is one feature in three parts.

### 2.1 `[capabilities]`, as a CEILING

```toml
[capabilities]
fs = "read"        # omitted = none | "read" | "write" | "all"
net = "on"
process = "on"
```

Three properties, each against a specific failure:

- **Present means enforced.** No environment variable needed. A declaration that only takes
  effect when someone remembers to set a variable is not a declaration.
- **The environment narrows, never widens.** `HELIX_ALLOW_FS=all` against `fs = "read"` gives
  read. This is what makes the file worth reading: it states the **most** the program can do,
  on any machine, under any deployment.
- **Absent means today's behaviour.** Non-breaking; an invitation, not a migration.

Explicitly **not** default-deny-with-no-block: every existing program breaks, and security
that makes a tool unusable gets turned off wholesale.

#### Landed ✅

All three properties are pinned by `a_declared_capability_ceiling_enforces_itself`, including
the negative that proves the first one: a table declaring `net` and saying nothing about `fs`
denies both filesystem effects with **no environment variable anywhere**. Without that case
the table could be doing nothing, which is precisely the failure `deny_unknown_fields` was
added to prevent.

Three decisions worth recording, because each closes a way the ceiling could have leaked:

- **`HELIX_CAP=audit` cannot weaken a declared ceiling.** Audit *allows* the access it logs,
  so honouring it would let the environment widen authority by spelling a mode. A manifest
  with capabilities is always `Enforce`.
- **Combining happens at read time**, in `capability::current`, rather than by rewriting the
  installed authority. The environment is knowable at startup and the manifest only after the
  entry resolves; keeping the install single-write means there is no authority that can be
  *replaced* mid-run, which is a thing an attacker would look for.
- **A dependency's `[capabilities]` is not consulted.** A library cannot grant itself
  authority the importing program did not declare — that is the whole point of a ceiling. The
  reverse, a dependency NARROWING the program, is a real idea and a different feature
  (per-evaluation attenuation, ADR 0021).

The gate caught the one thing worth catching: `an_unknown_manifest_key_is_rejected_rather_
than_silently_dropped` used `[capabilities]` as its specimen of a refused unknown key, so it
failed by succeeding. Its rule survives with a different example, plus the case that now
matters more — a misspelled grant (`fs_read = "on"`) must be refused rather than read as an
absent one, which would be the original failure wearing a new hat.

**Still open here:** the bundle does not carry its declared ceiling (2.3's second half). A
bundled program loads through `load_archive`, which has no manifest, so `helix build` must
bake the grants into the overlay — otherwise "ship to production" and "declared authority"
remain two features instead of one.

### 2.2 The manifest is honest about identity

- **`package.name` is unvalidated** and used *only in tests*. A package can name itself
  `my-package`, which `validate_dep_name` will refuse on the consumer's side — so the error
  lands on the wrong person. Validate it with the same identifier rule, at authoring time.
- **`helix add ui --path ./web` is not cross-checked.** The dependency key and the target's
  declared name are unrelated, and `import ui.x` resolves through the key — so a mislabelled
  dependency silently imports a different package. Read the target manifest, require
  agreement, name both on mismatch.

### 2.3 Two guarantees that are currently accidents

- **Installing a package executes no code.** ✅ Guarded by
  `installing_a_package_executes_no_code`, which scans `pkg.rs` and `module.rs` — the files
  a dependency travels through — for every way Rust starts a process.

  Unlike the unwrap budget it models, this one has **no raise path**: there is no version of
  "installing a package runs a little code" that keeps the property. The change that would
  have broken it ("shell out to `tar`, it is faster") is the kind that looks entirely
  reasonable in review, which is the whole reason it needed a guard rather than a comment.

  Verified to FAIL, not just to pass: injecting a `Command::new` into `pkg.rs` makes it
  report `src/pkg.rs:1568: Command::new`. A guard that cannot fail is worth nothing, and
  this session already produced one of those.

  The scan is textual and so defeatable by someone determined. That is the right target —
  it is aimed at the accident, not the adversary, the same job `#[deny]` does.
- **The bundle carries the declared capabilities** (needs 2.1 and Phase 1). A shipped binary
  should enforce what its manifest declared, or "ship to production" and "declared authority"
  remain two features instead of one.

---

## Phase 3 — the type system can name its own values

The enforcement is **good** and I under-rated it: parameter annotations are checked at the
call, inside the body, against unknown type names, and the types flow onward. `fn f(x: Int) =
x` then `f(1).upper()` is caught.

The problem is vocabulary.

| | |
|---|---|
| writable | `Int` `Float` `Num` `String` `Bool` `Array` `Tensor` `DataFrame` `Dna` |
| **not writable** | `Dict` `Bytes` `Tuple` `Record` `GroupBy` `Net` `Lock` `Unit` `Missing` `Function` |

`Tuple` and `Record` are core constructs. `Dict` has eleven methods and a doc page. So on a
lot of real code, annotating is not a choice the author declined — it is **a sentence they
cannot write**, and unannotated means `Unknown` means unchecked.

1. **Name the ten missing types.** Largest coverage win per unit of risk.
2. **`Array[Int]`.** `Type::Array(Box<Type>)` already exists and is used internally —
   `columns()` is typed `Array(String)`. Only the surface syntax is missing. The checker
   already knows more than it can say.
3. **Return annotations.** `fn f(x): Int` is a parse error; inputs can be constrained and
   outputs cannot.
4. **A registry ↔ checker drift guard.** "What methods exist" is declared in three
   independent places — `registry.rs`, `types/signatures.rs`, and the runtime dispatch — and
   only the first is tied to anything (the docs guard). That drift produced **two**
   self-contradicting errors in one day:

       type String has no method `to_bytes` — did you mean `to_bytes`?
       a DataFrame has no method `sort`    — did you mean `sort`?

   The hint reads the registry; the denial comes from the checker. A guard asserting every
   registry method has a checker type would have caught both at the gate.

**Note for whoever does this:** `Bytes` was typed `Unknown` deliberately in ADR 0042 to land
the feature without a large checker change, so `from_hex("00") + 1` currently passes `check`.
That was a real cost, not a footnote — it belongs on the list above, not in a caveat.

---

## Phase 4 — the remaining performance cliffs

### 4.1 The dict cliff — the worst one in the language

**71 seconds at n=128,000**, 16.6× per 4× the input. `Dict::insert` clones the whole
`BTreeMap` per call and the take-append-store rescues only the bare-local spelling.

The array fix does not transfer unchanged: an append-only buffer works because an array
appends at the END, so a value's prefix is settled forever. A `BTreeMap` insert lands
anywhere and can overwrite.

Design and its trap are written up in `docs/STATE.md`; the short version is that `count()` is
**not** `base.len() + adds.len()`, because an add that overwrites contributes nothing. Getting
that wrong is a wrong answer, not a slow one, so `dfdiff` + `vmparity` + the corpus must be
pointed at it.

#### Reproduced, with both shapes asserted

| shape | n=2,000 | n=8,000 | n=32,000 | per 4x |
|---|--:|--:|--:|--:|
| accumulator IS the dict | 0.00 s | 0.00 s | 0.00 s | flat |
| dict in a **record field** | 0.02 s | 0.31 s | 4.79 s | **x15.5** |

x15.5 against the 16 of a true quadratic, extrapolating to ~74 s at n=128,000 — the
lint's own figure. Both rows assert the printed `count()` before timing: the first attempt
at the flat row measured a program with a **syntax error**, which times at 0.00 s and reads
as a triumph.

The lint fires on the quadratic shape today and names the measurement and the workaround, so
ADR 0026 is satisfied on the diagnostic side; what is missing is the fix.

#### A second trap, beside `count()`

The sketch above says reads "merge base + adds once and cache". `count()` is the trap it
names; **`get(k)` is a second one**, and it is not the same shape.

`get` needs the LAST occurrence of `k` in `adds[..len]`, not the first. A `key -> first
index` side table — the obvious way to make `count()` O(1), since a key's first index never
changes under append-only — answers the wrong question here, and scanning `adds[..len]`
backwards is O(len), which restores the cliff on any fold that reads as it goes.

The resolution is that caching cannot be per-chain, because each value has its own `len`:
one shared cache would answer for the wrong prefix. It can be per-TIP, which is enough,
because the fold pattern only ever reads the tip; a read of an older view materialises
without caching, and those are rare by construction.

#### A third candidate, narrower than both

`Op::InsertIntoLocal` already rescues `acc.insert(k, v)` when the accumulator IS the
receiver. The record-field shape could be rescued the same way — take the record out of the
accumulator slot, take the field out of the record so its `Rc` is unique, insert in place,
rebuild.

That is much smaller than either general design and much easier to get subtly wrong. The
guard directly above the existing pattern documents what that costs: matching the receiver
by name alone made `[[1], [2]].reduce([], (a, a) => a.concat([9]))` answer `[9, 9]` on VM and
JIT against the walker's `[2, 9]` — a silent three-engine divergence at exit 0. A record-field
version needs the same care about which fields may observe the old value, and it rescues one
syntactic spelling rather than the operation.

### 4.2 String accumulation in a record field

10.1× per 4×, 243 ms at n=128,000. Same shape, milder. No lint names it — there is no
`dict()`-like literal to key on, so detecting it needs its own rule. Node rendering *is*
string building, so it matters for the UI library specifically.

---

## Phase 5 — the field proposals

### 5.1 §1.7g — a histogram, spelled the obvious way

`count(@col)`'s own documentation calls it "a histogram of a categorical column", and both
natural spellings are refused. The working form carries a **decoy column purely to be
aggregated**, with a comment longer than the code.

Mechanism found: `group_agg(keys, agg, value_col)` emits the aggregate under the *value
column's* name, so counting a key produces it twice — surfacing as a polars duplicate-schema
error **at the start of the statement**, not at the `count(@k)` that caused it.

Fix: `group_count(keys)` on the ADR 0012 seam, emitting under its own name. `group(@k).count()`
with no argument then means what the no-arg form already almost says.

### 5.2 §1.7f — a frame that can report its own types

`df.schema()` is easy and additive. But the general observation is the better fix:
*Helix is good at computing over values and has almost no way to ask about their types in
bulk.* `type_of` is exact for one value; any question about a column costs a call per
element — 41.8 ms per 500k rows, which a field project replaced with `type_of(vals.sum())` at
349× and a paragraph of proof.

Do the general form: an element-type query over an array, answering one type or `missing` for
mixed. `df.schema()` follows from it.

### 5.3 §1.7h — a library cannot hold a read cache

The remaining 4.8× on point lookups is two `read_at` syscalls at ~0.005 ms against SQLite's
whole 0.004 ms answer. Not closable by making fewer syscalls; closable only by not repeating
the read.

A function may **read** a mutable global but not **write** one, so a cache — a function that
reads, misses, and writes — cannot be written in a library.

**This needs an ADR, not a patch.** Letting a function write a mutable global changes what
"pure" means, and the three-engine differential oracle rests on that. The narrower form — a
`.cache()`-style opt-in on a file handle, following `DataFrame.cache()` as precedent — needs
an answer to "what invalidates it" that the language can enforce rather than the caller
promise. The argument *for* it is sound and worth taking seriously: a file's contents are
immutable for the lifetime of a content-addressed store, which is the same reasoning
`docs/caching-and-memory.md` already accepts for values, one level out.

---

## Phase 6 — guards that let something through

- **`release.sh`** ✅ now asks the SOURCE whether the language grew, not the prose. The
  `### Changed` check reads the notes; a language-surface addition is invisible to it, and in
  v0.8.0 it fired only because `html_escape` happened to alter `to_html`'s bytes and supply
  the one heading. Without that entry, a release adding a `[workspace]` table, seventeen
  builtins and a new `Value` type would have shipped as a patch.

  `registry.rs` holds `BUILTINS` and every `*_METHODS` table — the names a program can write
  — so the set is compared against the last tag. Verified against real history rather than a
  synthetic case: **v0.7.0 exposed 307 names and v0.8.0 exposed 331**, so a patch would have
  been refused on the merits, naming all 22 additions. HEAD is also 331, which is right: this
  cycle added a manifest table and changed behaviour but no builtins, so `### Changed` is the
  guard that makes it a minor. The two agree.

  A pure rename keeps the count level, which is correct here — that is a breaking change
  rather than an addition, and it cannot avoid a `### Changed` entry.

- **The panic ratchet** ✅ fixed. It skipped every line starting with `*` to ignore comment
  continuations, and `*` also opens a DEREFERENCE, so `*stack.last_mut().expect(…) = …` was
  never counted.

  **This note said `vm.rs` has two uncounted. It had four, and the tree had nine**: `vm.rs`
  +4, `array.rs` +2, `autodiff.rs` +2, `serve.rs` +1. Each was checked and each is provably
  safe, with the proof now at its site — `empty_guard` returns first, `stack.last()` already
  succeeded, and the rest are `RefCell` borrows on cells those files already budget. Budgets
  raised to the true count, with the reason on the list so the diff does not read as nine new
  panics.

---

## Phase 7 — syntax, and what the examples teach

Prompted by a reader's verdict: *"it looks ugly and it was difficult to understand."* Worth
taking seriously, and worth measuring before agreeing. Probing the actual constructs moved
most of the blame off the language.

### 7.1 The examples look more ceremonial than the language requires

**A claim I put here first was wrong, and the correction is the useful part.** I wrote that
the examples teach nested `if/then/else do` ladders where `match` would be flat, generalising
from `examples/api/event_server.helix`. Measured across all of `examples/`: **three `else if`
in total, no file with a ladder, and four files already using `match`.** The examples do not
have that problem. (Written into a plan before being measured — the same failure this
document opens by warning about.)

What IS there, in the flagship server example:

```helix
ready = l.wait(conns, 50)          # `ready` is never used again
...
sent = c.respond(handle(req))      # `sent` is never used again
```

A bare expression statement is legal in a `do` block. These two bindings exist only to hold a
side effect's result, and inventing a name for a value nobody reads makes the language look
like it demands ceremony it does not. In the same twenty lines `do` appears **three times**
— two of which 7.2 removes outright.

So the fix is smaller and more specific than "rewrite the examples": drop bindings that are
never read, and let 7.2 take the rest. Still cheap, still first, but it is an editing pass on
a handful of lines rather than a sweep.

**Where `match` with guards genuinely is the better form** — a ladder on one subject — the
examples already use it. Worth teaching in the docs where it is not yet shown, not worth a
migration:

```helix
fn handle(req) = match req {
  r if r.is_missing()  => {status: 400},
  r if r.path == ""    => {status: 404},
  r                    => {status: 200, path: r.path}
}
```

Flat, aligned, exhaustive, and an expression — better than early-return in TypeScript or
Python, which is worth saying to a reader who arrives expecting `return`.

### 7.2 `fn f(x) { … }` — the one genuine wart

`fn f(x) { … }` does not parse; every block-bodied function must write `= do { … }`. Five
characters and a concept, on the most-repeated construct in the language. Nothing else
probed is noise on this scale.

The fix keeps the distinction rather than creating two spellings of one thing:

- `fn f(x) = expr` — expression body, unchanged
- `fn f(x) { … }` — block body
- `do { … }` — a block USED AS AN EXPRESSION, unchanged and valid anywhere an expression
  goes; function bodies simply stop being the one place it is mandatory

Parser-only. No semantics move.

### 7.3 `@col` and `"col"` are two spellings of "name a column"

`df.sort(@v)` but `df.column("v")`. The same seam that makes a runtime column name
impossible (Phase 0.1), so decide it there rather than separately — an inconsistency fixed
in two places at two times becomes a third rule.

### 7.4 A pipeline operator — an addition, not a fix

I expected to recommend `|>` and the evidence weakened it. Method chaining with leading-dot
continuation already covers most of what a pipeline is for:

```helix
range(0, 4)
  .map(it * 2)
  .sum()
```

`|>` wins only for FREE functions — `req |> validate |> authorize` — where the alternative is
nesting that reads inside-out. Neither TypeScript nor Python has it, so it would be a
differentiator; it is also the first thing on this list that adds a concept rather than
removing one. Below 7.1–7.3, deliberately.

### 7.5 Units and dimensions — the actual scientific differentiator

Everything above is refinement. This is the one thing that would make "scientifically
superior" something a reader can point at.

`9.8 m/s^2`, with dimension mismatches caught at check time. Neither Python nor TypeScript
has it; it is exactly where scientific bugs hide (Mars Climate Orbiter is the famous one, and
every lab has a smaller version); and the machinery is closer than it looks — the type checker
already enforces annotations where they exist, and `Rational` already shows the project will
carry an exact numeric type for correctness reasons.

Large, and an ADR in its own right. Listed here so it is a decision rather than an omission.

### What is already ahead of TypeScript and Python, and should be said out loud

Not aspiration — these are shipped and measurable:

- **`missing` propagates** (ADR 0001) rather than silently coercing. No `None` arithmetic
  surprises, no pandas NaN ambiguity.
- **`sum`/`mean` are Neumaier-compensated by default.** NumPy's are not — so Helix is *more
  accurate out of the box than the reference scientific stack*, on the most common operation
  in scientific code.
- **Exact rationals**, and `/` that does not silently truncate.
- **Three engines held bit-identical** — a correctness instrument no other language has.
- **Named arguments, out of order, at every call site** (ADR 0037).
- **Installing a package cannot execute code** — unlike pip, npm and cargo.

A reader who finds the syntax unfamiliar is not wrong to; a reader who concludes the language
is unserious has been shown the wrong examples. 7.1 addresses that directly.

---

## Phase 8 — the throughput gap, and what to measure before touching it

A field benchmark, every server rendering a byte-identical 4.1 KB page (`<tbody>` sha
`660543d45b92c86c` from all six), 4,000 requests, 8 connections, one machine, one minute:

| | req/s | p50 |
|---|--:|--:|
| Go `net/http` + `html/template` | **35,323** | 0.18 ms |
| Bun | 13,595 | 0.46 ms |
| helix, stateless, 6 shards | 11,563 | 0.71 ms |
| Node | 10,445 | 0.66 ms |
| **helix, stateless, 1 thread** | **4,326** | 1.80 ms |
| Python + Jinja2 | 4,359 | (1 conn; 2,266 at 8 — GIL) |
| helix, dev mode (`HOT = true`) | 2,578 | 2.99 ms |

**Per thread: 2.4× behind Node, 3.1× behind Bun, 3.1× behind Go even with six shards against
its six cores.** Ahead of stdlib Python with Jinja2. Those are the numbers and they should be
quoted as they are.

### The cause is known and it is structural

`jit-explain` on a string/record program reports **`0 kernel sites offered to the JIT, 0
compiled`**. The JIT takes numeric `map`/`filter`/`reduce`/`scan` over packed arrays and
tail-recursive numeric functions. A template render is strings, records and array
concatenation — it sees none of it, so every one of those requests runs on the bytecode VM.
V8 and JSC JIT the equivalent JavaScript; Go compiles it ahead of time.

So this is not "the interpreter is a bit slow". It is that the fast path does not cover the
shape of the workload at all.

### Three options, none of them small, and the order matters

1. **Measure where the VM time actually goes on a render.** Not done. Before any of the
   below: is it interning, allocation, string building, record field lookup, or dispatch?
   The array-accumulation work this cycle found a 216x cliff by measuring rather than
   guessing, and the `.cache()` and `open_keys` findings in the field were both "the
   implausible number was the real one". **This is the only step that should happen first.**
2. **Extend JIT coverage past numeric kernels.** The largest lever and the largest risk —
   and note the differential oracle is *vacuous* for it in the opposite direction from usual:
   today all three engines agree because they run the same interpreted code, so a new JIT
   path is exactly where `vmparity` starts earning its keep again.
3. **Make the VM faster on string/record work** without new codegen. Lower ceiling, much
   lower risk, and 1 tells us whether there is a cheap 30% sitting in one place.

### What the table cannot see, and what should be measured beside it

Every row renders the same HTML, so this measures templating and HTTP — that is the whole
claim. It leaves out what this architecture actually trades for: **no bundler, no route
manifest, no hydration, no serialized payload, and a page and its "SPA transition" being the
same server render.** A hydrating framework spends its budget on exactly the things absent
here, and that shows up in bytes on the wire and time-to-interactive, not in requests per
second.

Not measured yet, and it is the comparison where this design is supposed to win. Until it is
taken, the honest sentence is "2.4x behind Node on templating throughput, and the rest is
unmeasured" — not "different tradeoffs" waved at without numbers.

### Two benchmark traps, both worth institutionalising

Both cost a full measurement before being caught, and both are the same failure: **believing
a timing before verifying the response.**

- **A stale server answered the first run.** An older `helix` still held the port, and
  `listen` uses `SO_REUSEPORT` for sharding — so the new binary bound the SAME port and the
  kernel split requests between two different programs. Only `ss -lntp` revealed it.
- **Python's first number was a flat 44 ms at every concurrency.** Flat against concurrency is
  a fixed delay, not work: `BaseHTTPRequestHandler` leaves Nagle on.
  `disable_nagle_algorithm = True` moved it 177 → 4,359 req/s, a **25x** swing that had
  nothing to do with Python or Jinja2.

The rule both point at: a benchmark harness should assert the response body before reporting
a time, and should assert *which process* answered.

---

## What this plan does not do

- **`emit-hbc` beyond v0.** It supports Int/Float/Bool constants only, so a program with a
  string cannot compile. That is a deep limitation on a separate track, not a gap to close in
  passing.
- **Path and host scoping for capabilities.** ADR 0021 names cap-std. The coarse form in
  Phase 2 must not paint us into a corner, so the value should accept a string **or** a table
  from the start, with the table form refused by name until it works.
- **Claim a security posture we do not have.** With Phase 2 done the honest sentence is
  *"declared authority, enforced and reviewable, and installing a package cannot execute
  code"* — not "secure by default". The default is still ambient authority, and saying
  otherwise would be the same class of lie as a `sync_dir` that returns `true` where it
  cannot flush.

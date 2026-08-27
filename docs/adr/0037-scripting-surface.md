# ADR 0037 — The scripting surface: a script declares its interface

- **Status:** **D1 accepted and implemented** (`src/climain.rs`); D2–D6 proposed, with D6's
  check-time refusal of an unbindable `main` parameter implemented alongside D1
- **Date:** 2026-08-25, substantially revised 2026-08-27, D1 implemented 2026-08-27
- **Revision:** D1's binding rule was rewritten. The first draft invented a mapping
  (*no default ⇒ positional, default ⇒ option*) which could not express a required named
  option; probing the v0.6.0 binary showed the language **already** binds arguments by
  name, out of order, with trailing defaults — so the command line adopts the call-site
  rule wholesale and the special case disappears. Added: what `main`'s return value means
  (discarded, like every top-level value — measured), the migration cost, a verification
  section with the four properties that must be checked, and what would show this ADR is
  wrong.
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0021 — Capability sandbox](0021-capability-sandbox.md),
  [ADR 0011 — Core/stdlib boundary](0011-core-stdlib-boundary.md),
  [ADR 0017 — Methods and functions](0017-methods-and-functions.md),
  [ADR 0024 — Total runtime](0024-total-runtime-no-host-panics.md),
  [ADR 0016 — Build and packaging](0016-build-and-packaging.md),
  [ADR 0036 — One semantics](0036-one-semantics.md)

## Context

Helix can compute. It cannot yet be a **tool**. Every fact below was measured against
the v0.6.0 binary, not assumed:

```
$ helix run tool.helix --threads 8 reads.fastq
hi
$ echo $?
0
```

The arguments are not rejected, not warned about, not passed anywhere. They are
**discarded in silence** — the worst of the three possible behaviours, because the
command looks like it worked. `args`, `argv`, `env`, `getenv`, `exit`, `run`, `cwd` and
`stdin` are each *"is not a known function"*. `helix build` bundles a standalone
executable with exactly the same blindness: you can ship a binary that cannot take a flag.

And the one piece that *does* work makes it worse. `#` opens a comment in Helix, so
`#!/usr/bin/env helix` on line 1 lexes as a comment and a `chmod +x`'d `.helix` file runs
from the shell today. **The shebang works and the arguments do not.** A file that
announces itself as a tool and then ignores everything the tool was told is a trap we
shipped by accident.

This is the gap between "a language that computes" and "a language you can put in a
pipeline". A scientist's real workflow is a chain of small programs driven by a shell, a
Makefile, Snakemake or Nextflow. A language whose programs cannot be a *step* in that
chain is a notebook.

### What already exists, and therefore constrains the answer

The surface is less empty than it looks, and every existing piece points the same way:

| already true | consequence for this ADR |
|---|---|
| `print`/`emit`/`write` go to stdout, `elog` to stderr | the stream split a CLI needs is already correct; diagnostics will not pollute a pipe |
| an uncaught `raise` exits 1, a normal end exits 0 | the exit-code *map* exists; nothing can *choose* a code |
| `read_int()` reads one line of stdin and answers `missing` at EOF | the shape for the rest of stdin is already set |
| `try e` evaluates to `{ok: Bool, value: …, error: String}` | the language already has a **value-shaped** way to inspect a failure — no new mechanism is needed to inspect a subprocess |
| `fn greet(name: String, times: Int = 2) = …` parses **today**; `helix check` refuses `score("three")` with *"argument 1 of `score` should be Int"* | a typed, defaulted, documented, statically-checked parameter list **already exists in the grammar** |
| `capability::Effect` reserves `Process` and `Env`; `Authority` and `gate` already handle them | the gate is built; only the `effect_of` arms are missing |
| ADR 0024: user input never aborts the host | argv is user input, and the argv→value conversion is therefore a total function that must produce an error, never a panic |

The fifth row is the one this ADR turns on. The declaration a command line needs — named
parameters, types, defaults, doc comments, checked before anything runs — is not a thing
Helix must invent. It is `fn`.

To be precise about the claim, because a later section does add syntax: **D1 adds none.**
`fn main(reads: String, threads: Int = 4)` parses on the v0.6.0 binary today, is
type-checked today, and binds named arguments out of order today. D2 introduces one new
top-level form (`env`), and argues for it on its own merits rather than under D1's
banner.

## Prior approaches and their documented shortcomings

| language | approach | documented shortcoming |
|---|---|---|
| Python | `sys.argv: list[str]`, then `argparse` | the list is untyped and unvalidated; every script re-derives its own parser, and the errors land wherever the author remembered to look — after side effects have already run |
| Python | `subprocess(…, shell=True)` | the docs put the burden on the caller: *"it is the application's responsibility to ensure that all whitespace and metacharacters are quoted appropriately to avoid shell injection vulnerabilities"* |
| Python | `subprocess.run(...)` | `check` defaults to **false** — a failed child is a value you must remember to inspect, and the common form silently continues |
| Python | `sys.exit()` | `SystemExit` inherits `BaseException` *"so that it is not accidentally caught by code that catches `Exception`"* — the right behaviour, obtained by an exception-hierarchy accident that every author must know about |
| PowerShell | `param()` block, typed, `[Parameter(Mandatory)]` | genuinely good binding; but *"scripts do not return an exit status"* by default, and **"any argument that is non-numeric or outside the platform-specific range is translated to the value of `0`"** — a failed exit reported as success |
| Deno | `--allow-env=API_KEY`, `--allow-run=git` | scoping exists, but it is supplied by the **invoker**, not declared by the program; and the docs concede the ceiling: *"a subprocess runs as a separate program with its own permissions, not the restricted set you granted the Deno process"*, with `--allow-run=deno` singled out as *"especially dangerous"* |
| Nushell | `def main [x: int]`, auto `--help` from doc comments | the closest prior art, and it is right: typed parameters, defaults, flags and generated help all from one declaration. It stops there — the declaration carries no authority meaning |
| Rust | `clap` derive | a struct's fields become the parser, the help and the types, checked by the compiler. Same shape as Nushell; same stopping point |
| POSIX sh | `getopts` | positional-only, no types, no help; and the shell's own word-splitting is the injection surface every other row is trying to escape |

### The observation this ADR is built on

A command line is **four artefacts that are all the same declaration**:

1. the **parser** (what the strings mean),
2. the **help text** (what a human is told they mean),
3. the **type contract** (what the program may assume, checked before it runs),
4. the **authority grant** (what the program is thereby permitted to touch).

clap, Nushell and PowerShell join 1–3 and leave 4 to the operating system. Deno has 4 and
leaves 1–3 to the program. Python has none of them joined. **Nobody joins all four**, and
the reason is historical rather than principled: argument parsing grew up in libraries,
and sandboxing grew up in runtimes, so the two never met.

Helix is in the rare position of being able to join them, because its capability
categories (ADR 0021) and its type checker are the same project.

### First-hand evidence for the subprocess decision

This is not a theoretical concern about quoting. Building *this very release*, a command
string passed through one extra layer of shell quoting arrived mangled **six separate
times** — `$B` evaluating to nothing so a script ran `run: command not found`; a heredoc
truncated mid-word so a `git commit` landed with half a message; a `for r in …` loop whose
variable vanished. Every one of those was a *string* that a *shell* re-parsed. None of
them could have happened to an argv array. The failure mode is not exotic and it does not
require an attacker — it is the default outcome of composing string commands, and it
happened repeatedly to a careful process that knew about it in advance.

## Decision

### D1 — A script's command line is `fn main`, bound by the rule Helix already uses.

```helix
## Score reads and write a summary.
##
## Reads a FASTQ, scores each read, and writes a CSV of per-read quality.
fn main(reads: String, out: String = "scores.csv", threads: Int = 4, verbose: Bool = false) =
    ...
```

**The binding rule is not invented for the command line. It is the call-site rule,
already implemented and already shipping**, measured on the v0.6.0 binary:

| at a call site, today | on the command line |
|---|---|
| `go(10, 3)` | `tool 10 3` |
| `go(a: 10, b: 3)` | `tool --a 10 --b 3` |
| `go(b: 3, a: 10)` — out of order, works | `tool --b 3 --a 10` |
| `go(10)` — trailing default omitted | `tool 10` |

So the rules a reader has to learn number **zero**:

- **every** parameter can be given by name (`--out scores.csv`) or positionally, exactly
  as every parameter can be given as `out: …` or positionally in Helix;
- a parameter **with** a default may be omitted; one **without** may not;
- `Bool` defaulting to `false` also accepts the bare form `--verbose`. This is the single
  CLI-specific affordance in the whole design, and it is a shorthand for `--verbose true`,
  not a separate concept;
- the **doc comment on `main`** is `--help`; its first line is the summary;
- the **types are the checker's types**. The argv string is converted once, at the
  boundary, and a bad conversion is an error naming the option and the value — never a
  panic (ADR 0024), and identical on all three engines.

**An earlier draft of this ADR got this wrong**, and the error is worth recording because
it is the kind a design makes when it reasons about a command line instead of about the
language. It said *"no default ⇒ positional; default ⇒ option"* — a clean-sounding
mapping that **cannot express a required named option**, since "required" and "has a
default" were made the same axis. Every real tool has one (`--input` that you must
supply). Helix's own call-site rule has no such hole: `a` in `go(a, b)` is required *and*
nameable. Adopting the existing rule wholesale removes the special case instead of
patching it.

**Ordering falls out of a constraint that already exists.** The parser refuses
`fn go(a: Int = 1, b: Int)` — *"parameter `b` has no default but follows one that does"* —
so required parameters are always a prefix, and the positional form is therefore always
unambiguous. Nothing new had to be decided.

**`main`'s return value is discarded, like every other top-level value.** Measured: a bare
`1 + 1` at the top level of a Helix program prints nothing and exits 0. A function body is
an expression, so `main` necessarily produces a value; making that value the exit code
would be a second, invisible channel for something D4 gives an explicit verb. Consistency
with the top level is the whole argument — a script is its top level, and `main` is not a
different kind of place.

If a file declares `main`, the runtime calls it after the top level, with argv bound. **If
a file does not declare `main`, passing arguments is refused** rather than ignored. The
silence stops in the same release that makes the alternative possible.

`--help` and `--version` are answered from the declaration **without running the program**,
which is what makes them safe to answer for a script whose top level has effects.

**Migration cost, stated exactly:** a program that today declares `fn main` and never
calls it currently runs its top level and nothing else (measured). After this, `main`
runs too. That is a real behaviour change for such a program, it is detectable
mechanically (declares `main`, never calls it), and it is the only one D1 causes.

### D2 — The environment is declared, not read.

```helix
## The API key for the upload endpoint.
env API_KEY: String

## Verbosity for the shared log sink.
env LOG_LEVEL: String = "info"
```

- A missing **required** variable is refused **before the first effect runs**, naming the
  variable and quoting its doc comment — not on line 300, after the output file has been
  truncated.
- The declared set **is** the `Env` grant. A program may read exactly these names.
- There is deliberately **no `getenv(s)` taking a computed string**. A computed name makes
  the read set unknowable, and an unknowable set cannot be granted, audited, or printed by
  `helix describe`. This is the same reasoning ADR 0011 used against a flat global
  namespace: keep the seam narrow while it is still cheap.

### D3 — Subprocess: argv only. There is no shell form.

```helix
mut r = run("samtools", ["sort", "-o", out, input])
```

- **No string-command form exists.** Injection is not discouraged, it is
  *unrepresentable*. Python's own documentation describes the burden `shell=True` creates;
  Helix declines to create the burden.
- A **non-zero exit is an error**, matching ADR 0036's rule that a failed computation is a
  failure and not a quietly-absent value. Python's `check=False` default is the
  counter-example: the easy spelling ignores the failure.
- To *inspect* a failure instead of propagating it, use the mechanism the language already
  has: `try run(...)` yields `{ok, value, error}`. No second API, no `check:` keyword —
  and since v0.6.0 that record is also what `assert_error(try run(...), "no such file")`
  reads, so a test asserting how a subprocess failed needs nothing new either.
- `Process` authority is granted **per program name**, not as a blanket.

**And the honest part.** Granting `run` is a **boundary exit, not confinement**. Deno's
documentation states it plainly — a subprocess runs with its own permissions, outside the
sandbox — and singles out granting a runtime the right to re-launch itself as *especially
dangerous*. The same is true here: `run("sh", …)`, or `run("helix", …)`, is equivalent to
granting everything. Helix will say so in the error text and in the audit log rather than
implying a guarantee it cannot keep. A capability system that overstates its reach is
worse than one that does not exist, because it also spends the trust.

### D4 — `exit(code)` is not catchable, and a bad code is refused.

- `try` catches **errors**. It does not catch an exit. Python arrives at this behaviour by
  making `SystemExit` a sibling of `Exception` rather than a subclass — the right outcome
  reached through a hierarchy detail every author must learn. Helix states it as a rule.
- The map is total: normal end → `0`; uncaught error → `1`; `exit(n)` → `n`.
- `n` must be an `Int` in `0..=255`. Anything else is **an error**. PowerShell documents
  translating a non-numeric or out-of-range exit argument to `0` — a program that failed
  reporting that it succeeded, which is the single worst thing an exit code can do.

### D5 — stdin is text, and EOF is `missing`.

`read_line()`, `stdin_text()`, `stdin_lines()` — following `read_int()`'s existing shape,
where end of input is `missing` rather than an error or a sentinel. Reading stdin is a
console effect, not an authority (the process's own stdin is not the filesystem), so it
stays ungated, exactly as `read_int` is today.

### D6 — `helix check` checks the command line.

`--help` becomes derivable statically; a `main` whose parameter cannot be bound from a
string is refused ahead of time; and `helix describe` — the machine-readable API surface —
grows the CLI, so an agent or a workflow engine can discover how to drive a Helix program
without running it.

## Rationale

**It adds one concept, not seven.** Every part of D1 is a feature the language already
has and already checks. The new work is in the *runtime* (bind argv, convert, call), not
in the grammar, the checker, or the type system. Compare the alternative — `args() ->
[String]` — which adds one builtin and then obliges every program to grow a parser, a help
text and a validation layer that nothing checks.

**A declared interface is a discoverable one, and that is not a nicety.** A field report
on v0.6.0 recorded *selective import* as a missing feature — it had shipped a release
earlier — because probing for it produced a true error about the wrong thing. In the same
report, destructuring was listed as flatly absent while the lambda form worked. Both are
the same failure: a capability that exists but cannot be *asked about*. A program whose
interface is a declaration can be asked (`--help`, and `helix describe`), which is the
difference between a tool someone can pick up and one they conclude is not there.

**It makes the errors early instead of deep.** The failure a scripting language actually
inflicts is not a wrong answer; it is a program that ran for six minutes, wrote half an
output file, and then discovered that `--threads` was `"eight"` or that `$API_KEY` was
unset. D1 and D2 move both of those in front of the first effect.

**It is the one design where the capability grant is not a second thing to maintain.**
ADR 0021's hardest unsolved problem is where scoped grants come from: a user who must
write `--allow-env=A,B,C` will write `--allow-env` instead. If the program's own
declaration is the grant, the scoped case is the *default* case and the blanket case is
the one you have to ask for.

**It follows this project's own repeated lesson.** ADR 0036 spent a release closing
sixteen divergences that existed because one rule lived in many implementations. A CLI
that is parsed in the program, documented in a README, and granted on the command line is
that same shape — three copies of one fact, free to drift. One declaration is the fix,
applied before the drift rather than after.

## Rejected alternatives

- **`args() -> [String]`** (the Python/Go/Deno shape) — rejected. It gives `--help` to
  nobody, gives the type checker nothing, gives the capability gate nothing to scope, and
  moves every error past the side effects. It is also *additive-forever*: the moment
  scripts read `args()`, a declared CLI can never be the one obvious way (ADR 0011's
  small-core discipline, and the PHP lesson it cites — introduce the seam early).
- **A dedicated `script { … }` declaration block** — rejected. `fn main` already parses,
  already type-checks, already has per-parameter defaults, already has doc comments. A new
  block would duplicate all four and then drift from them.
- **`run(cmd: String)` with shell parsing, even as an opt-in** — rejected. The opt-in *is*
  the vulnerability; every documented shell-injection bug is a program that took the
  opt-in. Nothing is gained that `run(prog, args)` cannot express, and pipelines belong in
  the calling shell or in explicit composition, not in a string.
- **C-style auto-`main` that replaces the top level** — rejected. A Helix file is a
  script; its top level is the program. `main` is the *argument surface*, not a second
  entry point, and a file without one keeps running exactly as it does today.
- **Ambient mutable environment (`os.environ`)** — rejected. The read set is unknowable,
  it is invisible in the program's signature, and it makes D2's grant unenforceable.
- **Deferring the whole surface to a package** — rejected. A CLI cannot be a library
  concern when the *runtime* is what must bind argv before the top level runs, and when
  the declaration is also a capability grant the gate has to see.

## How this gets verified

A proposal in this repository is not finished until it says how it can fail. **argv is an
axis no existing gate covers**, and each of them misses it for a different reason:

| gate | why it cannot see a command line |
|---|---|
| `tests/compat/` | freezes 119 programs invoked exactly ONE way — its record has no argv column |
| `scripts/dfdiff.sh` | runs every tracked program under both backends, with no arguments |
| `scripts/vmparity.sh` | same, across engines |
| `scripts/checkall.sh` | type-checks without running, so it never binds anything |
| the doc-example gate | runs `>>>` snippets, which have no invocation |

So D1–D6 ship with an axis of their own, and it already has a working shape to copy:
`tests/release/v0.6.0-errors.tsv` is a table of *(expected substring, program)* that
`scripts/release-smoke.sh` executes. The CLI table is that plus an argv column —
*(program, argv, exit, stdout, stderr)* — which is exactly the row `tests/compat/` would
need to grow to express a tool, and is why that growth is listed under Consequences below
rather than deferred.

Four properties have to be checked, and each names its own failure:

1. **Binding.** `tool 10 3`, `tool --a 10 --b 3` and `tool --b 3 --a 10` produce the same
   result, because D1 claims they are the call-site rule. If they diverge, D1 is wrong,
   not the test.
2. **Refusal.** A bad conversion (`--threads eight`), a missing required parameter, an
   unknown option, and an out-of-range `exit` code each exit non-zero **and name the
   thing**. Exit code alone is not enough: this session watched a field report record a
   shipped feature as missing because it read an exit code and not the message.
3. **Engine agreement.** The argv→value conversion is new code on a hot boundary and must
   be byte-identical on all three engines. `helix test --engines` already does exactly
   this for a Helix program, so a CLI written in Helix tests its own binding.
4. **The grant.** A program declaring `env API_KEY` is refused *before its first effect*
   when the variable is absent. That is D2's whole claim, and it is the one that is
   invisible unless something asserts the ORDER — a refusal after the output file was
   truncated satisfies "it refused" and defeats the purpose.

And the standing rule this project earned the hard way (`docs/testing.md`): **a gate has
to be sabotaged once to prove it can fail.** Three gates here were found unable to fail —
`dfcheck.sh` diffing three copies of "no such file", 28 `native-df` tests executed by
nothing, and `vmparity.sh` printing `RESULT=1` without exiting on it. A CLI gate that has
never been made to go red is a claim, not a check.

### What would show this ADR is wrong

- If the call-site rule turns out **not** to survive contact with real command lines —
  say, tools routinely need a repeated option (`--include a --include b`) and Helix's
  parameter model cannot express one — then D1's central claim (that the binding rule is
  free) is weaker than stated and the mapping needs its own design after all.
- If declaring the environment (D2) proves unusable because real programs read variables
  chosen at run time, then the grant argument collapses, since an unknowable read set
  cannot be granted. The fallback is Deno's: scoping supplied by the invoker.
- If `--help` generated from a doc comment is materially worse than a hand-written one for
  any real tool, the "one declaration, four artefacts" framing loses its fourth leg.

## Consequences

**Easier.** A Helix program becomes a Snakemake/Nextflow step, a Makefile rule, and a
`helix build` binary that answers `--help`. The shebang that already works stops being a
trap. `helix describe` can tell an agent how to invoke a program, not just what functions
it defines.

**Harder.** The argv→value conversion is a **new boundary with its own error surface**, and
by this project's standards it must be byte-identical on the tree-walker, the VM and the
JIT, and in a bundled binary — a new row in every conformance matrix, and one that
`scripts/dfdiff.sh` and `vmparity.sh` cannot reach, because both drive programs that take
no arguments.

**A gap in the new compat baseline.** `tests/compat/` freezes exit, stdout and stderr for
119 programs *invoked one way*. A program whose output depends on argv or the environment
is not expressible there. Capturing it needs a pinned **invocation**, not just a pinned
program — a format change to the baseline, decided before the first such program exists
rather than after.

**Commits the project to** filling in `capability::effect_of`'s `Process`/`Env` arms (the
categories, the `Authority` fields and the gate are already written and waiting), and to
ADR 0021 phase 1b's `--allow-run` / `--allow-env`, now with the program's own declaration
as the default source of the scope.

## Open questions

- **Paths are `String`.** A `Path` type would make `--out results/` checkable at the
  boundary and would give `cap-std` scoping (ADR 0021's later phase) a *value* to attach
  to instead of a string. Deferred to its own ADR; D1 works either way.
- **A frame from stdin.** The readers take paths; `read_csv("-")` is a convention, not a
  design. Whether stdin is a path spelling or a value is a seam question (ADR 0012).
- **Subcommands.** Nushell derives them from `main sub`. Deferred until a real program
  wants one; nothing in D1 forecloses it.
- **Signals.** A tool in a pipeline must die quietly on `SIGPIPE` and promptly on
  `SIGINT`. Today neither is specified or tested — `print` into a closed pipe is
  unmeasured. This is a totality question (ADR 0024) as much as a scripting one.
- **Repeated options** (`--include a --include b` into an `Array`) and `--` as the
  end-of-options marker: mechanical, but they must be decided once rather than per program.
- **A bundle is one source file, and a real tool is not.** `helix build` on a program
  with imports refuses:

  > `helix build` can only bundle a single-file program yet, but `…/main.helix`
  > imports other modules — inline the imports into one file for now.

  That refusal is the *right* failure (compare the argv gap above, which is silent),
  but it means the two capabilities interact: D1 gives a program an interface, and a
  program worth giving an interface to is exactly the one that has grown past a single
  file. `bundle::embedded()` carries one `(source, filename)` pair; a tool that ships
  as one binary wants a resolved module graph — entry, canonical module names, sources
  or bytecode, and the `helix.lock` hashes they were resolved at, so "one binary"
  names an exact program. That is its own ADR, and it should land beside this one
  rather than after it.

## Sources

- Python `subprocess` security considerations, the `shell` default, and `check`:
  <https://docs.python.org/3/library/subprocess.html>
- `SystemExit` and its `BaseException` base:
  <https://docs.python.org/3/library/exceptions.html>
- PowerShell `param()` blocks and exit-status handling (including the out-of-range → `0`
  translation):
  <https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_scripts>
- Deno permissions, scoped `--allow-env`, and the `--allow-run` sandbox-escape warning:
  <https://docs.deno.com/runtime/fundamentals/security/>
- Nushell scripts, `def main`, typed parameters and generated help:
  <https://www.nushell.sh/book/scripts.html>
- `clap` derive — a type declaration as the parser and the help:
  <https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html>

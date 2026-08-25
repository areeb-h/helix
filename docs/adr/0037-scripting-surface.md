# ADR 0037 — The scripting surface: a script declares its interface

- **Status:** Proposed
- **Date:** 2026-08-25
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

### D1 — A script's command line is `fn main`. No new syntax.

```helix
## Score reads and write a summary.
##
## Reads a FASTQ, scores each read, and writes a CSV of per-read quality.
fn main(reads: String, out: String = "scores.csv", threads: Int = 4, verbose: Bool = false) =
    ...
```

The binding rules are derived from what the declaration already says, so there is nothing
extra to remember:

- a parameter **without** a default is **positional and required**;
- a parameter **with** a default is an option, `--out scores.csv`;
- a `Bool` parameter defaulting to `false` is a **flag**, `--verbose`;
- the **doc comment on `main`** is `--help`; the first line is the summary;
- the **types are the checker's types** (`Int`, `Float`, `String`, `Bool`). The argv
  string is converted once, at the boundary, and a bad conversion is an error that names
  the option and the value — never a panic (ADR 0024), and identical on all three engines.

If a file declares `main`, the runtime calls it after the top level, with argv bound. **If
a file does not declare `main`, passing arguments is refused** rather than ignored. The
silence stops in the same release that makes the alternative possible.

`--help` and `--version` are answered from the declaration **without running the program**,
which is what makes them safe to answer for a script whose top level has effects.

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
  has: `try run(...)` yields `{ok, value, error}`. No second API, no `check:` keyword.
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

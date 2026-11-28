# ADR 0068 — Written by agents, verified by people

- **Status:** Proposed — a programme of work, in six parts, each to be accepted or cut on its own.
  Part 0 is implemented: `scripts/agentbench.py`, `bench/agents/` (eight tasks), the
  `--selftest` step in CI and `.github/workflows/agentbench.yml`; the baseline waits for a key.
- **Date:** 2026-11-27
- **Deciders:** Areeb + Claude
- **Related:**
  - [ADR 0062](0062-made-for-agents.md): measure agents' mistakes, fix them in the language.
  - [ADR 0021](0021-capability-sandbox.md): the capability sandbox. Phase 1 shipped.
  - [ADR 0063](0063-the-app-platform.md): inferred permissions and the session tape.
  - [ADR 0065](0065-rules-to-rust.md): rules proved in Helix.
  - [ADR 0024](0024-total-runtime-no-host-panics.md): a limit is an error.

## Context

More and more code is written by agents, and the share is rising. What becomes scarce is not
code but **confidence in code**. An agent can produce a change in a minute; a person needs an
hour to decide whether to trust it, and usually decides on a skim. Every language in use today
was designed for the person who writes the code. None was designed for the person who has to
accept code someone else wrote.

Helix is closer to that than any of them, by accident of what it already insists on.

**It insists on six things:**

- one program, one answer, on three engines, compared byte for byte;
- values that never change, so a function can be understood on its own;
- every authority a function reaches, closed over the call graph (`helix effects`);
- a session that replays exactly, with nothing read from the world (ADR 0063);
- rules whose properties are checked on every reachable state (ADR 0065);
- a binary that answers questions about itself (`helix search`, `describe`, `check --json`),
  and errors measured against what agents actually type (ADR 0062).

**What is missing:**

- **An agent cannot safely run what it wrote.** The gate from ADR 0021 exists, but only
  coarsely: each category is on or off, it is off by default, and there is no path or host
  scoping.
- **An error says what to do, in prose, and nothing can apply it.** `HelixError` is a message,
  a position and a hint, with no edits and no stable codes.
- **An agent reaches Helix through a shell**, parsing text written for people.
- **A change cannot say what it is supposed to do** except through examples and hand-written
  tests.
- **A reviewer gets nothing but the diff.** The facts Helix could state about a change are left
  for the reviewer to work out: authority it adds, behaviour it changes, behaviour it preserves.

## Decision

Helix's goal is **that every change arrives with evidence a person can read in a minute.**
Agents write; Helix states, by machine check, what the change does and does not do; a person
decides. The six parts below are each a step toward that, ordered so each is useful on its
own.

### The rules every part obeys

1. **Nothing for machines only.** Every JSON output has the same content as its prose form.
   Every agent tool is also a command a person can run. There is no syntax only a machine
   reads. If something helps an agent but makes code harder for a person to review, it is
   refused.
2. **The toolchain never calls a model.** Every answer Helix gives is computed and
   deterministic, the same on every machine and every run. Agents are outside Helix, never
   part of it.
3. **Measured, not guessed** (ADR 0062). Each part names the number it moves and is judged by
   that number.
4. **Honest about what was not checked.** Evidence that omits a gap is worse than none. Every
   report says what it covered and what it did not.

### Part 0 — The agent benchmark (measure first)

ADR 0062's study, made a standing instrument.

- `bench/agents/` holds tasks. Each is a `task.md` stating the job in a user's words, plus a
  hidden acceptance check: a test file, a replayed session, or expected output.
- `scripts/agentbench.py` runs any agent CLI through a command template, for example
  `claude -p {prompt}`. Each agent works in a fresh directory with only the `helix` binary and
  `AGENTS.md`.
- For each task it records four things:
  - whether it was solved;
  - whether the first `helix check` was clean;
  - how many check/run iterations it took to green;
  - how many errors had a fix the agent applied unchanged.
- Each task runs several times, and the report gives the spread, because agents are noisy.
- It runs on demand and weekly, not on every PR, because it needs an API key and costs money.
  Results are committed under `bench/agents/RESULTS.md`, so the trend is visible.

**Number:** the baseline, taken before Parts 1–5 land.

### Part 1 — Safe to run: finish the sandbox (ADR 0021 phases 1b–3)

A program an agent just wrote must be able to run with exactly the authority it was given, and
no more.

- **Scoped grants:**
  - files: `--allow-fs-read=./data`, `--allow-fs-write=./out`;
  - network: `--allow-net=api.example.com:443`;
  - processes: `--allow-run=git`.
  - The same grants can live in `[capabilities]` in `helix.toml`, where the code's hash pins
    them (ADR 0021 §2).
  - Paths are resolved by the kernel (`openat2` with `RESOLVE_BENEATH`, through `rustix`,
    which Helix already uses). A string check is never enough: ADR 0021 §4 forbids
    realpath-then-open.
- **`helix run --sandbox`** denies everything not granted. Agent tools (Part 3) only ever run
  this way. A denial names the builtin, the authority and the flag that would grant it.
- **`helix effects --check`** compares what the code can reach with what `helix.toml`
  declares, and fails when the code reaches more. Running it in CI means a change that adds
  authority cannot slip through.
- **Narrowing per evaluation** (ADR 0021 §5): a program can run generated code with a strictly
  smaller grant, never a larger one.
- **Rollout as ADR 0021 §7 says:** audit, then warn, then enforce for ordinary runs. Enforced
  from day one for anything an agent runs.
- **Honest positioning** (ADR 0021): this is a language-level sandbox. It is strong against a
  program that only uses Helix, but it is not a substitute for OS isolation when the code is
  hostile. The docs say so where the flag is described.

**Number:** zero escapes in an escape-test suite, which runs in CI:

- symlinks and `..`;
- the TOCTOU swap;
- every gated builtin and method by name, generated from the registry so that a new builtin
  cannot be ungated silently.

### Part 2 — Errors that machines can apply and people can read

- **A fix on the error.** `HelixError` gains an optional `fix`: one or more edits (file, span,
  replacement) and a confidence, either `certain` or `suggested`. `helix check --json` emits
  them; the rendered error shows the same edit as a one-line "change … to …". It is added at
  the existing hint sites, which already know the replacement (`did you mean \`length\`?`).
- **`helix fix [--dry-run] [files]`** applies only `certain` fixes, then re-checks. It is the
  same for a person in an editor and for an agent.
- **Stable codes.** Every diagnostic gets a code (`H0412`), and `helix explain H0412` prints
  the rule, an example, and the fix. A code never changes meaning once assigned. That is what
  lets an agent, a test or a person refer to an error without matching its text.
- **A ratchet, like `stranger.py`'s:** the share of diagnostics that carry a fix may only go
  up.

**Number:** in the benchmark, the share of errors resolved by applying the fix unchanged, and
the iterations to green.

### Part 3 — One interface for agents: `helix mcp`

A Model Context Protocol server over stdio, with no network port. Its tools are:

| Tool | What it does |
|---|---|
| `check` | the diagnostics, with their fixes |
| `fix` | apply certain fixes |
| `run` | always sandboxed; grants are passed explicitly and echoed back |
| `test` | `helix test --engines` |
| `describe` / `search` | the API, as it is |
| `effects` | authority and reproducibility |
| `replay` | a session, verified |
| `review` | Part 5 |

- Every tool returns exactly what the matching command prints with `--json`. Nothing new is
  computed, so the CLI and the server cannot disagree.
- It is tested with a scripted client in `tests/cli.rs`.

**Number:** benchmark tasks solved through MCP, compared with through a shell.

### Part 4 — Saying what code must do: properties

- **`## property:` lines** on a function, beside its `## >>>` examples:

  ```helix
  ## property: xs.sort().count() == xs.count()
  ## property: xs.sort().sort() == xs.sort()
  export fn sort_scores(xs) = …
  ```

- **`helix test` checks each property** on generated inputs, drawn from the function's types,
  its examples and the edge values the fuzzers already know (0, −1, `i64::MIN`, NaN, `missing`,
  empty, very deep, shared). It runs on all three engines with a fixed seed.
- **A failure is a minimal counterexample**, shrunk the way `std.model` already shrinks a
  path, and printed as a `## >>>` line that can be pasted in as a regression example.
- **A property over a finite domain is proved, not sampled.** It goes to `model.explore`, and
  the report says "proved" rather than "tested".

The point is for the reviewer: a property is one line a person reads, and the machine checks
it a thousand ways. An agent asked for a change can be asked for its properties first.

**Number:** in the benchmark, tasks where a property caught a wrong first draft.

### Part 5 — Showing what changed: `helix review`

`helix review <base> [<head>]` compares two revisions and prints one screen, with the same
content as JSON. Its sections, in order of importance:

1. **Authority.** Every function whose effects closure gained something, with the path to the
   builtin that adds it: "`export_report` can now reach **net** (via `send_summary` →
   `http_post`)". This is first because it is what a skimming reviewer most needs and least
   often sees.
2. **Interface.** Exported functions added, removed or changed in signature.
3. **Behaviour.** The same evidence, run on both revisions and on all three engines: doc
   examples, the corpus, recorded sessions (ADR 0063) and properties (Part 4). It lists what
   changed output, separately from what was expected to change. A session whose screens
   differ links the viewer.
4. **Preserved.** What was checked and did not change, counted: "312 examples, 41 sessions,
   18 properties unchanged on 3 engines".
5. **Not covered.** Changed functions that no example, test, property or session reaches. A
   reviewer then knows exactly where to read, and a "no differences" report can never be
   mistaken for "fully checked" (rule 4).

A CI job runs it on every PR and attaches the report, and the project's own PRs use it from
the first day it works.

**Number:** for a sample of real PRs, what the report found that the diff did not show (new
authority, unexercised changes), recorded as the job runs.

## Order

| Step | Part | Depends on | Size |
|---|---|---|---|
| 1 | 0 — benchmark and baseline | — | days |
| 2 | 1 — sandbox: scoped grants, `--sandbox`, `effects --check` | — | 2–3 weeks |
| 2 | 2 — fixes, codes, `helix fix` | — | 1–2 weeks, then ongoing |
| 3 | 3 — `helix mcp` | 1, 2 | ~1 week |
| 4 | 4 — properties | — | ~2 weeks |
| 5 | 5 — `helix review` | 1 (authority), 4 (properties); sessions exist | 2–3 weeks |

Parts 1 and 2 run in parallel. The benchmark is re-run after each step; a part that does not
move its number is reconsidered before more is built on it.

## Not doing

- **Natural-language programming,** or a model anywhere inside the toolchain (rule 2).
- **Syntax only an agent reads,** or code shaped for token counts at a reader's expense
  (rule 1).
- **Telemetry.** The benchmark measures agents we run, never users.
- **Prompts asking a person to approve a capability at run time** (ADR 0021 §3: fail closed).

## Risks

- **The benchmark is noisy and costs money.** Tasks run several times, the spread is
  reported, and it runs weekly rather than per PR.
- **A sandbox is a claim, and claims get relied on.** The escape suite runs in CI, and the
  docs say where the sandbox stops (OS isolation is still needed for hostile code).
- **A wrong "certain" fix is worse than none.** `certain` is reserved for fixes with exactly
  one correct reading (a typo with one candidate, `&&` → `and`). Everything else is
  `suggested`, and `helix fix` never applies it.
- **A clean review can be mistaken for a safe change.** Section 5 exists so it cannot be: an
  unexercised change is reported as such.

## What "revolutionary" has to mean here

Nothing in this plan is a new idea alone:

- capability security is decades old;
- property testing comes from QuickCheck;
- fix-its from Clang and rustc;
- MCP is a protocol.

What no language has is all of them, **built on one deterministic runtime whose engines check
each other**, so that every statement in a review is something the machine verified on three
engines, not a summary someone wrote. The test of this ADR is concrete:

- a reviewer reads a `helix review` report instead of the diff, and is right more often;
- the agent benchmark climbs, part by part.

If it does neither, it was not worth building.

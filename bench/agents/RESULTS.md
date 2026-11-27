# Agent benchmark results

Each section is one recorded run of `scripts/agentbench.py` (raw data in `results/`),
appended by `--record`. ADR 0068 judges each of its parts by how these numbers move, so a run
is recorded before a part lands and after it.

**No baseline yet.** The harness, the eight tasks and their checks are in place and checked
by CI (`--selftest`); the first recorded run needs an agent with an API key (see README).

## Before the baseline: what writing the reference solutions ran into

Not a measurement — one agent, once, unrecorded — but the same instrument's first signal. The
reference solutions were written by an agent working the way the benchmark's agents will:
from `AGENTS.md`, `helix search` and the errors. Six of its first guesses were wrong, and the
errors pointed to the right name in none of them (each was found with `helix search`):

| guessed | right | what the error said |
|---|---|---|
| `read_file(path)` | `read_text(path)` | "`read_file` is not a known function" — no suggestion |
| `write_text(path, s)` | `s.write_to(path)` | "`write_text` is not a known function" — no suggestion |
| `df.col(@x)` | `df.column(@x)` | "a DataFrame has no method `col`" — "no similar method" |
| `xs.push(x)` | `xs.concat([x])` | "an Array has no method `push`" — "no similar method" |
| `dict(pairs)` | `pairs.to_dict()` | "`dict` takes 0 arguments, got 1" |
| `fn main(path, top = 10)` | `top: Int = 10` | `--top 3` bound the String `"3"`; failed later, in `take` |

These are ADR 0068 Part 2's first cases: the names other languages use, mapped to Helix's,
with a fix the error can carry. The benchmark's baseline will say how often agents hit them.

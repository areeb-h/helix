# The agent benchmark

How well does an agent write Helix when all it has is the `helix` binary and `AGENTS.md`?
This directory answers that with a number, so that each change made for agents (ADR 0062,
ADR 0068) can be judged by whether the number moves.

```bash
python3 scripts/agentbench.py --selftest                # every check can say yes AND no (CI runs this)
python3 scripts/agentbench.py --list                    # the tasks
python3 scripts/agentbench.py --agent claude --runs 3   # a real run (needs the agent and its key)
python3 scripts/agentbench.py --agent claude --runs 3 --record   # …and add it to RESULTS.md
```

`.github/workflows/agentbench.yml` runs it weekly, and on demand, when the repository has an
`ANTHROPIC_API_KEY` secret, and uploads the results. A person records a run in
[RESULTS.md](RESULTS.md) with `--from <json> --record`.

## A task

`tasks/<name>/` holds:

| | |
|---|---|
| `task.md` | the job, written as a user would write it: what to build, what it must print or export, nothing about how |
| `files/` | what the user hands over: data, a module with a bug, a function to test |
| `accept.py` | the hidden check: `check(workspace, helix) -> [failure, ...]`, empty when it passes |
| `reference/` | a solution that must pass |
| `broken/` | a plausible wrong solution — the mistake a first draft makes — that must fail |

A check never just compares the agent's output with the sample it was given. It runs the
agent's program on inputs the agent never saw (next month's CSV, other `--top` values, a
newer JSON export, random message sequences checked against a model of the specification,
mutants of a function the agent's tests must catch), and on all three engines: an answer only
one engine gives is not an answer. `checks.py` holds what the checks share.

The eight tasks cover the work people bring: tabular data (`sales-by-region`), a command-line
tool (`word-count`), JSON (`order-totals`), numerics (`regression`), a stateful module
(`rate-limiter`, `todo-model`), fixing a reported bug in someone else's code (`fix-the-bug`),
and writing tests strong enough to catch broken implementations (`write-tests`).

## What is measured

The agent's `helix` is a recording wrapper, so the numbers come from what the agent did, not
from what it says it did:

| | |
|---|---|
| solved | the hidden check passed |
| first compile clean | the first `check`, `run`, `test` or `eval` of the agent's code had no error |
| helix calls / failed | how much it took, and how much of it went wrong |
| errors shown (with help) | distinct error messages it met, and how many carried a `help:` line |

Each task runs several times (`--runs`) and the report gives the spread. ADR 0068's fourth
number, *errors resolved by applying the fix unchanged*, needs errors that carry a fix as data
(ADR 0068 Part 2); until then it is not measured, rather than estimated.

## What the agent can and cannot do

- It works in a fresh directory outside the repository with the task's files and `AGENTS.md`.
- `--agent claude` allows reading and writing files and running `helix`: no other command,
  no web, no subagents, no MCP servers.
- Its environment is an allowlist, so it inherits no other session's identity.
- Its `helix` gets none of its credentials and runs under the capability gate
  (`HELIX_CAP=enforce`) with files allowed and the network and processes denied, so there is
  nothing it can send anywhere.
- **The gap, stated:** file access is not yet scoped (ADR 0021 phase 1 is coarse), so an agent
  that went looking could read this directory's checks through `helix` itself. ADR 0068
  Part 1 closes that; until then a run is read before it is recorded, and the workflow refuses
  to upload anything that contains the API key.

## Adding a task

Write `task.md`, `files/`, `accept.py`, a `reference/` that passes and a `broken/` that
fails for the reason the task is about, then run `--selftest`. Prefer jobs a user would really
bring over puzzles, and checks that run the agent's code on fresh inputs over checks that read
it. A change to a task invalidates its history: rename the task instead.

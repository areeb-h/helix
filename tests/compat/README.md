# Compatibility baselines

Each `v<version>/` directory records **what that release actually computed** for every
deterministic, self-contained program in the tree: exit code, stdout, and stderr.

## The one rule

**These files are written once and never rewritten.**

There is no `UPDATE_*` environment variable for them, on purpose. Every other gate in
this repository compares the tree against *itself* — three engines against each other,
two DataFrame backends against each other, the corpus against goldens that
`UPDATE_CORPUS=1` rewrites wholesale. All of that proves consistency. None of it can
answer the question a user asks:

> Does the program I wrote six months ago still compute the same number?

A baseline that can be re-blessed answers that question with "yes, by construction",
which is the same as not answering it.

## What to do when the test fails

`compat_baselines_hold` failing is **not automatically a bug**. It means a release
changed observable behavior. Exactly one of two things is true:

1. **It was unintentional** — a regression. Fix the code, not the baseline.
2. **It was intentional** — then record it in [`MIGRATIONS.md`](MIGRATIONS.md) with the
   program name and *why*. The test then treats that program as a known, documented
   change and stops failing on it.

The baseline file itself stays untouched either way: it is the historical record of
what v0.5.1 did, and that fact does not change just because v0.6.0 does something else.

`MIGRATIONS.md` therefore accumulates into the thing this project has never had — a
list of every user-visible behavior change, written at the moment it happened, by the
person who made it. That list is the release note, and it is checkable.

## Scope, and what is deliberately excluded

Captured: `tests/corpus/` (71 — including the negative fixtures, whose exact refusal
text is as much a compatibility promise as any printed number) and `examples/` (48).

Excluded, each because the pin would otherwise lie:

| Excluded | Why |
|---|---|
| `bench/**` | fixtures are **generated**, not tracked (`git ls-files 'bench/**/data/*'` is empty), so output depends on whether `gen_data.py` has been run |
| `bench/kernels/**` | timing programs, >6s each by design |
| `examples/api/` | two block forever as servers; one does live network I/O |
| `examples/python/` | feature-gated — output depends on how the binary was built |

Every captured program was verified deterministic by running it twice and comparing
(157 tracked programs checked, **zero** non-determinism found).

## Capturing a new baseline

At release time, after the version bump and before the tag:

```bash
BIN=./target/gate/helix bash scripts/capture-compat.sh 0.6.0
```

The script refuses to overwrite an existing directory. That refusal is the design.

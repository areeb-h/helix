#!/usr/bin/env bash
# Type-check EVERY Helix program in the repository — the anti-rot gate.
#
# WHY THIS EXISTS. `v0.1.0`'s release pipeline died in its PGO training step on
# `bench/crosslang/b3_groupby.helix`, which still said `io.read_csv(...)` — a spelling
# ADR-0017 removed. Nine benchmark programs had rotted the same way and nothing noticed,
# because every gate this project had RUNS its programs:
#
#   cargo test          → tests/corpus/ (three engines, pinned goldens)
#   scripts/vmparity.sh → examples/     (JIT vs tree-walker, byte-identical)
#
# and neither can cover a benchmark that needs a 250 MB generated fixture before it will
# run at all. Type-checking needs no fixture. So this covers what running cannot, and it
# costs ~0.03s for the whole tree — `helix check` takes every path in one process.
#
# tests/corpus/ is EXCLUDED on purpose: a dozen of those files are negative fixtures that
# must NOT compile (`x = 0x10`, `print("a\qb")`), and their exact error text is already
# pinned on all three engines by `corpus_is_engine_identical_and_pinned`. Asserting they
# check clean would assert the opposite of what they are for.
set -euo pipefail
cd "$(dirname "$0")/.."

# Overridable so the gate can reuse whatever profile it just built (BIN=./target/gate/helix).
BIN="${BIN:-./target/debug/helix}"

# `git ls-files` rather than `find`: an untracked scratch file in the worktree is not
# something CI should have an opinion about.
mapfile -t files < <(git ls-files '*.helix' | grep -v '^tests/corpus/')
if [ "${#files[@]}" -eq 0 ]; then
  echo "no .helix files found — is this the repository root?" >&2
  exit 1
fi

"$BIN" check "${files[@]}"

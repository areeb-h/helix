#!/usr/bin/env bash
# Freeze what THIS release computes — the version-compatibility baseline.
#
# WHY THIS EXISTS. Every other gate in this repo compares the tree against ITSELF:
# the three engines against each other, the two DataFrame backends against each
# other, the corpus against goldens that `UPDATE_CORPUS=1` rewrites wholesale (106
# rewrites so far, review the only thing behind them). None of that can answer the
# question a user actually asks: "does the program I wrote six months ago still
# compute the same number?" Nothing in the tree recorded what v0.5 printed, and
# ADR 0033 Stage 4, ADR 0034's three arithmetic deltas, the Neumaier switch, and
# the 0.6.0 polars tightenings are ALL queued to change printed numbers.
#
# The captured files are a HISTORICAL RECORD, not a golden set: they are written
# once and never rewritten. When a release intentionally changes one, the change is
# recorded in tests/compat/MIGRATIONS.md — that list IS the user-visible-change log,
# and it is what makes a semantics flip a diff a human approves line by line instead
# of a release note nobody can check.
#
#   Usage: scripts/capture-compat.sh <version>      e.g. scripts/capture-compat.sh 0.5.1
#
# Refuses to overwrite an existing baseline. That refusal is the whole design.
set -euo pipefail
cd "$(dirname "$0")/.."

VER="${1:?usage: capture-compat.sh <version>   (e.g. 0.5.1)}"
OUT="tests/compat/v${VER}"
BIN="${BIN:-./target/gate/helix}"

if [ -e "$OUT" ]; then
  echo "refusing to overwrite $OUT — a baseline is written ONCE." >&2
  echo "an intentional change goes in tests/compat/MIGRATIONS.md, not here." >&2
  exit 1
fi
[ -x "$BIN" ] || { echo "no binary at $BIN (set BIN=)" >&2; exit 1; }

# The pinnable set: deterministic, self-contained, and fast.
#   tests/corpus/  — the semantics fixtures (including the negative ones, whose
#                    exact refusal text is as much a compatibility promise as any
#                    printed number).
#   examples/      — what users read and copy; its data/ fixtures are tracked.
# EXCLUDED, each for a reason that would make the pin lie:
#   bench/         — fixtures are GENERATED (git ls-files 'bench/**/data/*' is empty),
#                    so the output depends on whether gen_data.py has been run;
#                    bench/kernels are timing programs that take >6s by design.
#   examples/api/  — two are servers that block forever, one does live network I/O.
#   examples/python/ — feature-gated; output depends on how the binary was built.
mapfile -t files < <(git ls-files '*.helix' \
  | grep -E '^(tests/corpus|examples)/' \
  | grep -v '^examples/python/' \
  | grep -v '^examples/api/')

[ "${#files[@]}" -gt 100 ] || { echo "only ${#files[@]} programs — wrong directory?" >&2; exit 1; }

mkdir -p "$OUT"
for f in "${files[@]}"; do
  slug="${f//\//__}"; slug="${slug%.helix}"
  set +e
  out=$("$BIN" run "$f" 2>/tmp/.cc_err); rc=$?
  set -e
  err=$(cat /tmp/.cc_err)
  # Same render as `corpus_is_engine_identical_and_pinned`, and the same <src>
  # substitution, so a path never makes a pin machine-specific. The repo prefix is
  # stripped FIRST: diagnostics carry the ABSOLUTE path, so substituting the
  # relative one alone would leave `/home/you/helix/<src>` in every pin.
  {
    printf 'exit: %s\n--- stdout ---\n%s\n--- stderr ---\n%s\n' \
      "$rc" "${out%$'\n'}" "$(printf '%s' "$err" | sed -e "s|$PWD/||g" -e "s|$f|<src>|g")"
  } > "$OUT/$slug.out"
done
rm -f /tmp/.cc_err
echo "captured ${#files[@]} programs -> $OUT"

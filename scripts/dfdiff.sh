#!/usr/bin/env bash
# Run every tracked Helix program under BOTH DataFrame backends and diff.
#
# WHY THIS EXISTS — the procedural lesson of v0.6.0. ADR 0034:14 states the
# doctrine: "A column expression means exactly what the same expression means on
# scalars." Twelve violations of it shipped in v0.5.1, five of them undeclared,
# and the reason none was caught is that at the v0.5.1 tag
# `examples/dataframes/dataframes.helix:27` was the ONLY `with({…})` in the entire
# tracked tree. The whole arithmetic surface of the frame language rested on one
# line of one example.
#
# Its predecessor `scripts/dfcheck.sh` was worse than absent: it ran
# `examples/dataframes.helix`, a path that moved to `examples/dataframes/`, so it
# had been diffing three copies of "no such file" and reporting them identical —
# while ADR 0033:60 cited it as acceptance evidence.
#
#   Usage: scripts/dfdiff.sh [--list] [--allow FILE]
#
# Requires a DUAL-ENGINE binary. `native-df` ships by DEFAULT, so the feature to
# add is the polars oracle:
#   CARGO_TARGET_DIR=target/dual cargo build --profile gate --features dataframes
#
# A divergence not named in the allowlist is a FAILURE. The allowlist holds only
# deltas an ADR has decided, each line `<program>  # <ADR reference>`.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN="${BIN:-./target/dual/gate/helix}"
ALLOW="${ALLOW:-scripts/dfdiff-allow.txt}"
LIST_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --list) LIST_ONLY=1 ;;
    --allow) ALLOW="$2"; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

[ -x "$BIN" ] || { echo "no dual-engine binary at $BIN — build with --features dataframes" >&2; exit 1; }
# Prove it IS dual, by probing for the ENGINE THIS BUILD MIGHT LACK. A
# single-engine build REFUSES an engine it does not carry (see
# `backend::check_engine_selection`) rather than silently answering with the other
# one, so a probe is a valid dual-ness test — but only if it names the engine that
# can actually be missing.
#
# This probe asked for `native`, which was the right question while polars was the
# default. Native now ships in EVERY build, so that check passed for a native-only
# binary and this script would have compared native against itself: 129 programs,
# 0 divergences, and no information — the exact vacuous-parity failure that
# `dfcheck.sh` shipped when it diffed three copies of "no such file". Polars is
# now the optional half, so polars is what must be probed for.
if ! HELIX_DF_ENGINE=polars "$BIN" eval 'print(1)' >/dev/null 2>&1; then
  echo "REFUSING: $BIN does not accept HELIX_DF_ENGINE=polars (no oracle engine)." >&2
  echo "  Build a dual-engine binary:" >&2
  echo "    CARGO_TARGET_DIR=target/dual cargo build --profile gate --features dataframes" >&2
  exit 1
fi

# Same exclusions as scripts/capture-compat.sh, for the same reasons: bench/
# fixtures are generated rather than tracked, examples/api blocks or needs the
# network, examples/python is feature-gated.
mapfile -t files < <(git ls-files '*.helix' \
  | grep -vE '^bench/' | grep -vE '^examples/(api|python)/')

if [ "$LIST_ONLY" = 1 ]; then printf '%s\n' "${files[@]}"; exit 0; fi

declare -A allowed=()
if [ -f "$ALLOW" ]; then
  while read -r line; do
    line="${line%%#*}"; line="${line// /}"
    [ -n "$line" ] && allowed["$line"]=1
  done < "$ALLOW"
fi

run_one() { HELIX_DF_ENGINE="$1" timeout 60 "$BIN" run "$2" 2>&1; echo "EXIT:$?"; }

diffs=0; allowed_hits=0
for f in "${files[@]}"; do
  p=$(run_one polars "$f"); n=$(run_one native "$f")
  [ "$p" = "$n" ] && continue
  if [ -n "${allowed[$f]:-}" ]; then
    allowed_hits=$((allowed_hits + 1)); continue
  fi
  diffs=$((diffs + 1))
  echo "=== DIVERGES: $f"
  diff <(printf '%s\n' "$p") <(printf '%s\n' "$n") | head -12 | sed 's/^/    /'
done

echo
echo "dfdiff: ${#files[@]} programs, $diffs undeclared divergence(s), $allowed_hits allowed by $ALLOW"
[ "$diffs" -eq 0 ]

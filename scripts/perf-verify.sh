#!/usr/bin/env bash
# Performance regression gate — the "only performance improvements" proof (ADR 0016).
# Runs the bench/crosslang workloads (B1..B7) on a BASELINE and a CANDIDATE binary and
# fails (non-zero exit) if the candidate regresses either wall time or peak RSS on any
# workload beyond tolerance. Use it to compare main-vs-mimalloc, plain-vs-PGO, etc.
#
#   scripts/perf-verify.sh <baseline-helix> <candidate-helix> [--scale 1.0]
#
# Measure on CLEAN release builds (the WSL incremental bug E0463 makes incremental
# binaries unrepresentative). best-of-3 (min) suppresses noise.
set -uo pipefail
BASE="${1:?usage: perf-verify.sh <baseline-helix> <candidate-helix> [--scale N]}"
CAND="${2:?usage: perf-verify.sh <baseline-helix> <candidate-helix> [--scale N]}"
shift 2
# ABSOLUTIZE BEFORE THE cd. This script runs from bench/crosslang, so a path given
# relative to where the caller stood — which is what the usage line above invites —
# stops resolving the moment we move. Every run then fails to 999 below, and 999/999
# is 1.00, so seven dead workloads reported PASS. Measured, not hypothesized.
for _v in BASE CAND; do
  eval "_p=\$$_v"
  case "$_p" in
    /*) ;;
    *) eval "$_v=\"$(pwd)/$_p\"" ;;
  esac
done
cd "$(dirname "$0")/../bench/crosslang"

WALL_TOL=1.03   # candidate wall may be up to 3% slower (measurement noise) before it fails
WALL_MIN=0.05   # ...and only on workloads slower than this; sub-50ms timings are dominated
                # by /usr/bin/time's 10ms resolution, so their ratios are meaningless noise
RSS_TOL=1.15    # candidate peak RSS ratio breach threshold (allocators trade some RSS for speed)
RSS_ABS_MB=50   # ...but only counts as a regression if peak RSS also grows by > this many MB
                # (a custom allocator's small fixed arena overhead must not fail tiny workloads)
WORKLOADS="b1_scalar b2_pipeline b3_groupby b4_matmul b5_stats b6_vcf b7_inverse"

python3 gen_data.py "$@" >/dev/null

# PREFLIGHT. A binary that cannot run is not a "no regression" result, and finding that
# out from a ratio of two sentinels is finding it out too late.
for _b in "$BASE" "$CAND"; do
  if [ ! -x "$_b" ]; then
    echo "perf-verify: not an executable: $_b" >&2; exit 2
  fi
  if ! "$_b" run b1_scalar.helix >/dev/null 2>&1; then
    echo "perf-verify: $_b cannot run b1_scalar.helix — refusing to report a comparison" >&2
    exit 2
  fi
done

best_wall() { # <binary> <workload.helix> -> fastest of 3 wall-seconds
  for _ in 1 2 3; do
    /usr/bin/time -f '%e' -o .t "$1" run "$2" >/dev/null 2>&1 || echo 999 >.t
    cat .t
  done | sort -n | head -1
}
best_rss() { # <binary> <workload.helix> -> smallest of 3 peak-RSS (KB)
  for _ in 1 2 3; do
    /usr/bin/time -v -o .r "$1" run "$2" >/dev/null 2>&1 || true
    awk '/Maximum resident set size/{print $NF}' .r
  done | sort -n | head -1
}
ratio() { awk -v c="$1" -v b="$2" 'BEGIN{printf "%.2f", (b>0)?c/b:9}'; }

printf '%-12s %-22s %-22s\n' "workload" "wall base->cand (x)" "rss base->cand (x)"
fail=0
for w in $WORKLOADS; do
  bw=$(best_wall "$BASE" "$w.helix"); cw=$(best_wall "$CAND" "$w.helix")
  br=$(best_rss  "$BASE" "$w.helix"); cr=$(best_rss  "$CAND" "$w.helix")
  # THE SENTINEL IS A FAILURE, NOT A DATUM. `best_wall` writes 999 when a run dies, and
  # dividing one sentinel by another yields exactly 1.00 — the shape of a perfect result.
  # The preflight above should make this unreachable; it is kept because a gate whose
  # error value is also its pass value must say so at every point it could appear.
  if [ "$bw" = "999" ] || [ "$cw" = "999" ]; then
    echo "  DID NOT RUN: $w (wall sentinel 999) — this comparison is void"; fail=1; continue
  fi
  wr=$(ratio "$cw" "$bw"); rr=$(ratio "$cr" "$br")
  dkb=$((cr - br))
  printf '%-12s %ss->%ss (%s)        %sK->%sK (%s, %+dMB)\n' "$w" "$bw" "$cw" "$wr" "$br" "$cr" "$rr" "$((dkb / 1024))"
  # Wall regression only when the baseline is above the timing-noise floor.
  if awk -v b="$bw" -v f="$WALL_MIN" 'BEGIN{exit !(b>=f)}' && awk -v r="$wr" -v t="$WALL_TOL" 'BEGIN{exit !(r>t)}'; then
    echo "  REGRESSION (wall): $w (${wr}x)"; fail=1
  fi
  # An RSS regression needs BOTH a ratio breach AND a meaningful absolute increase.
  # A custom allocator carries a small FIXED working-set overhead (~tens of MB) that
  # shows as a large ratio on tiny workloads but is trivial in practice; only a
  # proportional blow-up (ratio AND > RSS_ABS_MB more memory) is a real regression.
  if awk -v r="$rr" -v t="$RSS_TOL" 'BEGIN{exit !(r>t)}' && [ "$dkb" -gt $((RSS_ABS_MB * 1024)) ]; then
    echo "  REGRESSION (rss):  $w (${rr}x, +$((dkb / 1024))MB)"
    fail=1
  fi
done
rm -f .t .r

if [ "$fail" -eq 0 ]; then
  echo "PASS — candidate has no wall/RSS regression vs baseline"
else
  echo "FAIL — candidate regressed at least one workload"
fi
exit "$fail"

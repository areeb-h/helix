#!/usr/bin/env bash
# Cross-language KERNEL suite — Helix vs C / Rust / Go / CPython / NumPy.
#
#   ./run.sh                 # verify anchors, then time everything (best of 3)
#   ./run.sh --verify        # anchors only: every language must agree, or exit 1
#   ./run.sh --scale 0.02    # 2% sizes (a quick check; anchors change with N)
#   ./run.sh k1_dot k5_montecarlo    # only these kernels
#   HELIX=/path/to/helix ./run.sh    # force a binary
#
# THE ANCHOR GATE IS THE POINT. Before any timing is reported, every language
# implementing a kernel must print the byte-identical anchor. A mismatch is a
# hard failure (exit 1) — a benchmark whose languages compute different things
# measures nothing. This script is a gate: it CAN fail, and it will.
#
# HONESTY NOTES THAT AFFECT THE NUMBERS — read bench/kernels/README.md before
# quoting anything from here:
#   * Helix ships mimalloc, which madvise()s transparent huge pages; glibc
#     malloc does not. On allocation-heavy kernels (k1) that is worth ~3.5x and
#     has nothing to do with codegen. We export GLIBC_TUNABLES for the C/Rust
#     refs to equalize it. Go has no equivalent knob — its number is NOT
#     page-size-equalized, and we say so rather than quietly leaving it out.
#   * Helix auto-parallelizes some array kernels across cores; the C/Rust/Go
#     refs are single-threaded std-only loops. We report %CPU alongside wall
#     time so a reader can see what a wall-clock win cost.
#   * Helix cannot read argv (no argv builtin). Kernels take N on STDIN via
#     read_int(); this script pipes it. A Helix kernel run with stdin left open
#     BLOCKS — always redirect.
set -uo pipefail

cd "$(dirname "$0")" || exit 1
ROOT=$(cd ../.. && pwd)
BUILD=${BUILD:-/tmp/helix_kernels_build}
mkdir -p "$BUILD"

# The Helix binary: prefer an explicit $HELIX, then release (the honest number),
# then the gate profile. A binary that predates a kernel's features fails the
# anchor gate loudly rather than silently reporting a wrong number.
pick_helix() {
  if [ -n "${HELIX:-}" ]; then echo "$HELIX"; return; fi
  for c in "$ROOT/target/release/helix" "$ROOT/target/gate/helix" "$ROOT/target/debug/helix"; do
    [ -x "$c" ] && { echo "$c"; return; }
  done
  echo ""
}
HX=$(pick_helix)
[ -z "$HX" ] && { echo "no helix binary found (build one, or set HELIX=)"; exit 1; }

# Equalize the page-size advantage for the glibc-malloc refs (see the note
# above). Without this, C/Rust pay ~195k minor faults on k1 where Helix pays
# ~1.8k, and the kernel measures allocators instead of code.
export GLIBC_TUNABLES=glibc.malloc.hugetlb=1

VERIFY_ONLY=0
SCALE=1
KERNELS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --verify) VERIFY_ONLY=1; shift ;;
    --scale) SCALE=$2; shift 2 ;;
    -*) echo "unknown option $1"; exit 1 ;;
    *) KERNELS+=("$1"); shift ;;
  esac
done
if [ ${#KERNELS[@]} -eq 0 ]; then
  KERNELS=(k1_dot k2_mandelbrot k3_basel k4_allpairs k5_montecarlo k6_sieve k7_wordcount k8_matmul k9_matmul_naive)
fi

# Per-kernel default N (the size the published table uses).
default_n() {
  case "$1" in
    k1_dot) echo 50000000 ;;
    k2_mandelbrot) echo 1200 ;;
    k3_basel) echo 100000000 ;;
    k4_allpairs) echo 15000 ;;
    k5_montecarlo) echo 100000000 ;;
    k6_sieve) echo 10000000 ;;
    k7_wordcount) echo 5000000 ;;
    # k8 is the DELEGATION pair (helix-tensor vs numpy): n=1024, where the GEMM
    # finally dominates. At n=512 you are timing interpreter startup, not math.
    k8_matmul) echo 1024 ;;
    # k9 is the NAIVE peer group (helix comprehension vs C/Rust/Go/CPython):
    # n=512, because CPython's triple loop is ~19s there and ~150s at 1024 —
    # x3 runs x2 impls is 20 minutes of no new information. Different comparison,
    # different size, so it is a SEPARATE kernel: the anchor gate requires one N
    # per kernel (different N => different anchors => nothing to compare).
    k9_matmul_naive) echo 512 ;;
    *) echo 1000 ;;
  esac
}

scaled_n() {
  local n=$1
  awk -v n="$n" -v s="$SCALE" 'BEGIN { v = int(n * s); if (v < 1) v = 1; print v }'
}

have() { command -v "$1" >/dev/null 2>&1; }

# --- build (once per kernel, into $BUILD; never timed) ---
build_kernel() {
  local k=$1
  if [ -f "$k.c" ]; then
    local cflags="-O3 -march=native"
    grep -q 'ffp-contract=off' "$k.c" && cflags="$cflags -ffp-contract=off"
    grep -q 'fwrapv' "$k.c" && cflags="$cflags -fwrapv"
    # shellcheck disable=SC2086
    gcc $cflags -o "$BUILD/$k.c.bin" "$k.c" -lm 2>/dev/null || echo "  ! C build failed: $k"
  fi
  if [ -f "$k.rs" ]; then
    rustc -O -C target-cpu=native -o "$BUILD/$k.rs.bin" "$k.rs" 2>/dev/null || echo "  ! Rust build failed: $k"
  fi
  if [ -f "$k.go" ]; then
    (cd "$BUILD" && cp "$OLDPWD/$k.go" . && go build -o "$k.go.bin" "$k.go" 2>/dev/null) || echo "  ! Go build failed: $k"
  fi
}

# --- run one implementation, print its stdout ---
run_impl() {
  local k=$1 lang=$2 n=$3
  case "$lang" in
    helix) echo "$n" | "$HX" run "$k.helix" 2>/dev/null ;;
    helix_trial) echo "$n" | "$HX" run "${k}_trial.helix" 2>/dev/null ;;
    c) "$BUILD/$k.c.bin" "$n" 2>/dev/null ;;
    rs) "$BUILD/$k.rs.bin" "$n" 2>/dev/null ;;
    go) "$BUILD/$k.go.bin" "$n" 2>/dev/null ;;
    cpython) python3 "${k}_cpython.py" "$n" 2>/dev/null ;;
    numpy) python3 "${k}_numpy.py" "$n" 2>/dev/null ;;
  esac
}

impls_for() {
  local k=$1 out=()
  [ -f "$k.helix" ] && out+=(helix)
  # k6 ships a second Helix entry: the honest pure-Helix trial-division sieve
  # beside the native-builtin one, so the delegation gap is visible, not hidden.
  [ -f "${k}_trial.helix" ] && out+=(helix_trial)
  [ -f "$k.c" ] && out+=(c)
  [ -f "$k.rs" ] && out+=(rs)
  [ -f "$k.go" ] && out+=(go)
  [ -f "${k}_cpython.py" ] && out+=(cpython)
  [ -f "${k}_numpy.py" ] && out+=(numpy)
  echo "${out[@]}"
}

# --- the gate: every implementation of a kernel must print the same anchor ---
verify_kernel() {
  local k=$1 n=$2
  local first="" first_lang="" ok=0
  for lang in $(impls_for "$k"); do
    local out
    out=$(run_impl "$k" "$lang" "$n")
    if [ -z "$out" ]; then
      printf '  %-12s %s\n' "$lang" "NO OUTPUT (build/run failed)"
      ok=1
      continue
    fi
    if [ -z "$first" ]; then
      first=$out; first_lang=$lang
      printf '  %-12s %s   <- anchor\n' "$lang" "$out"
    elif [ "$out" = "$first" ]; then
      printf '  %-12s %s\n' "$lang" "$out"
    else
      printf '  %-12s %s   *** MISMATCH vs %s (%s) ***\n' "$lang" "$out" "$first_lang" "$first"
      ok=1
    fi
  done
  return $ok
}

# --- timing: best of 3 wall-clock, with %CPU so parallelism is visible ---
# The timed command MUST be the same one the anchor gate ran, and it MUST be
# proven to have produced output: an earlier version timed a subshell that could
# not see $HX/$BUILD, so every compiled entry "ran" in 0.00s — a failure to
# launch, reported as a world record. A timing you cannot attribute to real
# output is worse than no timing, so this discards any run that printed nothing.
# Repeats are for shaking out noise on FAST entries. A deterministic kernel that
# takes 83 seconds does not get truer on the third identical run — it just costs
# four minutes. So: run once, and only repeat if the first run was quick enough
# for noise to matter (<5s). Reported time is the best of whatever ran.
time_impl() {
  local k=$1 lang=$2 n=$3
  local best="" bestcpu=""
  local outfile=/tmp/helix_kernels_out.$$
  local reps=3
  for rep in 1 2 3; do
    # After run 1, drop the extra reps if this entry is slow.
    if [ "$rep" = 2 ] && [ -n "$best" ] && awk -v b="$best" 'BEGIN{exit !(b>5)}'; then
      break
    fi
    local t wall cpu
    t=$( { /usr/bin/time -f '%e %P' bash -c \
            "export HX='$HX' BUILD='$BUILD'; $(declare -f run_impl); run_impl '$k' '$lang' '$n' > '$outfile'" ; } 2>&1 | tail -1 )
    # No output => the program did not actually run. Never time that.
    [ -s "$outfile" ] || continue
    wall=$(echo "$t" | awk '{print $1}')
    cpu=$(echo "$t" | awk '{print $2}' | tr -d '%')
    case "$wall" in ''|*[!0-9.]*) continue ;; esac
    if [ -z "$best" ] || awk -v a="$wall" -v b="$best" 'BEGIN{exit !(a<b)}'; then
      best=$wall; bestcpu=$cpu
    fi
  done
  rm -f "$outfile"
  if [ -z "$best" ]; then printf 'FAILED -'; else printf '%s %s' "$best" "$bestcpu"; fi
}

echo "helix:  $HX"
echo "build:  $BUILD   (GLIBC_TUNABLES=$GLIBC_TUNABLES)"
[ "$SCALE" != "1" ] && echo "scale:  $SCALE"
echo

FAIL=0
for k in "${KERNELS[@]}"; do
  n=$(scaled_n "$(default_n "$k")")
  echo "=== $k  (N=$n)"
  build_kernel "$k"
  if ! verify_kernel "$k" "$n"; then
    echo "  ANCHOR GATE FAILED for $k"
    FAIL=1
    continue
  fi
  [ "$VERIFY_ONLY" -eq 1 ] && { echo; continue; }
  echo "  --- best of 3 (wall seconds, %CPU) ---"
  for lang in $(impls_for "$k"); do
    read -r wall cpu <<<"$(time_impl "$k" "$lang" "$n")"
    printf '  %-12s %8ss  %5s%%CPU\n' "$lang" "$wall" "$cpu"
  done
  echo
done

if [ "$FAIL" -ne 0 ]; then
  echo "FAILED: at least one kernel's languages disagree — the suite measures nothing until that is fixed."
  exit 1
fi
echo "all anchors agree."

#!/usr/bin/env bash
# Cross-language benchmark runner — Helix vs CPython / NumPy / pandas / scipy.
# Reports the best of 3 wall-clock runs (seconds, lower is better).
#
#   ./run.sh                 # full sizes
#   ./run.sh --scale 0.05    # 5% sizes for a quick check (forwarded to gen_data.py)
#
# Notes (see README.md for the full caveats):
#   * Helix uses the release binary if built (the honest number), else debug.
#   * Python times include interpreter + library import (as a user would pay).
#   * B6 is apples-to-oranges: Helix does a full spec parse, the Python script a
#     one-field split. Not the same work.
set -uo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"

# --- locate the helix binary (release = honest perf; debug is a fallback) ------
HELIX="${HELIX:-}"
if [ -z "$HELIX" ]; then
  for cand in "$ROOT/target/release/helix" "$ROOT/target/debug/helix"; do
    [ -x "$cand" ] && HELIX="$cand" && break
  done
fi
[ -n "$HELIX" ] || { echo "no helix binary — run:  (cd $ROOT && cargo build --release)"; exit 1; }

# --- python baselines: a local venv with numpy/pandas/scipy -------------------
if [ ! -x .venv/bin/python ]; then
  echo "creating .venv (numpy/pandas/scipy)…"
  python3 -m venv .venv && ./.venv/bin/pip install -q --upgrade pip numpy pandas scipy
fi
PY=./.venv/bin/python

echo "helix:  $HELIX"
echo "python: $($PY --version 2>&1)"
"$PY" gen_data.py "$@"

# --- best-of-3 timer (min of 3 wall-times via /usr/bin/time) ------------------
timeit() {  # timeit <cmd...>  → echoes the fastest of 3 runs, in seconds
  for _ in 1 2 3; do
    /usr/bin/time -f '%e' -o .t "$@" >/dev/null 2>&1 || echo 999 > .t
    cat .t
  done | sort -n | head -1
}
hx()  { timeit "$HELIX" run "$1"; }                 # Helix, default engine (JIT+VM)
hxnj(){ timeit env HELIX_NOJIT=1 "$HELIX" run "$1"; } # Helix, bytecode VM only
py()  { timeit "$PY" "$1"; }
line(){ printf '  %-14s %8s s\n' "$1" "$2"; }

echo
echo "=== results (best of 3, wall-seconds) ==="

echo "B1  scalar loop  Σ(x² mod 1000)  10M"
line helix          "$(hx  b1_scalar.helix)"
line helix-nojit    "$(hxnj b1_scalar.helix)"
line numpy          "$(py  b1_scalar_numpy.py)"
line cpython        "$(py  b1_scalar_cpython.py)"

echo "B2  filter→map→reduce  10M"
line helix          "$(hx  b2_pipeline.helix)"
line helix-nojit    "$(hxnj b2_pipeline.helix)"
line numpy          "$(py  b2_pipeline_numpy.py)"
line cpython        "$(py  b2_pipeline_cpython.py)"

echo "B3  CSV read + groupby-mean  1M rows"
line helix          "$(hx  b3_groupby.helix)"
line pandas         "$(py  b3_groupby_pandas.py)"

echo "B4  dense matmul  1024³"
line helix          "$(hx  b4_matmul.helix)"
line numpy          "$(py  b4_matmul_numpy.py)"

echo "B5  correlation + linear regression  1M"
line helix          "$(hx  b5_stats.helix)"
line scipy          "$(py  b5_stats_scipy.py)"

echo "B6  read VCF + mean QUAL  50k   (apples-to-oranges, see README)"
line helix          "$(hx  b6_vcf.helix)"
line python-split   "$(py  b6_vcf_python.py)"

echo "B7  matrix inverse  256³   (the faer path)"
line helix          "$(hx  b7_inverse.helix)"
line numpy          "$(py  b7_inverse_numpy.py)"

rm -f .t
echo
echo "done. (debug-binary caveat: Helix's measured hot paths are release-quality —"
echo " JIT-native loops and opt-3 deps Polars/noodles/faer/matrixmultiply — even in"
echo " a debug build; build --release for the cleanest numbers.)"

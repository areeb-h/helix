#!/usr/bin/env bash
# PGO training driver. Exercises every Helix hot path so the profile-guided build
# (scripts/pgo-build.sh) optimizes the code that actually runs: the bytecode VM
# dispatch + superinstructions, the Cranelift JIT build path and its native loops,
# Polars group-by, spec VCF parsing, and the faer/ndarray linear algebra.
#
#   HELIX=/path/to/instrumented/helix ./train.sh [--scale 0.25]
#
# `--scale` is forwarded to gen_data.py (only the data-bound B3/B6 inputs scale; the
# compute-bound workloads hardcode their sizes). Profiles capture code COVERAGE, not
# data volume, so a small scale gives an equivalent profile far more cheaply.
set -euo pipefail
cd "$(dirname "$0")"
: "${HELIX:?set HELIX to the instrumented helix binary}"

python3 gen_data.py "$@"   # writes data/big.csv, data/big.vcf (stdlib only, no venv)

for w in b1_scalar b2_pipeline b3_groupby b4_matmul b5_stats b6_vcf b7_inverse; do
  echo "train: $w"
  "$HELIX" run "$w.helix" >/dev/null
  # Also profile the VM-only path (the JIT is x86_64-linux-only; the VM runs everywhere
  # and is the fallback the instrumented build must also optimize).
  HELIX_NOJIT=1 "$HELIX" run "$w.helix" >/dev/null
done
echo "training complete"

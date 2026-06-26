#!/usr/bin/env bash
# Correctness gate: every language must print identical anchor lines per workload.
# Run from bench/stress/.  Usage: bash verify.sh
set -uo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"
HX="${HELIX:-$ROOT/target/debug/helix}"
PY="${PYBIN:-../crosslang/.venv/bin/python}"   # numpy/pandas/scipy venv
TMP=/tmp/hxstress
mkdir -p "$TMP"

hr() { printf '\n######## %s ########\n' "$1"; }

hr "S1  k-mers (k=10)"
echo "--- helix ---";   "$HX" run s1_kmers.helix
echo "--- cpython ---"; python3 s1_kmers_cpython.py
echo "--- rust ---";    rustc -O s1_kmers.rs -o "$TMP/s1" 2>/dev/null && "$TMP/s1"
echo "--- go ---";      go run s1_kmers.go

hr "S2  fastq GC + Phred"
echo "--- helix ---";   "$HX" run s2_fastq.helix
echo "--- cpython ---"; python3 s2_fastq_cpython.py
echo "--- rust ---";    rustc -O s2_fastq.rs -o "$TMP/s2" 2>/dev/null && "$TMP/s2"
echo "--- go ---";      go run s2_fastq.go

hr "S3  VCF filter+group+count"
echo "--- helix ---";   "$HX" run s3_vcf.helix
echo "--- pandas ---";  "$PY" s3_vcf_pandas.py
echo "--- cpython ---"; python3 s3_vcf_cpython.py
echo "--- rust ---";    rustc -O s3_vcf.rs -o "$TMP/s3" 2>/dev/null && "$TMP/s3"
echo "--- go ---";      go run s3_vcf.go

hr "S4  pattern matching (Σ weights, 20M)"
echo "--- helix ---";   "$HX" run s4_match.helix
echo "--- cpython ---"; python3 s4_match_cpython.py
echo "--- numpy ---";   "$PY" s4_match_numpy.py
echo "--- rust ---";    rustc -O s4_match.rs -o "$TMP/s4" 2>/dev/null && "$TMP/s4"
echo "--- go ---";      go run s4_match.go

hr "S5  fused polynomial pipeline (50M)"
echo "--- helix ---";   "$HX" run s5_fused.helix
echo "--- cpython ---"; python3 s5_fused_cpython.py
echo "--- numpy ---";   "$PY" s5_fused_numpy.py
echo "--- rust ---";    rustc -O s5_fused.rs -o "$TMP/s5" 2>/dev/null && "$TMP/s5"
echo "--- go ---";      go run s5_fused.go

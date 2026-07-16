#!/usr/bin/env bash
# Cross-language STRESS suite — Helix vs CPython / NumPy / pandas / Rust / Go.
# Reports the best of 3 wall-clock runs (seconds, lower is better). Correctness
# is gated separately by verify.sh (every language prints identical anchors).
#
#   ./run.sh                 # full sizes
#   ./run.sh --scale 0.1     # 10% sizes (forwarded to gen.py)
#
# Each language uses its natural idiom: Helix one-liners on the new feature set,
# Rust/Go hand-written std-only loops (compiled, then timed), Python as a user
# pays it (interpreter + library import included). See README.md.
set -uo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"
TMP=/tmp/hxstress
mkdir -p "$TMP"

# --- helix binary: release if it exists & runs, else debug --------------------
HELIX="${HELIX:-}"
if [ -z "$HELIX" ]; then
  for c in "$ROOT/target/release/helix" "$ROOT/target/debug/helix"; do
    [ -x "$c" ] && HELIX="$c" && break
  done
fi
[ -n "$HELIX" ] || { echo "no helix binary — (cd $ROOT && cargo build)"; exit 1; }

# --- python baselines: reuse the crosslang venv (numpy/pandas/scipy) ----------
PY=../crosslang/.venv/bin/python
if [ ! -x "$PY" ]; then
  echo "creating venv (numpy/pandas)…"
  python3 -m venv ../crosslang/.venv && ../crosslang/.venv/bin/pip -q install numpy pandas scipy
fi

echo "helix:  $HELIX"
echo "python: $($PY --version 2>&1) / system $(python3 --version 2>&1)"
echo "rustc:  $(rustc --version)"
echo "go:     $(go version)"
"$PY" gen.py "$@"

# --- compile the Rust + Go binaries once (timed runs use the built binary) -----
echo "compiling rust + go…"
for s in s1_kmers s2_fastq s3_vcf s4_match s5_fused s7_align s8_canonical; do
  rustc -O "$s.rs" -o "$TMP/${s}_rs" 2>/dev/null
  go build -o "$TMP/${s}_go" "$s.go" 2>/dev/null
done

timeit() {  # best of 3 wall-seconds
  for _ in 1 2 3; do
    /usr/bin/time -f '%e' -o "$TMP/.t" "$@" >/dev/null 2>&1 || echo 999 > "$TMP/.t"
    cat "$TMP/.t"
  done | sort -n | head -1
}
hx()   { timeit "$HELIX" run "$1"; }
hxnj() { timeit env HELIX_NOJIT=1 "$HELIX" run "$1"; }
py()   { timeit "$PY" "$1"; }
cpy()  { timeit python3 "$1"; }
bin()  { timeit "$TMP/$1"; }
line() { printf '  %-16s %9s s\n' "$1" "$2"; }

echo
echo "=== results (best of 3, wall-seconds; lower is better) ==="

echo "S1  k-mer spectrum k=10  (10 Mbp genome)  [bio flagship: native kmer_counts]"
line helix         "$(hx  s1_kmers.helix)"
line cpython       "$(cpy s1_kmers_cpython.py)"
line rust          "$(bin s1_kmers_rs)"
line go            "$(bin s1_kmers_go)"

echo "S2  FASTQ GC + Phred  (200k reads)  [bio readers + Dna/phred methods]"
line helix         "$(hx  s2_fastq.helix)"
line cpython       "$(cpy s2_fastq_cpython.py)"
line rust          "$(bin s2_fastq_rs)"
line go            "$(bin s2_fastq_go)"

echo "S3  VCF filter+group+count  (200k variants)  [@column dataframe verbs]"
line helix         "$(hx  s3_vcf.helix)"
line pandas        "$(py  s3_vcf_pandas.py)"
line cpython       "$(cpy s3_vcf_cpython.py)"
line rust          "$(bin s3_vcf_rs)"
line go            "$(bin s3_vcf_go)"

echo "S4  pattern matching  (Σ weights over 20M)  [match: or-patterns + guards]"
line helix         "$(hx  s4_match.helix)"
line helix-nojit   "$(hxnj s4_match.helix)"
line cpython       "$(cpy s4_match_cpython.py)"
line numpy         "$(py  s4_match_numpy.py)"
line rust          "$(bin s4_match_rs)"
line go            "$(bin s4_match_go)"

echo "S5  fused polynomial pipeline  (50M)  [JIT fusion: helper-fn inlined, 0 alloc]"
line helix         "$(hx  s5_fused.helix)"
line helix-nojit   "$(hxnj s5_fused.helix)"
line cpython       "$(cpy s5_fused_cpython.py)"
line numpy         "$(py  s5_fused_numpy.py)"
line rust          "$(bin s5_fused_rs)"
line go            "$(bin s5_fused_go)"

echo "S7  local alignment  (100 reads vs a gene)  [native Gotoh affine-gap DP]"
line helix         "$(hx  s7_align.helix)"
line cpython       "$(cpy s7_align_cpython.py)"
line rust          "$(bin s7_align_rs)"
line go            "$(bin s7_align_go)"

echo "S8  canonical k-mers k=10  (10 Mbp)  [strand-agnostic canonical_kmer_counts]"
line helix         "$(hx  s8_canonical.helix)"
line cpython       "$(cpy s8_canonical_cpython.py)"
line rust          "$(bin s8_canonical_rs)"
line go            "$(bin s8_canonical_go)"

rm -f "$TMP/.t"
echo
echo "done.  (S6 interop is a capability demo, not a race — see README; it needs"
echo "        cargo run --features python -- run s6_interop.helix)"

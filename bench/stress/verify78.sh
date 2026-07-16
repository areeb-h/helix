#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"
HX="${HELIX:-$ROOT/target/debug/helix}"
TMP=/tmp/hxstress
mkdir -p "$TMP"

hr() { printf '\n######## %s ########\n' "$1"; }

hr "S7  local alignment"
echo "--- helix ---";   "$HX" run s7_align.helix
echo "--- cpython ---"; python3 s7_align_cpython.py
echo "--- rust ---";    rustc -O s7_align.rs -o "$TMP/s7" 2>/dev/null && "$TMP/s7"
echo "--- go ---";      go run s7_align.go

hr "S8  canonical k-mers"
echo "--- helix ---";   "$HX" run s8_canonical.helix
echo "--- cpython ---"; python3 s8_canonical_cpython.py
echo "--- rust ---";    rustc -O s8_canonical.rs -o "$TMP/s8" 2>/dev/null && "$TMP/s8"
echo "--- go ---";      go run s8_canonical.go

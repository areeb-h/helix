#!/usr/bin/env bash
# Array-reduce benchmark: accumulate (s+i) % 1e9+7 over 10,000,000 elements.
# A fair "iterate + non-trivial recurrence" test (resists closed-form), with the
# same integer result in every language. Helix uses reduce over a range — which
# currently runs on the TREE-WALKER (comprehensions aren't in the VM yet), so this
# honestly shows the in-memory data-path gap the VM widening is meant to close.
cd "$(dirname "$0")/.."
set -e
gcc -O2 -o bench/sum_c bench/sum.c
go build -o bench/sum_go bench/sum.go

bestof() {
  local label="$1"; shift
  local best=99999
  for i in 1 2 3; do
    local t
    t=$( { /usr/bin/time -f "%e" "$@" >/dev/null; } 2>&1 )
    awk -v a="$t" -v b="$best" 'BEGIN{exit !(a<b)}' && best="$t"
  done
  printf "%-22s %ss\n" "$label" "$best"
}

echo "== correctness (all should match) =="
echo "  C:      $(./bench/sum_c)"
echo "  Helix:  $(./target/release/helix bench/sum.helix)"
echo "== reduce over 10M — best of 3, wall-clock seconds =="
bestof "C (gcc -O2)"       ./bench/sum_c
bestof "Go"                ./bench/sum_go
bestof "Node / JS"         node bench/sum.js
bestof "Python 3"          python3 bench/sum.py
bestof "Helix (JIT loop)"  ./target/release/helix bench/sum.helix
bestof "Helix (VM loops)"  env HELIX_NOJIT=1 ./target/release/helix bench/sum.helix
bestof "Helix (treewalk)"  env HELIX_NOVM=1 ./target/release/helix bench/sum.helix

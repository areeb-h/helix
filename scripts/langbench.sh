#!/usr/bin/env bash
# Cross-language fib(35) microbenchmark — pure scalar recursion (~29.9M calls).
# Measures the language's CALL/CONTROL-FLOW core, not any data/vector path.
cd "$(dirname "$0")/.."
set -e

echo "== compiling C (-O2) and Go =="
gcc -O2 -o bench/fib_c bench/fib.c
go build -o bench/fib_go bench/fib.go

bestof() { # $1=label, rest=command
  local label="$1"; shift
  local best=99999
  for i in 1 2 3; do
    local t
    t=$( { /usr/bin/time -f "%e" "$@" >/dev/null; } 2>&1 )
    awk -v a="$t" -v b="$best" 'BEGIN{exit !(a<b)}' && best="$t"
  done
  printf "%-22s %ss\n" "$label" "$best"
}

echo "== fib(35) — best of 3, wall-clock seconds =="
bestof "C (gcc -O2)"           ./bench/fib_c
bestof "Helix (Cranelift JIT)" ./target/release/helix bench/fib.helix
bestof "Go (compiled)"         ./bench/fib_go
bestof "Node/JS (V8 JIT)"      node bench/fib.js
bestof "Python 3"              python3 bench/fib.py
bestof "Helix (bytecode VM)"   env HELIX_NOJIT=1 ./target/release/helix bench/fib.helix
bestof "Helix (tree-walker)"   env HELIX_NOVM=1 ./target/release/helix bench/fib.helix

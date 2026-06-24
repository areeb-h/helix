#!/usr/bin/env bash
# Find the recursion depth at which the eval thread's stack overflows.
cd "$(dirname "$0")/.."
BIN=./target/debug/helix
for d in 30000 50000 70000 85000 95000; do
  printf 'fn count(n) = if n <= 0 then 0 else count(n - 1)\nprint(count(%d))\n' "$d" > /tmp/d.helix
  out=$("$BIN" /tmp/d.helix 2>&1 | tail -1)
  echo "count($d): $out"
done

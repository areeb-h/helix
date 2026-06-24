#!/usr/bin/env bash
# Every example must produce byte-identical output on the VM and the tree-walker.
cd "$(dirname "$0")/.."
BIN=./target/debug/helix
fail=0
for f in examples/*.helix; do
  a=$("$BIN" "$f" 2>&1)
  b=$(HELIX_NOVM=1 "$BIN" "$f" 2>&1)
  if [ "$a" = "$b" ]; then
    echo "ok   $(basename "$f")"
  else
    echo "DIFF $(basename "$f")"
    fail=1
  fi
done
echo "RESULT=$fail"

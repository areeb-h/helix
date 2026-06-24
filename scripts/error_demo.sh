#!/usr/bin/env bash
# Exercises Helix's error paths. Run from the project root.
set -u
cd "$(dirname "$0")/.."
cargo build -q

BIN=./target/debug/helix
TMP=/tmp/helix_err.helix

run() {
  echo "### $1"
  printf '%s\n' "$2" > "$TMP"
  "$BIN" "$TMP" 2>&1
  echo
}

run "immutable reassignment" $'x = 1\nx = 2'
run "method typo suggestion" $'xs = [1, 2, 3]\nxs.maen()'
run "no truthiness" 'ok = 5 and true'
run "invalid DNA base" 'seq = dna("ATBX")'
run "undefined name suggestion" $'counts = [1, 2]\nprint(conts)'
run "division by zero" 'y = 1 / 0'
run "index out of bounds" $'xs = [1, 2, 3]\nprint(xs[9])'
run "missing parens on method" $'xs = [1, 2, 3]\nxs.mean'

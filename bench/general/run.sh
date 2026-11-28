#!/bin/bash
# run.sh BIN [NAME...] — instructions executed (callgrind `Ir`) per workload, one line each.
#
# Instruction counts, not wall time: they are the same on every run to within a few
# thousand, so two builds can be compared on a shared machine. Needs valgrind.
# Run from anywhere; BIN is a helix binary (use an absolute path).
set -u
BIN=${1:?usage: run.sh BIN [NAME...]}
shift
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

ir() { # ir NAME DIR ARGS...
  local name=$1 dir=$2
  shift 2
  local out
  out=$(cd "$dir" && valgrind --tool=callgrind --callgrind-out-file=/dev/null "$BIN" "$@" 2>&1 >/dev/null </dev/null)
  echo "$name $(echo "$out" | grep -o 'refs: *[0-9,]*' | tr -d ', ' | cut -d: -f2)"
}

want() { [ $# -eq 1 ] && return 0; local n=$1; shift; for w in "$@"; do [ "$w" = "$n" ] && return 0; done; return 1; }

for f in records nested strings closures match dicts methods calls app; do
  want $f "$@" && ir $f "$HERE" run $f.helix
done
# Real programs from examples/: an app replayed through `ui.replay`, and rule code checked
# over every reachable state (`std.model`).
want gallery "$@" && ir gallery "$ROOT" run examples/apps/gallery.helix --replay
want orbit "$@" && ir orbit "$ROOT" run examples/apps/orbit.helix --replay
want freezer_model "$@" && ir freezer_model "$ROOT/examples/rules" run freezer_model_test.helix
want updates_model "$@" && ir updates_model "$ROOT/examples/rules" run updates_model_test.helix
want lockout_model "$@" && ir lockout_model "$ROOT/examples/rules" run lockout_model_test.helix
true

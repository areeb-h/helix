#!/usr/bin/env bash
# Every example must succeed, and produce byte-identical output and the same exit status, on
# the JIT, the VM and the tree-walker.
#
# One by-design difference is NOT exercised here (no example triggers it): the VM
# keeps call frames on the heap (1M-deep) while the tree-walker recurses on the
# native stack (20k-deep `MAX_CALL_DEPTH`), so a function recursing in (20k, 1M]
# succeeds on the VM and is rejected by the tree-walker. The differential fuzzer and
# the `recursion_depth_is_a_by_design_engine_difference` unit test pin this as
# accepted agreement (B2); keep example programs' recursion well under 20k.
cd "$(dirname "$0")/.."
# Binary under test — overridable so the gate can reuse whatever profile it just built
# (e.g. BIN=./target/gate/helix). Defaults to the debug binary.
BIN="${BIN:-./target/debug/helix}"
fail=0
# The runnable, self-contained example categories. Excludes examples/{data,modules,
# python,api}: data holds fixtures (not programs), modules is an import demo, and
# python/api need optional features / network.
#
# ALL THREE ENGINES, AND THEIR EXIT STATUS. This compared the default engine's combined
# output against the tree-walker's and nothing else, so an example that exited 0 on one
# engine and 42 on another with the same output passed (reproduced by an outside review
# with a stub binary), as did one that failed identically everywhere. A runnable example
# must now SUCCEED, and the JIT (default), the VM (`HELIX_NOJIT=1`) and the tree-walker
# (`HELIX_NOVM=1`) must agree on output and on status.
for f in $(find examples/language examples/numerics examples/dataframes examples/statistics examples/bio -name '*.helix' | sort); do
  a=$("$BIN" "$f" 2>&1); ca=$?
  v=$(HELIX_NOJIT=1 "$BIN" "$f" 2>&1); cv=$?
  b=$(HELIX_NOVM=1 "$BIN" "$f" 2>&1); cb=$?
  if [ "$ca" -ne 0 ] || [ "$cv" -ne 0 ] || [ "$cb" -ne 0 ]; then
    echo "FAIL $(basename "$f") — exit jit=$ca vm=$cv walker=$cb (a runnable example must succeed)"
    fail=1
  elif [ "$a" != "$v" ] || [ "$a" != "$b" ]; then
    echo "DIFF $(basename "$f")"
    fail=1
  else
    echo "ok   $(basename "$f")"
  fi
done
echo "RESULT=$fail"
# ...and EXIT with it. Printing the verdict is not reporting it: nothing anywhere parsed
# `RESULT=` (the line above was its only occurrence in the repo), the gate piped this
# script to `tail` without capturing a status, and CI ran it as a bare step — which
# passes whenever the script exits 0, i.e. always. So a JIT-vs-tree-walker divergence
# across every example would print `DIFF …`, print `RESULT=1`, and leave both the local
# gate and CI green.
#
# That is the THIRD gate in this repo found unable to fail: `dfcheck.sh` diffed three
# copies of "no such file", and 28 `native-df` tests were executed by nothing. The
# lesson each time is the same — a gate has to be sabotaged once to prove it can fail.
exit "$fail"

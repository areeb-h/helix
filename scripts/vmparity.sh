#!/usr/bin/env bash
# Every example must produce byte-identical output on the VM and the tree-walker.
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
for f in $(find examples/language examples/numerics examples/dataframes examples/statistics examples/bio -name '*.helix' | sort); do
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

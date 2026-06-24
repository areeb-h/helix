#!/usr/bin/env bash
cd "$(dirname "$0")/.."
BIN=./target/debug/helix
HELIX_NOVM=1 "$BIN" examples/dataframes.helix > /tmp/df_tw1.txt 2>&1
HELIX_NOVM=1 "$BIN" examples/dataframes.helix > /tmp/df_tw2.txt 2>&1
"$BIN" examples/dataframes.helix > /tmp/df_vm.txt 2>&1
if diff -q /tmp/df_tw1.txt /tmp/df_tw2.txt >/dev/null; then
  echo "tree-walker is STABLE across runs"
else
  echo "tree-walker is NONDETERMINISTIC across runs (Polars row order)"
fi
echo "--- diff: default(VM-preferred) vs tree-walker ---"
diff /tmp/df_vm.txt /tmp/df_tw1.txt | head -20
echo "(end diff)"

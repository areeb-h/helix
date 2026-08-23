#!/usr/bin/env bash
# The iteration gate: clippy + the full test suite + the VM/tree-walker parity diff,
# run on a fast profile. This is the check to run after every change.
#
# WHY THIS EXISTS: `[profile.release]` is a *shipping* profile — `lto = "fat"` +
# `codegen-units = 1` links the whole dependency tree (Polars, noodles, cranelift) as
# one LLVM unit, which is ~20 min and ~4 GB in a single rustc. Running `cargo test
# --release` for routine gating paid that cost on every build. The `gate` profile
# (Cargo.toml) keeps `opt-level = 3` — so the differential fuzzers and perf tests run
# at full speed — but drops LTO and parallelises codegen, making a rebuild minutes
# faster and memory-light. Optimization level does not change any test's pass/fail, so
# this is a faithful gate; `release` is only needed to validate the actual shipped
# binary.
#
#   Usage: scripts/gate.sh [gate|dev|release]     (default: gate)
#     gate    - opt-3, no LTO (fast + full-speed tests)   <- use this normally
#     dev     - unoptimized crate (fastest compile; slower fuzzers)
#     release - the real shipping build (fat-LTO, slow)   <- pre-ship only
set -uo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-gate}"
case "$PROFILE" in
  gate)    TEST_ARGS=(--profile gate); BIN=./target/gate/helix ;;
  dev)     TEST_ARGS=();               BIN=./target/debug/helix ;;
  release) TEST_ARGS=(--release);      BIN=./target/release/helix ;;
  *) echo "unknown profile '$PROFILE' (use gate|dev|release)"; exit 2 ;;
esac

# Use mold if it's installed — a faster linker for the iteration loop. Scoped to this
# script on purpose: the shipping `release` build (and any machine/CI without mold)
# keeps the default linker, so this is a pure local speedup with zero portability risk.
if command -v mold >/dev/null 2>&1; then
  export RUSTFLAGS="${RUSTFLAGS:-} -Clink-arg=-fuse-ld=mold"
fi

rc=0
T0=$SECONDS
PHASE_T=$SECONDS
log() {
  printf '\n=== %s (prev phase %ss, total %ss) ===\n' "$1" "$((SECONDS - PHASE_T))" "$((SECONDS - T0))"
  PHASE_T=$SECONDS
}

# GATE_QUICK=1: the mid-iteration loop — clippy + lib tests only. LOUDLY not
# the merge bar (no CLI suite, no parity diff, no checkall); the full gate is
# still the only thing allowed to call a change done.
if [ "${GATE_QUICK:-0}" = "1" ]; then
  echo "== QUICK LOOP: lib tests only — NOT the merge bar =="
  cargo clippy --all-targets -- -D warnings 2>&1 | tail -2 || rc=1
  cargo test "${TEST_ARGS[@]}" --lib 2>&1 | grep -E "test result:|FAILED|panicked" | tail -6 || rc=1
  echo "QUICK_RC=$rc (run the full gate before calling it done)"
  exit "$rc"
fi

# `-D warnings`, EXACTLY AS CI RUNS IT. Without it this step reported and moved on, so a
# change could be green here and red there — which is precisely what happened: threading two
# parameters into a JIT analysis pushed it to 8 arguments, `too_many_arguments` fired only in
# CI, and the local gate had said RC=0. A gate that disagrees with the gate is not a gate.
log "clippy (--all-targets -D warnings)"
CLOG=$(mktemp)
cargo clippy --all-targets -- -D warnings >"$CLOG" 2>&1 || rc=1
grep -E "^error|^warning:" "$CLOG" | tail -8 || true
tail -1 "$CLOG"
rm -f "$CLOG"

log "tests ($PROFILE)"
TLOG=$(mktemp)
cargo test "${TEST_ARGS[@]}" >"$TLOG" 2>&1 || rc=1
grep -E "test result:|FAILED|error\[|panicked" "$TLOG" | tail -25
rm -f "$TLOG"

log "vmparity (BIN=$BIN)"
if [ ! -x "$BIN" ]; then cargo build "${TEST_ARGS[@]}" >/dev/null 2>&1 || rc=1; fi
BIN="$BIN" bash scripts/vmparity.sh 2>&1 | tail -2

# Everything above RUNS its programs, so none of it covers a `bench/` program that needs
# a generated fixture before it will start — which is how nine of them rotted against
# ADR-0017's namespace flattening until a release build tripped over one. ~30 ms.
log "checkall (every .helix type-checks)"
BIN="$BIN" bash scripts/checkall.sh 2>&1 | tail -1 || rc=1

log "GATE_RC=$rc"
exit "$rc"

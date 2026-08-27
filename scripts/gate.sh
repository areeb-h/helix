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

# THE DUAL-ENGINE DIFFERENTIAL CAMPAIGN. Until now this ran NOWHERE: `native-df`
# is not in Cargo.toml's `default`, this script runs a bare `cargo test`, and CI's
# only native-df step was a clippy WITHOUT `--all-targets` — so the test targets
# were never even compiled. 28 `#[test]` in src/backend/native/tests.rs, including
# every `mod against_the_oracle` comparison against the polars oracle, were written,
# reviewed, committed, and then executed by nothing. docs/testing.md told readers
# they ran through this script.
#
# Scoped to `backend::native` and given its OWN target dir: the feature set differs
# from the main build, so sharing a target dir would make the two invocations evict
# each other's cache on every gate run. ~47s warm, nearly all of it the crate
# recompile; the tests themselves are 0.02s.
# CLIPPY ON THE OTHER FEATURE SET. `src/backend/native/` is `#[cfg]`-ed out of a default
# build, so the lint step above never sees a line of it — and a lint error there is a CI
# failure that a green local gate cannot predict. That happened TWICE in one day (a
# collapsible `if`, both times), which is the definition of a gate that does not gate.
# `target/dual` is the same directory the two steps below use, so this is a link, not a
# rebuild.
#
# `--no-default-features --features appliance` is deliberately NOT here: it shares no
# build artifacts with either target dir, so it would mean a third full compile of the
# crate on every gate run. It stays a CI-only check; this catches the overlapping
# majority, which is every line of the native backend.
log "clippy (--features native-df -D warnings)"
CLOG=$(mktemp)
CARGO_TARGET_DIR=target/dual cargo clippy --all-targets --features native-df -- -D warnings >"$CLOG" 2>&1 || rc=1
grep -E "^error|^warning:" "$CLOG" | tail -5 || true
rm -f "$CLOG"

log "native-df differential (the dual-engine campaign)"
CARGO_TARGET_DIR=target/dual cargo test "${TEST_ARGS[@]}" --features native-df --bins backend::native >"$TLOG" 2>&1 || rc=1
grep -E "test result:|FAILED|error\[|panicked" "$TLOG" | tail -5
rm -f "$TLOG"

# ...AND THEN THE WHOLE-PROGRAM DIFF, which until now ran nowhere either. ADR 0036
# created `dfdiff.sh` precisely because the verb-level campaign above was green while
# SIXTEEN semantic divergences were live: the deltas hid in expression shapes no verb
# test built. docs/testing.md and docs/execution-engine.md both describe it as a gate.
# It was not one — it ran only when a human typed it, which is the same shape as the
# 28 tests above being "executed by nothing", one layer up.
#
# The test step does NOT produce the binary this needs (`cargo test --bins` builds the
# test harness, not the plain bin), so the link is explicit. Measured on this box: 363ms
# to link against the already-warm deps, 2.9s for 120 programs under both backends.
log "dfdiff (every tracked program under BOTH DataFrame backends)"
CARGO_TARGET_DIR=target/dual cargo build "${TEST_ARGS[@]}" --features native-df >/dev/null 2>&1 || rc=1
bash scripts/dfdiff.sh 2>&1 | tail -2 || rc=1

log "vmparity (BIN=$BIN)"
if [ ! -x "$BIN" ]; then cargo build "${TEST_ARGS[@]}" >/dev/null 2>&1 || rc=1; fi
# `|| rc=1` is load-bearing and was missing: `pipefail` makes the pipeline carry
# vmparity's status, but nothing assigned it to `rc`, so the phase could not fail the
# gate even once the script itself started exiting non-zero. `checkall` one phase below
# always had it; this line did not.
BIN="$BIN" bash scripts/vmparity.sh 2>&1 | tail -2 || rc=1

# Everything above RUNS its programs, so none of it covers a `bench/` program that needs
# a generated fixture before it will start — which is how nine of them rotted against
# ADR-0017's namespace flattening until a release build tripped over one. ~30 ms.
log "checkall (every .helix type-checks)"
BIN="$BIN" bash scripts/checkall.sh 2>&1 | tail -1 || rc=1

log "GATE_RC=$rc"
exit "$rc"

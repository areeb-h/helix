#!/usr/bin/env bash
# Local profile-guided (PGO) release build of `helix` for x86_64-unknown-linux-gnu —
# the only target where the Cranelift JIT (the hottest code PGO most rewards) exists
# and where cargo-pgo is fully supported natively.
#
#   scripts/pgo-build.sh [--scale 0.25]
#
# Builds CLEAN to dodge the WSL incremental-corruption bug (E0463); expect ~10 min+
# (Polars rebuild). The instrument phase uses thin LTO (cargo-pgo default; the
# instrumented binary is throwaway); the optimize phase keeps the project's fat LTO +
# codegen-units=1 + opt-level=3 + panic=abort + strip and adds -Cprofile-use. Because
# `helix` is a bin crate (no lib/dylib), the PGO×fat-LTO bug (rust#117220) does not
# apply. Output: target/x86_64-unknown-linux-gnu/release/helix
set -euo pipefail
cd "$(dirname "$0")/.."
TGT=x86_64-unknown-linux-gnu
SCALE="${1:-}"

command -v cargo-pgo >/dev/null 2>&1 || cargo install cargo-pgo
rustup component add llvm-tools-preview >/dev/null 2>&1 || true

echo "==> clean (avoids WSL incremental E0463)"
cargo clean --target "$TGT"

echo "==> instrument build (thin-LTO, throwaway)"
cargo pgo build -- --target "$TGT"

echo "==> train (profile collection)"
HELIX="$PWD/target/$TGT/release/helix" bench/crosslang/train.sh ${SCALE:+--scale "$SCALE"}

echo "==> optimize build (PGO + fat LTO; the shipped artifact)"
cargo pgo optimize build -- --target "$TGT"

echo "==> done: target/$TGT/release/helix"

#!/usr/bin/env sh
# Helix installer.
#
#   curl -LsSf https://raw.githubusercontent.com/<owner>/helix/main/install.sh | sh
#
# Installs a self-contained `helix` binary onto your PATH. With a published
# release it downloads the prebuilt binary for your platform; otherwise (or with
# HELIX_FROM_SOURCE=1) it builds from source with cargo. No Python or other
# runtime is required for the core language.
#
# Env knobs:
#   HELIX_INSTALL_DIR   where to put the binary (default: $HOME/.local/bin)
#   HELIX_VERSION       release tag to fetch (default: latest)
#   HELIX_FROM_SOURCE   set to 1 to force a source build
#   HELIX_REPO          owner/name on GitHub (default: areeb/helix)

set -eu

REPO="${HELIX_REPO:-areeb/helix}"
INSTALL_DIR="${HELIX_INSTALL_DIR:-$HOME/.local/bin}"
BIN="helix"

say() { printf '%s\n' "helix-install: $*" >&2; }
die() { say "error: $*"; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

detect_target() {
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os" in
    Linux)  os_t="unknown-linux-gnu" ;;
    Darwin) os_t="apple-darwin" ;;
    *) die "unsupported OS '$os' — on Windows use the PowerShell installer or scoop (see docs)." ;;
  esac
  case "$arch" in
    x86_64|amd64) arch_t="x86_64" ;;
    arm64|aarch64) arch_t="aarch64" ;;
    *) die "unsupported architecture '$arch'." ;;
  esac
  printf '%s-%s' "$arch_t" "$os_t"
}

install_binary_from() {
  # $1 = path to a built/downloaded `helix` executable
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$1" "$INSTALL_DIR/$BIN" 2>/dev/null || { cp "$1" "$INSTALL_DIR/$BIN"; chmod 0755 "$INSTALL_DIR/$BIN"; }
  say "installed $BIN -> $INSTALL_DIR/$BIN"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) : ;;
    *) say "note: add $INSTALL_DIR to your PATH, e.g.  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
  esac
  "$INSTALL_DIR/$BIN" version || true
}

from_release() {
  target="$(detect_target)"
  ver="${HELIX_VERSION:-latest}"
  if [ "$ver" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
  else
    base="https://github.com/$REPO/releases/download/$ver"
  fi
  asset="helix-$target.tar.gz"
  url="$base/$asset"
  tmp="$(mktemp -d)"
  say "downloading $url"
  if have curl; then curl -fSL "$url" -o "$tmp/$asset" 2>/dev/null || return 1
  elif have wget; then wget -q "$url" -O "$tmp/$asset" || return 1
  else return 1; fi
  tar -xzf "$tmp/$asset" -C "$tmp" || return 1
  install_binary_from "$tmp/$BIN"
  rm -rf "$tmp"
}

from_source() {
  have cargo || die "no prebuilt binary available and cargo isn't installed — install Rust from https://rustup.rs first."
  [ -f Cargo.toml ] || die "run this from a Helix checkout (no Cargo.toml here), or wait for a published release."
  say "building from source (cargo build --release)…"
  cargo build --release
  install_binary_from "target/release/$BIN"
}

main() {
  if [ "${HELIX_FROM_SOURCE:-0}" = "1" ]; then
    from_source
  elif from_release; then
    :
  else
    say "no matching prebuilt release; falling back to a source build."
    from_source
  fi
  say "done. try:  helix eval \"print(1 + 2)\"   or   helix repl"
}

main "$@"

#!/usr/bin/env sh
# Helix installer.
#
#   curl -LsSf https://helix-lang.dev/install.sh | sh
#   curl -LsSf https://raw.githubusercontent.com/areeb-h/helix/main/install.sh | sh
#
# Installs a self-contained `helix` binary onto your PATH. With a published
# release it downloads the prebuilt binary for your platform and VERIFIES its
# SHA-256 before installing; otherwise (or with HELIX_FROM_SOURCE=1) it builds
# from source with cargo. No Python or other runtime is required.
#
# Env knobs:
#   HELIX_INSTALL_DIR   where to put the binary (default: $HOME/.local/bin)
#   HELIX_VERSION       release tag to fetch (default: latest)
#   HELIX_FROM_SOURCE   set to 1 to force a source build
#   HELIX_MUSL          set to 1 to fetch the fully-static musl binary on x86_64 Linux
#                       (maximum portability / air-gapped; the default gnu build is PGO)
#   HELIX_NO_VERIFY     set to 1 to skip checksum verification (NOT recommended)
#   HELIX_REPO          owner/name on GitHub (default: areeb-h/helix)

set -eu

REPO="${HELIX_REPO:-areeb-h/helix}"
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
    # Git Bash / MSYS2 / Cygwin report these. They CAN run this script, but the
    # release asset is a .zip with helix.exe, so send them to the native installer
    # rather than half-working here.
    MINGW*|MSYS*|CYGWIN*)
      die "on Windows use PowerShell:  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
    *) die "unsupported OS '$os'." ;;
  esac
  case "$arch" in
    x86_64|amd64) arch_t="x86_64" ;;
    arm64|aarch64) arch_t="aarch64" ;;
    *) die "unsupported architecture '$arch' — build from source: HELIX_FROM_SOURCE=1" ;;
  esac
  printf '%s-%s' "$arch_t" "$os_t"
}

# Verify $1 (a file) against the `sha256sum`-format line for $2 (its basename) in
# $3 (a SHA256SUMS file). Returns non-zero on mismatch OR when no hashing tool
# exists — the caller decides, and it decides to abort: a `curl | sh` installer
# that cannot check what it downloaded should say so, not shrug.
verify_sha256() {
  file="$1"; name="$2"; sums="$3"
  want="$(grep -E "[[:space:]]\*?${name}\$" "$sums" 2>/dev/null | awk '{print $1}' | head -1)"
  [ -n "$want" ] || { say "no checksum for $name in SHA256SUMS"; return 1; }
  if have sha256sum; then got="$(sha256sum "$file" | awk '{print $1}')"
  elif have shasum; then got="$(shasum -a 256 "$file" | awk '{print $1}')"
  elif have openssl; then got="$(openssl dgst -sha256 "$file" | awk '{print $NF}')"
  else say "no sha256sum/shasum/openssl available to verify the download"; return 1; fi
  [ "$got" = "$want" ] || { say "CHECKSUM MISMATCH for $name"; say "  expected $want"; say "  actual   $got"; return 1; }
  say "checksum ok ($name)"
}

fetch() {
  # $1 = url, $2 = output path. Quiet on failure; the caller reports.
  if have curl; then curl -fSL "$1" -o "$2" 2>/dev/null
  elif have wget; then wget -q "$1" -O "$2"
  else return 1; fi
}

install_binary_from() {
  # $1 = path to a built/downloaded `helix` executable
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$1" "$INSTALL_DIR/$BIN" 2>/dev/null || { cp "$1" "$INSTALL_DIR/$BIN"; chmod 0755 "$INSTALL_DIR/$BIN"; }
  say "installed $BIN -> $INSTALL_DIR/$BIN"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) : ;;
    *)
      say "note: $INSTALL_DIR is not on your PATH. Add it with:"
      say "      echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.profile && . ~/.profile"
      ;;
  esac
  "$INSTALL_DIR/$BIN" version || true
}

from_release() {
  target="$(detect_target)"
  # Opt into the fully-static musl artifact on x86_64 Linux (air-gapped / maximum
  # portability). The default x86_64 Linux build is the glibc PGO binary.
  if [ "${HELIX_MUSL:-0}" = "1" ] && [ "$target" = "x86_64-unknown-linux-gnu" ]; then
    target="x86_64-unknown-linux-musl"
  fi
  ver="${HELIX_VERSION:-latest}"
  if [ "$ver" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
  else
    base="https://github.com/$REPO/releases/download/$ver"
  fi
  asset="helix-$target.tar.gz"
  tmp="$(mktemp -d)"
  # Clean up the temp dir on any exit path, including the error ones below.
  trap 'rm -rf "$tmp"' EXIT INT TERM

  say "downloading $base/$asset"
  fetch "$base/$asset" "$tmp/$asset" || return 1

  if [ "${HELIX_NO_VERIFY:-0}" != "1" ]; then
    if fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
      verify_sha256 "$tmp/$asset" "$asset" "$tmp/SHA256SUMS" \
        || die "refusing to install an unverified download. Re-run to retry, or set HELIX_NO_VERIFY=1 to override."
    else
      # A release published before checksums existed. Warn loudly rather than
      # failing shut, but never pretend it was verified.
      say "warning: no SHA256SUMS published for this release — INSTALLING UNVERIFIED"
    fi
  fi

  tar -xzf "$tmp/$asset" -C "$tmp" || return 1
  install_binary_from "$tmp/$BIN"
}

from_source() {
  have cargo || die "no prebuilt binary for your platform, and cargo isn't installed — install Rust from https://rustup.rs first."
  [ -f Cargo.toml ] || die "run this from a Helix checkout (no Cargo.toml here), or wait for a published release."
  say "building from source (cargo build --release) — this takes a few minutes…"
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

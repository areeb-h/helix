#!/usr/bin/env bash
# Run a release's own notes against the PUBLISHED artifact.
#
# WHY THIS EXISTS. Every other gate in this repo compares the tree against itself:
# the corpus pins what the tree computes, `dfdiff.sh` compares two backends,
# `vmparity.sh` compares three engines, `tests/compat/` compares against what an
# earlier release computed. None of them can notice that the CHANGELOG promises
# something the binary does not do — and v0.6.0 promised exactly that. ADR 0036
# policy 1 said "String `+` is refused in a query, as it is on scalars"; the polars
# backend concatenated, with exit 0, and every gate was green over it, because no
# tracked program adds to a String column so no differential run ever evaluated the
# expression. The tag was already pushed when this script found it.
#
# The expectations in `tests/release/v<version>-claims.expected` are authored BY
# HAND from the release notes. Capturing them from a binary would make this
# tautological — it would prove the binary agrees with itself, which is the one
# thing already covered.
#
#   Usage: scripts/release-smoke.sh <version>          # installs the public artifact
#          BIN=./target/gate/helix scripts/release-smoke.sh <version>   # a local binary
#
# With BIN set, steps 1, 2 and 6 (install, version, glibc floor) are skipped: they
# are properties of the published artifact, not of the claims.
set -uo pipefail
cd "$(dirname "$0")/.."

VER="${1:?usage: release-smoke.sh <version>   (e.g. 0.6.0)}"
DIR="tests/release"
CLAIMS="$DIR/v$VER-claims.helix"
EXPECT="$DIR/v$VER-claims.expected"
ERRORS="$DIR/v$VER-errors.tsv"
[ -f "$CLAIMS" ] || { echo "no claims program at $CLAIMS — write the release notes as a program first" >&2; exit 2; }
[ -f "$EXPECT" ] || { echo "no expectations at $EXPECT" >&2; exit 2; }

SCRATCH="${TMPDIR:-/tmp}/helix-smoke-$VER"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/bin"
FAIL=0
ok()  { printf 'ok   %s\n' "$*"; }
bad() { printf 'FAIL %s\n' "$*"; FAIL=1; }

LOCAL="${BIN:-}"
if [ -n "$LOCAL" ]; then
  H="$LOCAL"
  echo "== using $H (skipping install / version / glibc-floor checks)"
else
  echo "== 1. install v$VER through the PUBLIC installer (it verifies the checksum)"
  HELIX_INSTALL_DIR="$SCRATCH/bin" \
    curl -LsSf https://raw.githubusercontent.com/areeb-h/helix/main/install.sh | sh
  H="$SCRATCH/bin/helix"
  [ -x "$H" ] || { bad "installer produced no binary at $H"; exit 1; }

  echo
  echo "== 2. it is the version that was tagged"
  V=$("$H" --version 2>&1)
  if [ "$V" = "helix $VER" ]; then ok "$V"; else bad "version: got '$V', want 'helix $VER'"; fi
fi

echo
echo "== 3. the value claims (hand-authored from the release notes)"
if ! "$H" run "$CLAIMS" > "$SCRATCH/got" 2>&1; then
  bad "the claims program did not run"
  sed -n '1,15p' "$SCRATCH/got" | sed 's/^/       /'
elif diff -u "$EXPECT" "$SCRATCH/got"; then
  ok "$(grep -c . "$EXPECT") value claims"
else
  bad "the binary does not compute what the release notes say"
fi

echo
echo "== 4. the claims that are ERRORS (non-zero exit AND the diagnostic)"
if [ -f "$ERRORS" ]; then
  while IFS=$'\t' read -r want src; do
    case "${want:-}" in ''|'#'*) continue ;; esac
    printf '%s\n' "$src" > "$SCRATCH/e.helix"
    out=$("$H" run "$SCRATCH/e.helix" 2>&1); rc=$?
    if [ "$rc" -eq 0 ]; then
      bad "must refuse but exited 0: $src"
    elif printf '%s' "$out" | grep -qF "$want"; then
      ok "refused, and says '$want'"
    else
      bad "exit $rc but the diagnostic lacks '$want': $src"
      printf '%s\n' "$out" | sed -n '1,4p' | sed 's/^/       /'
    fi
  done < "$ERRORS"
else
  echo "  (no $ERRORS — skipping)"
fi

echo
echo "== 5. HELIX_DF_ENGINE is validated, not silently ignored"
for eng in native nonsense; do
  out=$(HELIX_DF_ENGINE="$eng" "$H" eval 'print(1)' 2>&1); rc=$?
  if [ "$rc" -eq 0 ]; then
    # A dual-engine build legitimately HAS `native`; only `nonsense` must always fail.
    if [ "$eng" = native ]; then ok "HELIX_DF_ENGINE=native accepted (dual-engine build)"
    else bad "HELIX_DF_ENGINE=$eng was silently ignored"; fi
  elif printf '%s' "$out" | grep -qF "$eng"; then
    ok "HELIX_DF_ENGINE=$eng refused BY NAME"
  else
    bad "HELIX_DF_ENGINE=$eng refused without naming it: $out"
  fi
done

if [ -z "$LOCAL" ]; then
  echo
  echo "== 6. glibc floor: the artifact must not need more than install.sh routes to it"
  FLOOR=$(grep -m1 '^GLIBC_FLOOR=' install.sh | cut -d'"' -f2)
  HI=$(objdump -T "$H" 2>/dev/null | grep -o 'GLIBC_[0-9]\+\.[0-9]\+' | sort -t_ -k2 -V | tail -1)
  if [ -z "$HI" ]; then
    ok "no GLIBC_ symbols — a static musl artifact was installed here, floor n/a"
  elif [ "$(printf '%s\n%s\n' "${HI#GLIBC_}" "$FLOOR" | sort -V | tail -1)" = "$FLOOR" ]; then
    ok "highest requirement ${HI#GLIBC_} <= install.sh floor $FLOOR"
  else
    bad "artifact needs glibc ${HI#GLIBC_} but install.sh's floor is $FLOOR"
  fi
fi

echo
if [ "$FAIL" -eq 0 ]; then
  echo "SMOKE OK — v$VER does what its release notes say."
else
  echo "SMOKE FAILED — the notes and the binary disagree. Fix the binary, not the notes."
fi
exit "$FAIL"

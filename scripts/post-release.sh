#!/usr/bin/env bash
# Run AFTER the tag is pushed. Re-arms the tree so it stops claiming to BE the release it
# just shipped.
#
# WHY THIS EXISTS. `scripts/release.sh` bumps Cargo.toml at release time and nothing ever
# moved it again, so between releases a build from `main` reported the version that had
# just SHIPPED. A field report found the consequence precisely: after v0.6.0 both the
# released binary and a main build reported `helix 0.6.0`, so a project needing `now()` —
# which landed eight commits after the tag — could not say so. `helix = ">=0.6.0"` is
# satisfied by the very binary that lacks it, and the user found out at run time with
# "`now` is not a known function" instead of at the manifest check whose entire purpose is
# to replace that with one clear sentence.
#
# The marker is `X.Y.(Z+1)-dev`, which `pkg::parse_semver` orders BELOW the release it
# names: 0.6.0 < 0.6.1-dev < 0.6.1. So a manifest can say ">=0.6.1-dev" and mean "newer
# than the 0.6.0 release", and `release.sh` turns it into 0.6.1 by stripping the marker or
# 0.7.0 by the usual minor bump.
#
# HONEST LIMIT: this is a monotone counter, not a feature probe. Every commit in the
# window reports the same `0.6.1-dev`, so the floor still cannot say "needs `now`" — it
# says "newer than the last release", which converts a false yes for the released-binary
# population (almost everyone) into a correct no. For an ADDITION, `helix describe <name>`
# is the precise instrument and exits non-zero for an unknown name. For a SEMANTICS change
# nothing is probeable — tests/compat/MIGRATIONS.md is this project's own example, where
# v0.5.1 -> v0.6.0 changed what an expression COMPUTES and no name appeared or vanished.
# That is the case only an ordering claim covers, and it is why the marker earns its keep.
#
# WHICH MARKER. The level says what the NEXT release is expected to be, because the
# marker names it: after tagging 0.6.0, `patch` arms 0.6.1-dev and `minor` arms 0.7.0-dev.
# Pick `minor` when the changelog already carries a `### Changed` entry, since the policy
# in docs/RELEASING.md makes that a minor by definition. `patch` is the default because it
# is the conservative claim — every later release outranks it either way.
#
#   Usage: scripts/post-release.sh <the version just tagged> [patch|minor]
set -euo pipefail
cd "$(dirname "$0")/.."

VER="${1:?usage: post-release.sh <the version just tagged>}"

CUR=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
if [ "$CUR" != "$VER" ]; then
  echo "error: Cargo.toml reads \"$CUR\", not the tagged \"$VER\"." >&2
  echo "       Run this on the tagged commit, after pushing the tag." >&2
  exit 1
fi

case "$VER" in
  *-dev)
    echo "error: \"$VER\" already carries the marker — a release is never tagged with one." >&2
    exit 1 ;;
esac

LEVEL="${2:-patch}"
IFS=. read -r MA MI PA <<<"$VER"
case "$LEVEL" in
  patch) NEXT="$MA.$MI.$((PA + 1))-dev" ;;
  minor) NEXT="$MA.$((MI + 1)).0-dev" ;;
  *)     echo "usage: post-release.sh <version> [patch|minor]" >&2; exit 2 ;;
esac

sed -i "s/^version = \"$CUR\"/version = \"$NEXT\"/" Cargo.toml

# `release.sh` REQUIRES a `## Unreleased` heading and nothing ever wrote one back after it
# consumed it, so every cycle began by hand-adding it. Re-open it here.
grep -q '^## Unreleased' CHANGELOG.md || sed -i "0,/^## v/s//## Unreleased\n\n## v/" CHANGELOG.md

echo "== tree re-armed: $CUR -> $NEXT"
echo "   Cargo.toml bumped and CHANGELOG reopened — commit and push both."
echo "   From here `helix --version` reports $NEXT, which is TRUE: this is not $VER."

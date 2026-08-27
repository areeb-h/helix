#!/usr/bin/env bash
# The release ritual, mechanized UP TO the irreversible steps — which it prints
# and never runs. What it does: bump the version, roll CHANGELOG's Unreleased
# section into the new release heading, sanity-check the release workflow is
# parseable AND listed by name (a literal control byte once made GitHub
# silently degrade it — a tag push would have built nothing), and re-check the
# crate. Committing, tagging, and pushing stay human actions.
#
#   Usage: scripts/release.sh minor|patch
#
# The versioning policy lives in docs/RELEASING.md. The enforcement here: an
# Unreleased section containing a "### Changed" entry (frozen-format or other
# output changes) refuses a PATCH — that is a MINOR by policy.
set -euo pipefail
cd "$(dirname "$0")/.."

LEVEL="${1:-}"
if [ "$LEVEL" != minor ] && [ "$LEVEL" != patch ]; then
  echo "usage: scripts/release.sh minor|patch"
  exit 2
fi

CUR=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
# BETWEEN RELEASES THE TREE CARRIES THE NEXT PATCH WITH A `-dev` MARKER (scripts/
# post-release.sh), so the marker ALREADY NAMES the version a patch release becomes:
# stripping it is the bump, and incrementing as well would skip a version. Without this,
# `IFS=. read` put "1-dev" in PA and `$((PA + 1))` died with `bash: dev: unbound
# variable` — while `minor` silently discarded the marker, being right by accident.
BASE=${CUR%-dev}
IFS=. read -r MA MI PA <<<"$BASE"
ARMED=$([ "$CUR" != "$BASE" ] && echo 1 || echo 0)
if [ "$LEVEL" = minor ]; then
  # A tree armed for a MINOR (a marker whose patch is 0) already NAMES that release, so
  # stripping is the bump. Computing MI+1 from it would skip the version the marker
  # names: 0.7.0-dev would have released 0.8.0.
  if [ "$ARMED" = 1 ] && [ "$PA" = 0 ]; then NEW="$BASE"; else NEW="$MA.$((MI + 1)).0"; fi
elif [ "$ARMED" = 1 ]; then
  if [ "$PA" = 0 ]; then
    echo "error: the tree is armed for the MINOR $BASE, so a patch release from here is"
    echo "       not defined — the last release was on an earlier minor line."
    echo "       Release $BASE with 'minor', or re-arm: scripts/post-release.sh <last> patch"
    exit 1
  fi
  NEW="$BASE"
else
  NEW="$MA.$MI.$((PA + 1))"
fi
echo "== version: $CUR -> $NEW ($LEVEL)"

grep -q '^## Unreleased' CHANGELOG.md || {
  echo "error: CHANGELOG.md has no '## Unreleased' section — write the notes first."
  exit 1
}
UNREL=$(sed -n '/^## Unreleased/,/^## v/p' CHANGELOG.md)
if [ "$LEVEL" = patch ] && echo "$UNREL" | grep -q '^### Changed'; then
  echo "error: Unreleased carries a '### Changed' entry (an output/format change)."
  echo "       That is a MINOR release by policy (docs/RELEASING.md) — rerun with 'minor'."
  exit 1
fi

sed -i "s/^version = \"$CUR\"/version = \"$NEW\"/" Cargo.toml
sed -i "s/^## Unreleased/## v$NEW — $(date +%F)/" CHANGELOG.md
echo "== Cargo.toml and CHANGELOG.md staged for v$NEW"

python3 - <<'PY'
import yaml
yaml.safe_load(open(".github/workflows/release.yml"))
print("== release.yml parses as YAML")
PY
if command -v gh >/dev/null 2>&1; then
  gh workflow list 2>/dev/null | grep -qi release \
    && echo "== release workflow listed by NAME" \
    || echo "WARNING: release workflow not listed by name — check it before tagging"
fi

cargo check -q
echo "== cargo check ok"
echo
echo "== staged, NOT committed. The irreversible half, in order (docs/RELEASING.md):"
echo "   1. CARGO_BUILD_JOBS=2 bash scripts/gate.sh          # must be RC=0"
echo "   2. commit the bump, push, then: gh workflow run release.yml -f dry_run=true"
echo "   3. all six platforms green -> tag THE VALIDATED SHA: git tag v$NEW <sha> && git push origin v$NEW"
echo "      (the tag push IS the publish)"
echo "   4. public-installer smoke on the artifact; objdump glibc floor <= install.sh's GLIBC_FLOOR"

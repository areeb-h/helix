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

# A LANGUAGE-SURFACE ADDITION IS ALSO A MINOR, and reading the CHANGELOG cannot see it.
#
# The check above asks the PROSE whether anything changed. In the v0.8.0 cycle it did fire,
# but only by luck: `html_escape` happened to alter `to_html`'s bytes, which supplied the one
# `### Changed` heading. Without that entry a release adding a `[workspace]` table, seventeen
# builtins and a new `Value` type would have gone out as a patch, because every other line
# sat under `### Added`.
#
# The surface is in the source, so ask the source. `registry.rs` holds `BUILTINS` and every
# `*_METHODS` table — the names a program can actually write — so comparing the set of names
# against the last tag answers "did the language grow" mechanically, with no dependence on
# how the notes were worded.
#
# A pure RENAME keeps the count level. That is a breaking change rather than an addition, and
# it cannot avoid a `### Changed` entry, so the check above is the one that catches it.
surface() { awk '/pub static (BUILTINS|[A-Z_]*METHODS)/,/^\];/' | grep -o '"[a-z_0-9]*"' | sort -u | wc -l; }
LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || true)
if [ "$LEVEL" = patch ] && [ -n "$LAST_TAG" ]; then
  OLD_SURFACE=$(git show "$LAST_TAG:src/registry.rs" 2>/dev/null | surface || echo 0)
  NEW_SURFACE=$(surface < src/registry.rs)
  if [ "$OLD_SURFACE" -gt 0 ] && [ "$NEW_SURFACE" -gt "$OLD_SURFACE" ]; then
    echo "error: the language surface grew since $LAST_TAG ($OLD_SURFACE -> $NEW_SURFACE names)."
    echo "       A builtin or method that did not exist in the last release is an ADDITION,"
    echo "       which is a MINOR by policy (docs/RELEASING.md) — rerun with 'minor'."
    echo "       Added:"
    diff <(git show "$LAST_TAG:src/registry.rs" | awk '/pub static (BUILTINS|[A-Z_]*METHODS)/,/^\];/' | grep -o '"[a-z_0-9]*"' | sort -u) \
         <(awk '/pub static (BUILTINS|[A-Z_]*METHODS)/,/^\];/' src/registry.rs | grep -o '"[a-z_0-9]*"' | sort -u) \
      | grep '^>' | sed 's/^> /         /' | head -20
    exit 1
  fi
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

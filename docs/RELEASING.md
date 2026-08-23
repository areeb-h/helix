# Releasing Helix

The versioning policy and the release ritual, written down where a release is
cut rather than remembered across sessions. `scripts/release.sh minor|patch`
mechanizes the reversible half and refuses the policy violations it can see.

## Versioning policy

Pre-1.0 semver, with the line drawn where USERS feel it:

- **Patch** (0.X.Y+1): bug fixes, error-message improvements, performance,
  additive tooling (`--json`, lints), documentation. Nothing a working program
  or a byte-compared output notices.
- **Minor** (0.X+1.0): language-surface additions (`where`, new verbs), ANY
  change to printed output — the frozen frame format, record field order,
  float rendering — and anything that removes or narrows behavior. The frozen
  formats exist so output can be diffed; changing them is a versioned event
  with a `### Changed` entry, never a drive-by.
- The changelog is the contract: everything lands under `## Unreleased` in the
  release-notes voice as it merges, and the release commit only renames that
  heading. `release.sh` refuses a patch whose Unreleased carries `### Changed`.

## The ritual

1. `scripts/release.sh minor|patch` — bumps `Cargo.toml`, rolls the changelog
   heading, verifies `release.yml` parses AND is listed by name (a literal
   control byte once silently degraded the workflow; a tag push would have
   built nothing), and re-checks the crate. Staged, not committed.
2. `CARGO_BUILD_JOBS=2 bash scripts/gate.sh` — must end `GATE_RC=0`.
3. Commit the bump, push, and run the dry run:
   `gh workflow run release.yml -f dry_run=true`. All six platforms must be
   green **on the SHA you are about to tag**.
4. Tag **the validated SHA** — not whatever HEAD has become:
   `git tag vX.Y.Z <sha> && git push origin vX.Y.Z`. The tag push is the
   publish (draft-until-complete; six assets + SHA256SUMS).
5. Smoke the PUBLIC artifact: install via the public installer (checksum must
   verify), run the release-note claims against the installed binary.
6. Floor check: `objdump -T` on the gnu artifact — its highest `GLIBC_x.y`
   requirement must be ≤ `GLIBC_FLOOR` in `install.sh` (the release workflow
   also asserts this inside build-pgo). If the runner image changed, the floor
   moves IN THE RELEASE COMMIT, never after.

## Invariants worth re-reading before any release

- The gnu builds pin `ubuntu-22.04` (glibc 2.35). `install.sh` auto-routes
  musl distros and older glibc to the static musl artifact.
- `releases/latest` must resolve to the new tag once assets are complete.
- A release that changes frozen output carries the migration sentence in its
  notes (what changed, what a program that depended on it should do).

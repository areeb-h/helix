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

### Between releases the tree says `-dev`, and that is the point

A tagged commit carries a clean `X.Y.Z`. Every commit after it carries a marker
naming the release it is working toward, written by `scripts/post-release.sh`
as ritual step 7: `post-release.sh 0.6.0 patch` arms `0.6.1-dev`, and
`post-release.sh 0.6.0 minor` arms `0.7.0-dev`. Pick `minor` once the changelog
carries a `### Changed` entry, since the policy above makes that a minor by
definition. `patch` is the default because it is the conservative claim — every
later release outranks it either way.

`release.sh` reads the marker as the version it names: from `0.7.0-dev`,
`minor` releases **0.7.0** rather than stepping to 0.8.0, and `patch` is refused
outright because a patch release from a tree armed for the next minor line is
not a defined thing.

This exists because the tree used to keep the version it had just SHIPPED, so a
build from `main` and the released binary reported the same string. A field
report found the consequence exactly: `now()` landed eight commits after the
v0.6.0 tag, both binaries said `helix 0.6.0`, and a project needing `now()` had
no way to say so — `helix = ">=0.6.0"` is satisfied by the very binary that
lacks it, so the user met `` `now` is not a known function `` at run time
instead of one clear sentence at the manifest check whose whole purpose is to
replace that.

`pkg::parse_semver` orders the marker BELOW the release it names —
`0.6.0 < 0.6.1-dev < 0.6.1` — so a manifest can say `">=0.6.1-dev"` and mean
"newer than the 0.6.0 release". `-dev` is the ONLY pre-release spelling
accepted: `-rc1` and `-alpha.2` order against each other by convention alone,
and a version that cannot be compared is not a version.

**What it does not buy.** It is a monotone counter, not a feature probe. Every
commit in a release window reports the same string, so the floor says "newer
than the last release", never "has `now`". For an ADDITION the precise
instrument already exists — `helix describe now` exits non-zero for a name this
build does not have. The marker earns its keep on the case nothing can probe: a
SEMANTICS change, where `tests/compat/MIGRATIONS.md` records v0.5.1 → v0.6.0
altering what an expression computes while no name appeared or vanished.

**The transitional wart.** A manifest cannot usefully declare a `-dev` floor
until the release that taught the toolchain to parse one is adopted. An older
binary meeting `">=0.6.1-dev"` says *"must be a minimum version"* — a syntax
complaint — rather than *"your binary is too old"*. Unavoidable, and the reason
the parser ships one release ahead of the first marker.

## The ritual

1. `scripts/release.sh minor|patch` — bumps `Cargo.toml`, rolls the changelog
   heading, verifies `release.yml` parses AND is listed by name (a literal
   control byte once silently degraded the workflow; a tag push would have
   built nothing), and re-checks the crate. Staged, not committed.
2. `CARGO_BUILD_JOBS=2 bash scripts/gate.sh` — must end `GATE_RC=0`. Also run
   `scripts/dfdiff.sh` (needs a dual-engine binary) — 0 undeclared divergences.
3. **Write the release notes as a program, and run them BEFORE the tag.**
   `tests/release/vX.Y.Z-claims.helix` prints one line per cell of the
   changelog's tables; `…-claims.expected` holds what the notes SAY, authored
   by hand; `…-errors.tsv` holds the cells whose answer is a refusal. Then:
   `BIN=./target/gate/helix bash scripts/release-smoke.sh X.Y.Z`.

   This step exists because it is the only one that can fail. Every other gate
   compares the tree against itself — the corpus against its own `.expected`,
   `dfdiff` between two backends, `vmparity` between three engines, `compat`
   against an earlier release. None of them can notice that the changelog
   promises something the binary does not do, and in v0.6.0 one did: ADR 0036
   said String `+` was refused in a query, the polars backend concatenated with
   exit 0, and every gate was green. The tag was already pushed when this
   check found it (ADR 0036's addendum). Capturing the expectations from the
   binary instead of writing them by hand would have found nothing.
4. Commit the bump, push, and run the dry run:
   `gh workflow run release.yml -f dry_run=true`. All six platforms must be
   green **on the SHA you are about to tag**.
5. Tag **the validated SHA** — not whatever HEAD has become:
   `git tag vX.Y.Z <sha> && git push origin vX.Y.Z`. The tag push is the
   publish (draft-until-complete; six assets + SHA256SUMS). The release is a
   DRAFT until the last asset lands, so a tag can still be withdrawn up to
   that point — `gh run cancel`, delete the draft, delete the tag.
6. Smoke the PUBLIC artifact: `bash scripts/release-smoke.sh X.Y.Z` with no
   `BIN`. It installs through the public installer (which verifies the
   checksum), checks `--version`, re-runs the claims against the *published*
   binary, and does the floor check: `objdump -T` on the gnu artifact — its
   highest `GLIBC_x.y` requirement must be ≤ `GLIBC_FLOOR` in `install.sh`
   (the release workflow also asserts this inside build-pgo). If the runner
   image changed, the floor moves IN THE RELEASE COMMIT, never after.
7. **Re-arm the tree**: `bash scripts/post-release.sh X.Y.Z [patch|minor]`,
   then commit and push. `Cargo.toml` takes the marker and `## Unreleased` is reopened
   (`release.sh` requires that heading and nothing used to write it back, so
   every cycle began by adding it by hand). From here `helix --version` reports
   the marker, which is TRUE: the tree is not the release any more.

   Forgetting the reverse — tagging a tree that still carries a marker — is
   caught by `create-release` in `release.yml`, which asserts the tag matches
   `Cargo.toml` and carries no `-dev` **before any asset is published**. That
   guard is the only pre-publish check of the version string; step 6's
   `release-smoke.sh` is the only other one and it runs after.

## Invariants worth re-reading before any release

- The gnu builds pin `ubuntu-22.04` (glibc 2.35). `install.sh` auto-routes
  musl distros and older glibc to the static musl artifact.
- `releases/latest` must resolve to the new tag once assets are complete.
- A release that changes frozen output carries the migration sentence in its
  notes (what changed, what a program that depended on it should do).

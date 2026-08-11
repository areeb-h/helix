# Changelog

## v0.1.1 — 2026-08-11

**The first installable release.** `v0.1.0` is published but nobody can install from it, and
this release exists to replace it rather than to add anything.

### What was wrong with v0.1.0

Probed against the live release: `helix-x86_64-unknown-linux-gnu.tar.gz` → **404**,
`SHA256SUMS` → **404**. Four of six platforms uploaded, and the installers refuse to install
what they cannot verify, so even those four were unreachable. Three independent causes:

- The profile-guided Linux build died in its training step on
  `bench/crosslang/b3_groupby.helix`, which still called `io.read_csv(…)` — a spelling
  ADR-0017 removed. **Nine** benchmark programs had rotted the same way; nothing in CI ever
  compiled them, because every gate the project had *runs* its programs and these need a
  250 MB generated fixture first.
- Six build jobs each called `action-gh-release`, which creates-if-missing, so six of them
  raced to create one release. The musl job died mid-upload.
- The checksum job was skipped for a failed dependency, and the incomplete release went
  public anyway.

### Fixed

- **The release is built as a draft and published last.** One job creates it, every build
  uploads into it, and publishing is gated on all six platforms plus `SHA256SUMS` being
  present. A failure now leaves an invisible, re-runnable draft instead of a broken public
  artifact.
- **Every build smoke-tests the binary it produced** where the runner can execute it, and
  the musl job asserts the artifact is genuinely static — the one machine that would
  otherwise discover it is the air-gapped one with no way to fix it.
- **`workflow_dispatch` takes a `dry_run` input** (default on) that builds and smoke-tests
  all six platforms while writing nothing, so the pipeline can be checked without publishing
  something to check it with.
- The nine stale benchmark programs are repaired and verified by running them.

### New

- **`helix check <script>…`** — type-check without running or writing anything. Takes many
  paths in one process; `scripts/checkall.sh` covers all 85 tracked programs in ~0.03 s and
  runs in CI, which is what closes the gap that let the benchmarks rot. It never executes
  the program, and it is honestly *only* a type check: code that checks clean can still fail
  at run time.

### Toolchain and hygiene

- All five CI jobs now block. Clippy is at zero warnings with `-D warnings` (no `#[allow]`
  suppressions); MSRV 1.96 is verified rather than asserted; `cargo audit` is green, with two
  advisories fixed by upgrade (crossbeam-epoch, quinn-proto) and four that have no reachable
  fix documented in `.cargo/audit.toml`, each with the crate that blocks it.
- `CONTRIBUTING.md` added.
- The Docker build context went from 1.3 GB to 9 MB — `.dockerignore` was not excluding
  `website/node_modules`, `website/.next` or the generated benchmark fixtures.

**No language changes.** A `v0.1.0` program runs identically on `v0.1.1`.

## v0.1.0 — 2026-08-11

First tagged release. Incomplete — see above; use `v0.1.1`.

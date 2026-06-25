# ADR 0013 — Package manager (manifest, lockfile, resolution)

- **Status:** In progress (shipped: manifest + lockfile + path & https dependencies)
- **Date:** 2026-06-26
- **Deciders:** Areeb + Claude
- **Builds on:** [ADR 0009](0009-distribution-and-install.md) (decision #6: tamper-evident
  lockfile, mandatory hashes, no install-time resolver) and
  [ADR 0010](0010-networking-privacy-security.md) (the hash is the trust boundary).

## Context

A language without a way to share and install libraries is a binary, not an
ecosystem — the single largest remaining adoption gap. ADR 0009 decided the
*lockfile philosophy*; this ADR designs the *package-manager mechanics* and the
properties that make it, for the scientific audience, better than the incumbents.

The bar is explicit: **do it better than every other language.** That is a design
goal, achieved by learning from each incumbent's documented failure rather than
copying any one of them ([design philosophy](../../README.md)).

## Decision — the model, and how it beats each incumbent

| Failure mode | Who suffers it | Helix's answer |
|---|---|---|
| Mutable registry — a published version can change or vanish | npm (left-pad), PyPI | **Immutable, content-addressed.** The lockfile pins each dependency by a sha256 of its source tree; a changed source is a *different* package. |
| Non-reproducible installs — resolve at install time | pip, npm semver ranges | **No install-time resolver.** Resolution runs at `add`; `sync` only fetches and *verifies* against the lockfile → bit-identical forever. |
| Arbitrary code on install | npm `postinstall` (the supply-chain hole) | **Pure-source packages, zero install scripts.** Nothing executes on add/sync. |
| TLS treated as the trust boundary | most | **The hash is the trust boundary** (ADR 0010), not the transport — works through mirrors, proxies, fully offline. |
| Split toolchain — language + package manager + env are separate tools | Python (pip + venv + pyenv) | **One binary, and (target) one `helix.lock`** pinning Helix deps *and* the managed Python interpreter + wheels (ADR 0009 #6). |

**The killer property for a science language: reproducibility as an enforced,
cryptographic fact** — "run this 2019 study's code in 2030 and get a bit-identical
dependency tree." The hash-pinned lockfile makes that the default, not a discipline.

### Formats

- **`helix.toml`** — the hand-edited manifest (TOML: the Cargo/pyproject convention,
  comments, low punctuation). `[package]` (name, version) + `[dependencies]`.
- **`helix.lock`** — machine-generated, `@generated` header, committed to VCS. One
  `[[package]]` per resolved dependency: `name`, `source`, `sha256` (of the source
  tree). Deterministic; a second `sync` reports "up to date".

### Resolution

Walk the manifest's dependencies transitively, dedup by canonical path, detect cycles,
hash each source tree, emit the lockfile + a `name → directory` map. The module loader
resolves `import dep.module` within `dep`'s directory (the first segment selects the
package). A single flat dependency namespace in v1.

### CLI (one binary is the whole toolchain — ADR 0009)

`helix new <name>` (init a manifest) · `helix sync` (resolve + write the lockfile) ·
`helix run` (resolves and loads dependencies). `helix add` and `helix verify` are next.

## Status

- **v1 shipped:** manifest + lockfile types (serde + TOML), sha256 tree hashing,
  transitive **path-dependency** resolution with cycle/collision detection, module-loader
  integration (`import dep.module`), and `helix new` / `helix sync`. `sha2` is now a
  core dependency (the integrity hash is part of the toolchain). Fully offline and
  locally verifiable — the reproducibility property holds today for path deps.
- **Remote `https` sources shipped:** `dep = { url = "…tar.gz", sha256 = "…" }`. The
  download is rejected unless its hash matches the pinned `sha256` (the trust boundary,
  via the ADR-0010 verified-download layer), then unpacked into a **content-addressed
  cache** (`$XDG_CACHE_HOME/helix/cache/<sha256>/`, override `HELIX_CACHE`) keyed by that
  hash — a present entry was provably verified, so fetch is skipped forever after.
  `tar`'s extraction refuses path-escape entries. The single-top-level-dir tarball layout
  (GitHub/npm) is unwrapped automatically. The networking primitive (`src/net.rs`) is now
  gated on `http` (default-on), not `managed`; an air-gapped build (`--no-default-features`)
  rejects `url` deps with a clean, actionable error and stays path-only.
- **Next:** `git` sources (rev-pinned); `helix run` already *verifies* the lockfile
  (error if a dependency's source drifted since `sync`); `helix add`/`verify`; per-package
  dependency scoping; and the unified Helix + managed-Python lockfile (ADR 0009 #6).

## Security model (remote sources)

Fetching and unpacking remote tarballs is the package manager's real attack surface.
The threat model and the mitigation for each hole (all enforced in `src/pkg.rs`):

- **Substituted / corrupted bytes.** The pinned `sha256` is verified *before* anything
  is written (ADR 0010). TLS protects transport but a mirror or a TLS-intercepting proxy
  can still serve bad bytes — so the *hash*, not TLS, is the trust boundary.
- **Path injection through the hash.** The `sha256` string becomes a cache directory
  name, so an unchecked value like `../../etc/cron.d/x` would escape the cache.
  `normalize_sha256` requires exactly 64 hex characters before the value is used as a
  path or a trust value.
- **Decompression bomb** (a few-KB gzip that inflates to terabytes). The unpacker
  *streams* the decompressor — it never reads the fully expanded image into memory — and
  caps both total expanded bytes (512 MiB) and entry count (100 000). The per-entry size
  is checked *before* the body is read, so a bomb's payload is never materialized.
- **Tar escape.** Absolute paths and `..` that would escape the destination are refused
  (`unpack_in`'s path check), and symlink / hardlink / device / fifo entries are rejected
  outright — a package is plain files and directories and never needs them. Archive
  permissions/xattrs are not preserved (no setuid surprises).
- **Partial / raced cache.** Extraction lands in a private temp dir and is promoted with
  an atomic `rename`, so a crash or a concurrent `sync` can never leave a half-unpacked
  tree that the cache-hit check mistakes for a complete, verified package.
- **No code runs on install.** Packages are pure Helix source; nothing executes on
  add/sync (no npm-`postinstall`-style supply-chain hole).

Residual / accepted: a `url` fetch issues an HTTPS request from the developer's machine
at `sync` time (a theoretical SSRF probe vector), but the response is hash-checked so no
attacker-chosen *content* can enter the build. A pinned-but-malicious *source* is out of
scope for the fetcher — that is the reviewer's call, which is exactly why nothing runs on
install and the hash makes the bytes auditable and reproducible.

## Rejected alternatives

- **A central mutable registry (npm/PyPI model).** Rejected for its mutability and
  install-script footguns; integrity comes from the lockfile's hashes, not a registry.
  A registry can be added later purely as a *discovery* layer over hash-pinned content.
- **Install-time dependency resolution (pip/npm).** Non-reproducible by construction;
  resolution is a one-time `add`-time act, the lockfile the durable result.
- **JSON manifest.** Workable (serde_json is already a dependency) but a worse
  hand-editing experience than TOML for the human-facing manifest.
- **A `git`-submodule / vendoring-only scheme.** No integrity guarantee and no
  transitive resolution; the lockfile + hashing is strictly better.

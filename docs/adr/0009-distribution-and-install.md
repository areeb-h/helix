# ADR 0009 — Distribution & installation

- **Status:** 🛠 in progress (CLI + source install shipped; prebuilt releases wired, not yet hosted)
- **Date:** 2026-06-24
- **Deciders:** Areeb + Claude
- **Research:** [2026-06-24 distribution & toolchain](../research/2026-06-24-distribution-toolchain.md)

## Context

Until now the only way to run Helix was `cargo run` / `cargo build` — the build
workflow of Helix's *Rust implementation*. That makes Helix feel like a Rust project,
not a language: there is no `helix` on your PATH, no installer, nothing a scientist
who doesn't have (or want) Rust can use. This is the **distribution** half of the
viability bar ([docs/adoption.md](../adoption.md)) and, with interop, the highest-
leverage adoption gap left.

The question isn't just "ship a binary" — it's **can we make installing and using
Helix better than the incumbents?** The answer is yes, because the bar has moved and
Helix is well-positioned for where it moved to.

## What "good" looks like now

The modern gold standard is **not** npm/pip/php (which need a separate runtime
installed first) — it's **uv / bun / deno / rustup / go**:

- a **single self-contained binary** (no runtime to install, near-zero startup);
- a **one-line install** (`curl -LsSf … | sh`) plus presence in the usual package
  managers (Homebrew, Scoop/winget, AUR, `cargo-binstall`);
- that binary is **the whole toolchain** — run, REPL, package manager, formatter —
  one command (`cargo`/`go`/`deno`/`bun`/`uv` style), not a pile of separate tools.

Helix already has the hard part: the core is a single, statically-linkable Rust
binary with negligible startup overhead (see [benchmarks.md](../benchmarks.md)). So
matching uv/bun/deno is mostly packaging, not engineering.

## Decision

**Ship Helix as a single self-contained `helix` binary, installed by a one-line
script or a package manager, where the binary is also the whole toolchain.** Match
the uv/bun/deno experience, and beat everyone on the one axis unique to Helix — the
Python-interop story.

### How we go beyond the incumbents

1. **Self-contained *core*, opt-in heavy features.** The default `helix` links
   nothing external (no libpython, no system BLAS — Helix's linear algebra is pure
   Rust). So the one-line install yields a binary that *just runs*, on any box, with
   no "now install X" second step. That already beats Python/node/php and matches
   Go/Rust/uv.

2. **Managed Python for interop — the differentiator nobody nails.** Mojo's #1
   shipped failure is a runtime *"can't locate libpython."* We do what **uv does for
   Python toolchains**: when you want interop, `helix` downloads and manages a
   relocatable CPython (python-build-standalone) for you and pins it in the project's
   lockfile. So `helix` + Python "just works" and is reproducible — no system Python
   hunt, no "two dependency graphs." This is genuinely better than Mojo and better
   than how most languages expose an FFI. (Implementation: ADR 0008 roadmap item
   "bundled CPython".)

3. **One unified lockfile for *both* worlds.** Because interop pins a Python env, a
   single `helix.lock` pins Helix deps *and* the Python interpreter + packages —
   one reproducible source of truth, instead of `requirements.txt` + a venv + a
   separate language manager. (Implementation: the package-manager roadmap item.)

4. **One binary = the whole toolchain.** `helix run`, `helix repl`, `helix eval`,
   and later `helix add`, `helix fmt`, `helix test`, `helix python` — the cargo/go/
   deno model. No separate package manager or formatter to install.

### Install surface (target)

```
# one-liner (downloads the prebuilt self-contained binary for your platform)
curl -LsSf https://raw.githubusercontent.com/<owner>/helix/main/install.sh | sh

# package managers (later)
brew install helix          # macOS/Linux
scoop install helix         # Windows
cargo binstall helix        # from crates.io prebuilts

# from source (needs Rust)
cargo install --path .      # or: HELIX_FROM_SOURCE=1 ./install.sh
```

Then: `helix run script.helix`, `helix repl`, `helix eval "print(1 + 2)"`.

## Status / implementation

- **✅ Real CLI** (`src/main.rs`): `helix run <script>`, `helix eval "<code>"`,
  `helix repl`, `helix version`, `helix help` — plus the `helix <script.helix>`
  shorthand and the old `-V`/`-h` flags (backward compatible; tested).
- **✅ `cargo install --path .`** puts a release `helix` on the PATH today (the
  source-install path for anyone with Rust).
- **✅ `install.sh`** — the eventual `curl | sh` one-liner: detects OS/arch, tries to
  download a prebuilt release asset, falls back to a source build. Usable now in
  from-source mode.
- **✅ `.github/workflows/release.yml`** — cross-compiles the self-contained core for
  linux (x86_64/aarch64), macOS (x86_64/aarch64), and Windows (x86_64) and attaches
  the binaries to a GitHub Release on a `vX.Y.Z` tag. **Activates once the repo is on
  GitHub and a tag is pushed** — nothing else to wire.
- **⬜ Hosting** — the repo isn't on GitHub yet, so no releases exist to download; the
  one-liner therefore source-builds for now. Pushing to GitHub + tagging flips it to
  prebuilt downloads.

## Toolchain & version management (research-backed)

Grounded in the [2026-06-24 research](../research/2026-06-24-distribution-toolchain.md)
(25 claims, all 3-vote verified). The principle: **adopt the proven models, don't
invent** — Go's auto-toolchain ergonomics, uv's managed-runtime mechanics, Rust's
edition/channel stability model, Sigstore provenance.

1. **Release provenance — GitHub Artifact Attestations (Sigstore).** Sign every
   release binary: SLSA Build **Level 2 free** on public repos, **Level 3** via an
   isolated reusable workflow. The installer verifies before installing; a **minisign
   sidecar** covers `cargo-binstall`. (Adds `attestations: write` +
   `actions/attest-build-provenance` to `release.yml`.)

2. **Version management — Go's `GOTOOLCHAIN=auto` model.** A project-manifest version
   directive is a **hard rule**: `helix` auto-downloads and hands off to the *exact*
   required toolchain (itself a checksum-verified artifact), rather than building with
   the wrong version. Combined with **rustup-style channels** (stable/beta/nightly)
   and structured pinnable names. `helix` is one tool managing many toolchains — **no
   external version manager needed.**

3. **Stability — Rust editions + feature-gating.** Breaking changes land in **opt-in,
   per-project editions**; the non-negotiable rule is that cross-edition code must
   interoperate seamlessly (no ecosystem split). Unstable features are mechanically
   blocked outside nightly.

4. **Managed Python — implicit-by-default, uv-style (resolves the prior open
   question).** Download a relocatable **python-build-standalone** CPython on **first
   interop use** (opt-out to a manual mode); select per-project via a `.python-version`
   searched up the directory tree plus a `requires-python` constraint; and — the
   load-bearing fix — **rewrite the distribution's embedded absolute build-time paths
   at install time** (`_sysconfigdata_*.py`, the config `Makefile`, `PYTHON.json`),
   which is precisely what kills the "can't find libpython" failure class. An explicit
   **`helix python`** subcommand manages/pins/lists interpreters. (Caveat: install-time
   fixup targets the install location; full move-anywhere relocatability also needs
   dylib-install-name / `pyvenv.cfg` handling.) **This download is the first consumer
   of the [ADR 0010](0010-networking-privacy-security.md) networking layer — verify the
   interpreter's hash/signature before extracting, never trust TLS alone.**

## Install delivery, signing & operational defaults (research-backed)

From the [second research pass](../research/2026-06-24-distribution-toolchain.md)
(24/25 verified, 1 refuted):

5. **Install delivery — harden curl|sh, keep alternatives.** Keep the one-liner (it's
   what users expect from uv/rustup/deno) but **hardened**: TLS-only, `set -euf`,
   idempotent, fail-closed on partial download, **verify a checksum/signature before
   executing**. Prominently offer an **inspect-before-run** path and a **direct binary
   download**. Add a **minisign sidecar** (cargo-binstall) + Sigstore attestations.
   Broaden to **Homebrew / WinGet / Scoop / Docker**. *Reject* bare `curl | bash` with
   no verification or alternative. (`sget | bash` is refuted — sget is archived.)

6. **Lockfile & registry — tamper-evident, mandatory hashes, no install-time
   resolver.** Reject npm's mutable registry. Use **immutable checksums** (Go
   `go.sum`-style) and a **PEP 751 `pylock.toml`**-style design (mandatory per-package
   sha256, no resolver at install) → reproducible installs. **One unified lockfile**
   pins Helix deps *and* the managed Python interpreter + wheels. A Go-sumdb-style
   **transparency log is deferred** — a signed mandatory-hash lockfile likely suffices
   at this scale; revisit when an ecosystem forms.

7. **Signing — sign macOS, use cheap Windows signing, plan the reputation ramp.**
   macOS: Developer ID → **notarize → staple**. Windows: **EV no longer bypasses
   SmartScreen (2024)** — use **Azure Trusted Signing (~$10/mo, no HSM)**, not an EV
   cert; SmartScreen reputation accrues per-hash over weeks and even signed new
   binaries can trip **AV false-positives** (plan for it). Sigstore covers non-OS trust.

8. **Linux portability — glibc primary, musl static option (with a swapped
   allocator).** Ship a **glibc build against an old baseline** as primary (perf); a
   **musl static build** as the max-portability / air-gapped option — but **swap the
   allocator** (mimalloc/jemalloc), because musl's default allocator is a documented
   performance cliff. Cover x86_64 + arm64.

9. **Telemetry & operational defaults — privacy-first.** Telemetry is **opt-in, off by
   default** (Go reversed opt-out→opt-in after backlash; our audience is often
   air-gapped) with a fully **offline mode**. Follow **XDG base dirs** (+ macOS
   `~/Library`, Windows `%LOCALAPPDATA%`) for cached managed runtimes; ship a clean
   **uninstall** and cross-shell PATH setup; respect **proxy env vars + system cert
   stores** and offer an **offline bundle** (binary + pinned Python).

10. **Binary size — `strip`, keep speed.** Apply `strip = true` (free; symbols only).
    **Do NOT use `opt-level = "z"`** — it sacrifices the speed that is Helix's point.
    `panic = "abort"` + fat LTO + `codegen-units = 1` are already set.

## Rejected alternatives

- **Keep `cargo run` as the entry point.** It's a Rust build tool, requires the Rust
  toolchain, and makes Helix feel like a library, not a language. Fine for
  contributors, wrong for users.
- **Require a runtime (the pip/npm/php model).** A second mandatory install is
  exactly the friction the modern single-binary tools removed. Helix's core has no
  runtime, so there's no reason to inherit that friction.
- **Bundle Python into the *default* binary.** That would bloat every download and
  re-introduce the libpython coupling for the 90% who don't need interop. Python is
  an *opt-in*, *managed* component, not a baseline dependency.
- **Separate `helix`, `helix-pkg`, `helix-fmt` tools.** The ecosystem has clearly
  converged on one multi-command binary (cargo/go/deno/bun/uv). One tool, one install.

## Consequences

- The default release is the **core** (no Python); interop is a separate, managed
  capability — this keeps the baseline download small and dependency-free.
- We take on a release pipeline (the CI workflow) and, eventually, package-manager
  manifests (Homebrew formula, Scoop manifest) to maintain.
- "Managed Python" and "one lockfile" are now committed product promises that the
  bundled-CPython and package-manager work must deliver (ADR 0008 + roadmap Phase 7).

## Open questions (genuine residuals — the rest is now decided above)

Two research passes (49/50 claims verified) settled the model; these remain:

- **Transparency log at scale** — whether a Go-sumdb-style log is worth it, or a
  signed mandatory-hash lockfile suffices. Deferred until an ecosystem forms.
- **crates.io publication** (`cargo install` / `cargo binstall`) vs GitHub-Releases-
  only — a sequencing call, not a blocker.
- **Package-manager rollout order** — which of Homebrew / WinGet / Scoop / AUR / Nix
  to submit first, and when to seek crates.io / Homebrew-core inclusion.
- **macOS quarantine nuance** — the curl-download vs browser-download quarantine
  difference for the one-liner path (cosmetic, worth confirming during implementation).

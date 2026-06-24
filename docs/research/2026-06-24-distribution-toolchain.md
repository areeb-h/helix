# Research — distribution, install & toolchain strategy (2026-06-24)

Cited research grounding the toolchain/version-management decisions in
[ADR 0009 — Distribution & installation](../adr/0009-distribution-and-install.md).
Method: 5-angle fan-out (108 agents), 26 primary sources, 122 candidate claims →
**25 verified by 3-vote adversarial check, 0 refuted**, synthesized to 6 findings.

**The throughline:** for a single self-contained CLI, adopt **Go's auto-toolchain
ergonomics, uv's managed-runtime mechanics, Rust's edition/channel stability model,
and Sigstore-based provenance — from day one.**

## Verified findings (all 3-0)

### 1. Build provenance — GitHub Artifact Attestations (Sigstore)
Sign every release binary with **GitHub Artifact Attestations** (Sigstore-backed):
**SLSA Build Level 2 for free** on public repos; **Level 3** when the build is an
*isolated reusable workflow* (separating build steps from signing material).
Attestations are cryptographically signed, unfalsifiable provenance. Verify with a
single **cosign** policy (OIDC issuer + expected repo/workflow identity) — the same
pattern works across npm, Actions, and Homebrew. For `cargo-binstall`, add a light
**minisign sidecar** (`{url}.sig`, pubkey in `Cargo.toml [package.metadata.binstall.signing]`).
*Sources:* [GitHub attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations),
[cosign bundles](https://blog.sigstore.dev/cosign-verify-bundles/),
[cargo-binstall SIGNING](https://github.com/cargo-bins/cargo-binstall/blob/main/SIGNING.md).
*Time-note:* cosign v3 changed bundle defaults; version-check the exact command.

### 2. Version/compiler management — Go's `GOTOOLCHAIN=auto`
The strongest model: a **project-manifest version directive is a HARD rule** (not a
hint) that triggers **automatic download-and-handoff to the exact required
toolchain**. Go 1.21+ refuses to build with too-old a toolchain rather than
miscompiling; under `GOTOOLCHAIN=auto` it downloads the required version and
re-invokes it. **Toolchains are distributed as ordinary checksum-verified artifacts**
(proxyable, checked against a checksum DB). This is the Go analogue of rustup's
`rust-toolchain` and uv's `.python-version` auto-provisioning.
*Sources:* [Go toolchain](https://go.dev/doc/toolchain), [Go blog](https://go.dev/blog/toolchain).

### 3. Multi-version management — rustup's model
One tool managing multiple compiler installations ("toolchains") around a
**stable/beta/nightly release train**, with **structured pinnable names**
(`channel[-date][-host]`, e.g. `nightly-2014-12-18`, `1.42.0`) so a user can pin a
channel, a dated nightly, or an exact version.
*Sources:* [rustup toolchains](https://rust-lang.github.io/rustup/concepts/toolchains.html),
[RFC 0507 release channels](https://rust-lang.github.io/rfcs/0507-release-channels.html).

### 4. Backward-compatibility — Rust editions + feature-gating
Evolve the language without splitting the ecosystem via **editions**: opt-in,
chosen **per-project**, existing code keeps its behavior until the author migrates.
**The non-negotiable rule: code in one edition MUST seamlessly interoperate with code
in another** (so each unit migrates independently). Pair with **feature-gating** that
mechanically blocks unstable features outside nightly (Rust's `#[feature(...)]` is a
hard error on stable, E0554).
*Sources:* [Rust edition guide](https://doc.rust-lang.org/edition-guide/editions/),
[RFC 0507](https://rust-lang.github.io/rfcs/0507-release-channels.html).

### 5. Managed Python — uv's mechanics (answers "subcommand vs implicit")
Copy uv: obtain **pre-built relocatable CPython from python-build-standalone**
(never source-build, which is what makes pyenv slow). **Download automatically on
demand by default**, disable-able to "manual" mode. Select per-project by searching
for a **`.python-version` file up the directory tree** plus a `requires-python`
constraint. **→ The answer to "explicit `helix python` subcommand vs implicit": both —
implicit-by-default download on first use, with an explicit subcommand to manage/pin
and a manual opt-out.**
*Source:* [uv python versions](https://docs.astral.sh/uv/concepts/python-versions/).

### 6. Managed Python — the path-rewrite fix that kills "can't find libpython"
python-build-standalone distributions are highly portable but **embed absolute
build-time paths** (in `_sysconfigdata_*.py`, the config `Makefile`, and
`PYTHON.json` — e.g. `/build`, `/install`). A freshly extracted distribution is **not
cleanly relocatable** until those are patched. **uv rewrites the embedded absolute
paths at install time** — Helix must do the same, not ship the distributions raw.
*Caveat:* this fixup targets the install location; it does **not** make the install
fully move-anywhere relocatable (macOS dylib install names, `pyvenv.cfg`,
`bin/python` still embed paths — uv exposes a separate `--relocatable` flag).
*Sources:* [python-build-standalone docs](https://gregoryszorc.com/docs/python-build-standalone/main/),
[uv python versions](https://docs.astral.sh/uv/concepts/python-versions/).

## Honest gaps — NOT resolved by verified claims

The verification pass produced **no surviving claims** for four areas the brief
asked about. These must **not** be treated as decided; they need a follow-up pass
(relevant sources *were* fetched — listed below — but their claims didn't reach the
verified top-25):

1. **curl|sh install security matrix** — the criticisms and safer alternatives, and
   what Helix's minimal trustworthy install matrix should actually be.
   (fetched: [detecting curl|bash server-side](https://www.idontplaydarts.com/2016/04/detecting-curl-pipe-bash-server-side/))
2. **Lockfile architecture + registry model** (Domain 4, essentially unrepresented) —
   registry vs Go-style VCS+proxy+checksum-DB vs Deno/JSR URL imports; content-
   addressing; and a **unified lockfile pinning both Helix deps AND the managed Python
   env**. (fetched: [go checksum DB](https://words.filippo.io/gosum/),
   [PEP 751 lockfile](https://peps.python.org/pep-0751/),
   [lockfile format tradeoffs](https://nesbitt.io/2026/01/17/lockfile-format-design-and-tradeoffs.html))
3. **OS-specific trust** — macOS notarization/Gatekeeper, Windows Authenticode +
   SmartScreen reputation, antivirus false-positives on unsigned binaries, and the
   **musl-vs-glibc** static-linking decision. (fetched:
   [musl differences](https://wiki.musl-libc.org/functional-differences-from-glibc.html))
4. **Telemetry stance + air-gapped/proxy/uninstall/XDG** — opt-in vs opt-out (the
   Homebrew/Next.js/Deno controversies). (fetched:
   [clig.dev CLI guidelines](https://clig.dev/),
   [gh CLI opt-out telemetry](https://github.blog/changelog/2026-04-22-github-cli-opt-out-usage-telemetry/),
   [Go's GODEBUG compat](https://go.dev/doc/godebug))

## Second pass — the four gaps (2026-06-24, 24/25 verified, 1 refuted)

A focused follow-up (111 agents, 28 sources) closed the gaps:

### A. Install delivery — harden curl|sh, don't abandon it
`curl … | sh` is the **de-facto primary pattern** (uv, rustup, Deno, Bun) and users
expect it — but ship it **hardened**, plus alternatives:
- **In the script:** TLS-only, `set -euf`, idempotent, fail closed on partial
  download; verify a checksum/signature before executing the binary.
- **Offer an inspect-before-run path** (download → read → run) and a **direct binary
  download**, prominently.
- **Signature verification:** **minisign sidecar** (via cargo-binstall) + Sigstore
  attestations. *Refuted:* piped `sget … | bash` (sget is archived) — **adopt the
  pattern, not that tool**.
- **Broad package-manager matrix:** Homebrew, WinGet, Scoop, Docker.
- **Reject bare `curl | bash` with no verification and no alternative.**
*Sources:* [uv install](https://docs.astral.sh/uv/getting-started/installation/),
[a safer curl|bash](https://blog.sigstore.dev/a-safer-curl-bash-7698c8125063/),
[cargo-binstall SIGNING](https://github.com/cargo-bins/cargo-binstall/blob/main/SIGNING.md),
[dangers of curl|bash](https://lukespademan.com/blog/the-dangers-of-curlbash/).

### B. Lockfile & registry — tamper-evident, mandatory hashes, no install-time resolver
**Reject npm's mutable registry.** Adopt Go's tamper-evident model: **immutable
checksums** (`go.sum`-style) plus, eventually, a **transparency log** that
authenticates even first-time downloads. For the Python side, follow **PEP 751
`pylock.toml`** (Final 2025-03-31): **mandatory per-package sha256, no install-time
resolver** → fully reproducible installs. **Unify** one lockfile to pin Helix deps
*and* the managed Python interpreter + wheels.
*Caveat:* a sumdb-style transparency log may be overkill at a young language's scale —
a **signed, mandatory-hash lockfile may be sufficient**; revisit the log if/when an
ecosystem forms.
*Sources:* [Go sumdb proposal](https://go.googlesource.com/proposal/+/master/design/25530-sumdb.md),
[PEP 751](https://peps.python.org/pep-0751/),
[pylock.toml spec](https://packaging.python.org/en/latest/specifications/pylock-toml/),
[JSR design](https://deno.com/blog/jsr-is-not-another-package-manager).

### C. Signing & Linux portability
- **macOS:** Developer ID signing → **notarization** → **stapling**. (Un-notarized
  binaries are blocked by Gatekeeper; note the curl-download vs browser-download
  quarantine nuance.)
- **Windows:** **EV certs NO LONGER bypass SmartScreen as of 2024** — so don't pay for
  EV just for that. Use **Azure Trusted/Artifact Signing (~$10/mo, no HSM)**; OV certs
  have required an HSM since June 2023. **SmartScreen reputation accrues per file-hash
  over weeks**, and even a *signed* new binary can trip **AV false-positives** — plan
  for a reputation ramp.
- **Linux musl vs glibc:** musl maximizes portability (great for air-gapped/locked-
  down boxes) but its **default allocator is a documented performance cliff** — if
  shipping a musl static build, **swap in another allocator** (mimalloc/jemalloc).
  Recommendation: **glibc build against an old baseline as the primary; a musl static
  build as the max-portability/air-gapped option** (with a non-default allocator).
*Sources:* [SmartScreen reputation](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation),
[code-signing options](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options),
[musl allocator perf](https://nickb.dev/blog/default-musl-allocator-considered-harmful-to-performance/),
[rust+musl malloc](https://raniz.blog/2025-02-06_rust-musl-malloc/),
[macOS without notarization](https://lapcatsoftware.com/articles/without-notarization.html).

### D. Telemetry & operational defaults
- **Telemetry: OPT-IN, off by default.** Go *reversed* its opt-out plan to opt-in
  after backlash; even then it uploads only weekly aggregate counts (<once/install/
  year). For a scientist/air-gapped audience, **local-first, opt-in, with a fully
  offline mode** is the trust-preserving default. (The gh-CLI opt-out rollout drew the
  predictable backlash.)
- **Binary size (the core is ~65 MB):** apply `strip = true`, `panic = "abort"`
  (already set), `lto = "fat"` + `codegen-units = 1` (already set). **Do NOT use
  `opt-level = "z"`** — it trades the speed that is Helix's whole point. `strip` is the
  free win (symbols only, no perf cost).
- **Dirs/uninstall/PATH:** follow **XDG base directories** (+ platform equivalents:
  macOS `~/Library`, Windows `%LOCALAPPDATA%`) for cached managed runtimes; ship a
  clean **uninstall** (binary + caches + managed Python); manage PATH across bash/zsh/
  fish/PowerShell on install.
- **Air-gapped/proxy:** respect proxy env vars + system cert stores; provide an
  **offline bundle** (binary + a pinned Python) for machines with no network.
*Sources:* [Go telemetry discussion](https://github.com/golang/go/discussions/58409),
[Go 1.23 telemetry opt-in](https://devclass.com/2024/08/14/go-1-23-released-with-telemetry-uploaded-to-google-but-opt-in-after-developer-feedback/),
[clig.dev](https://clig.dev/), [XDG basedir](https://specifications.freedesktop.org/basedir/latest/),
[min-sized-rust](https://github.com/johnthagen/min-sized-rust),
[perf-book build config](https://nnethercote.github.io/perf-book/build-configuration.html).

# Security policy

## Reporting a vulnerability

Please report security issues privately using GitHub's
[private vulnerability reporting](https://github.com/areeb-h/helix/security/advisories/new)
rather than opening a public issue.

This is a young project maintained by one person. Expect an acknowledgement within a few
days, not within hours — and if something is being actively exploited, say so in the
subject line so it is not queued behind ordinary work.

## What is in scope

Helix runs untrusted-ish input in several places, and these are the ones worth reporting:

- **Memory unsafety reachable from a Helix program.** The interpreter, VM and value model
  contain no `unsafe`; it is concentrated in the JIT's FFI boundary and SIMD paths
  (see [docs/memory-safety.md](docs/memory-safety.md) for the measured breakdown). A
  Helix program that causes a segfault, a use-after-free, or an out-of-bounds access is a
  security bug, not merely a crash.
- **Sandbox escape in the capability system.** `helix.toml` declares capabilities
  (filesystem, network); a program obtaining an effect it was not granted is in scope.
- **The HTTP client's hardening boundaries**
  ([ADR 0031](docs/adr/0031-http-client-hardening.md), complete in v0.4.0). Header
  injection is refused in both directions; redirects strip `Authorization` and `Cookie`
  on an origin change and never downgrade https; the cookie jar refuses supercookies via
  the Public Suffix List (`src/cookiejar.rs`). A request that crosses any of those
  boundaries is in scope.
- **The website playground** (maintained in its own repository — the `website/` tree
  moved out of this one) executes submitted programs when `HELIX_PLAYGROUND=1`. It
  refuses anything the binary's own registry marks impure, runs in a temp directory with
  a minimal environment, and enforces a timeout and output cap. A way around any of that
  is in scope.
- **The installers** (`install.sh`, `install.ps1`) download and execute a binary. They
  verify SHA-256 against a published `SHA256SUMS` and abort on mismatch. A way to make
  them install an unverified or substituted artifact is in scope.
- **CI workflow injection** — anything that lets a fork or a pull request obtain the
  repository's `GITHUB_TOKEN` or write to releases.

## What is not in scope

- **A Helix program that crashes the process without memory unsafety.** A panic or an
  abort on malformed input is a correctness bug — please do file it as a normal issue, and
  see ADR-0024, which is an explicit never-abort ratchet with a per-file budget enforced in
  CI. It is just not a security report.
- **Denial of service by writing an expensive program.** Helix will happily run an infinite
  loop; that is the language working.
- **Findings against the benchmark fixtures** in `bench/` — those are throwaway artifacts,
  not shipped code.

## Supported versions

Only `main` and the most recent tagged release. There are no backported security fixes
for older tags at this stage of the project — that would be a promise this project cannot
currently keep, and saying so plainly is better than implying otherwise.

# ADR 0010 — Networking, privacy & security

- **Status:** Proposed (no networking exists yet; this governs the first code that adds it)
- **Date:** 2026-06-24
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0009 — Distribution & installation](0009-distribution-and-install.md),
  [ADR 0008 — CPython interop](0008-cpython-interop.md),
  [distribution research](../research/2026-06-24-distribution-toolchain.md)

## Context

Helix today has **no networking** — no HTTP client, no sockets, no TLS. The core
(interpreter, VM, JIT, Polars-on-local-files) is fully offline. This is a deliberate
strength: local-first, air-gapped-friendly, with no unexpected outbound connections.

Five *optional, future* features each require the network: **managed-Python
downloads**, **package fetching**, **toolchain auto-download** (the Go-style version
directive), **self-update**, and **opt-in telemetry**. None require a server, async
I/O, or a persistent connection; they are downloads plus, at most, one small upload.
The moment any of them ships, Helix takes on a network attack surface and (for
telemetry) a privacy/GDPR surface. This ADR establishes the rules *before* that code
is written, so the first change that adds a download does so correctly.

## Decision

### D1 — The first law: verify everything; TLS is not the trust boundary

**Every byte fetched from the network is untrusted until its signature/checksum
verifies against a pinned key or a hash recorded in the lockfile** — regardless of
how it arrived. TLS protects the *transport*; it does **not** protect against a
compromised mirror, a malicious registry, or the **corporate TLS-intercepting proxy**
that legitimately decrypts traffic on most institutional machines. So:

- Managed Python, packages, and toolchains: verify against lockfile hashes
  (go.sum / PEP 751 style) and/or publisher signatures (minisign / Sigstore) — see
  ADR 0009 §6–7.
- **Self-update:** verify the new binary's signature **before** swapping it; replace
  atomically; support rollback. (A self-updater that trusts TLS alone constitutes
  remote code execution on every user.)
- Reject any "download and run/extract without verification" path.

### D2 — Networking stack: HTTPS-only, pure-Rust, self-contained

- **HTTPS only.** No plaintext HTTP, and no disabling of certificate verification.
- **`ureq` + `rustls`**, *not* `reqwest` + `tokio` (which drags in an async runtime)
  and **never system OpenSSL** — linking OpenSSL re-introduces a system dependency and
  **breaks the self-contained-binary property**. `rustls` is pure-Rust TLS.
- Use **`rustls-platform-verifier`** to honor the **system certificate store**, so
  installs work behind corporate MITM proxies (whose custom CA must be trusted).
- Respect `HTTPS_PROXY` / `NO_PROXY`; ship an **offline mode** that makes **no**
  network calls, plus **offline bundles** (binary plus a pinned Python) for air-gapped
  machines. "No network unless requested" is both a usability property and a security
  posture.

### D2a — Transfer protocols: HTTPS + JSON only; no gRPC; networking out of the language

- **The toolchain wire protocol is HTTPS only.** Every networked feature is a **GET**
  (download a Python / package / toolchain / self-update binary) plus, at most, one
  small **POST** (opt-in telemetry). No server, no streaming, no persistent
  connection.
- **JSON for metadata** (release manifests, a package index, telemetry payloads) — the
  universal, CDN-friendly, human-readable choice. (The *lockfile* is TOML, which is
  human-facing configuration, not a wire format.)
- **gRPC is rejected** for the toolchain: it requires protobuf plus HTTP/2 plus a
  running service plus a heavy async stack (tonic/tokio) to accomplish what is
  fundamentally "fetch a file and an index." It is excessive, and it re-introduces the
  async-runtime weight that D2 avoids. Likewise no websockets, SFTP, or custom
  protocols — plain HTTPS from CDNs and GitHub Releases.
- **Data-access HTTP and JSON are core language capabilities.** An earlier version of
  this ADR characterized language networking as a non-goal; that conflated the
  *toolchain's* network posture with what a *program* can do. Fetching and consuming
  data is a core scientific task, so **`http_get` and `parse_json`/`to_json` ship in
  the default build** (the `http` feature, default-on; `--no-default-features` yields a
  network-free binary for locked-down machines).
- **Serving APIs, gRPC, websockets, Kafka, and similar are out of the core.** That is
  web-backend territory — niche for scientists and heavy (protobuf/HTTP-2/servers).
  The escape hatch is **Python interop** (`import python.grpc`, `import python.fastapi`),
  consistent with the rest of the ecosystem strategy (ADR 0008).
- **A program's `http_get` is distinct from the toolchain's posture.** The *tool* still
  never connects unprompted (telemetry is opt-in; downloads are verified). A user
  calling `http_get` is an explicit program action — a capability, not the toolchain
  reaching out.

### D3 — Privacy / telemetry: opt-in, off by default, no personal data

Telemetry is the only GDPR-relevant surface (downloads create only standard CDN
access logs). The posture (already chosen in ADR 0009 §9):

- **Opt-in, off by default**, with a one-flag / env-var off switch and a fully
  offline mode. (Go *reversed* opt-out → opt-in after backlash; the target audience is
  often air-gapped or regulated.)
- **Collect no personal data** — no IP storage, no machine/user IDs, no file
  paths, no usernames. **Aggregate counts only** ("command X ran N times this week",
  the Go-telemetry model).
- **Consent and transparency:** explicit opt-in, a plain-language privacy document
  stating exactly what is sent and why, purpose-limited, with the right not to
  participate satisfied by construction.
- **Default to collecting nothing**; treat any future collection as a separately
  consented feature. *(Engineering guidance, not legal advice — obtain counsel before
  any data collection ships.)*

### D4 — Threat model & required mitigations for networked features

| Threat | Mitigation |
|---|---|
| Compromised mirror / registry / MITM proxy | **Signature + hash verification** (D1), not TLS alone |
| Malicious self-update | Verify-before-swap, atomic replace, rollback (D1) |
| Zip-slip / path traversal / symlink attack on extraction | Sanitize archive paths; refuse `..`/absolute/symlink escapes |
| Supply-chain (managed Python / packages = remote code) | Pinned hashes + provenance (python-build-standalone reproducible builds) |
| Cache poisoning / world-writable caches | Correct permissions; never execute un-verified content; XDG dirs |
| Telemetry as exfil / privacy leak | Opt-in, anonymized, aggregate-only, disable-able (D3) |
| Surprise outbound connections (audit/compliance) | Offline-by-default; every network call is an explicit, documented action |

## Rationale

- **TLS-versus-verification is the lesson behind most supply-chain incidents** — the
  wire was encrypted; the *content* was malicious. Hash/signature verification is the
  only measure that survives a hostile mirror or an intercepting proxy.
- **`rustls` over OpenSSL** preserves the single-self-contained-binary value
  proposition on which the entire distribution strategy (ADR 0009) rests.
- **Opt-in, no-PII telemetry** keeps the GDPR burden minimal *and* matches the trust
  expectations of scientists on institutional machines — the same population that
  requires air-gapped support.

## Rejected alternatives

- **Plaintext HTTP or "TLS is sufficient".** Defeated by mirrors and intercepting
  proxies; verification is mandatory.
- **`reqwest` + `tokio`.** A full async stack for occasional downloads — unnecessary
  weight and complexity for a CLI; and the common OpenSSL backend breaks
  self-containment.
- **Opt-out (or always-on) telemetry.** The documented backlash pattern
  (Homebrew, gh-CLI, Go's reversal); wrong for an air-gapped/regulated audience.
- **A bespoke download protocol.** Reinventing HTTPS, with no benefit.

## Consequences

- The first networked feature must add the `[net]` layer with verification built in;
  it cannot be retrofitted safely later. **Managed CPython (ADR 0008/0009) is the
  first consumer and the location where this is enforced first.**
- New runtime dependencies appear (`ureq`, `rustls`, `rustls-platform-verifier`), but
  only behind the features that require them, keeping the pure-language core
  dependency-free.
- A published **privacy policy** and **security disclosure process** (for example,
  `SECURITY.md`) become commitments once any networked feature or telemetry ships.

## Open questions

- Signature scheme **per artifact class**: minisign (binaries/self-update) versus
  Sigstore (provenance) versus lockfile hashes (packages) — likely all three, scoped
  by use.
- Where managed runtimes/caches reside per OS (XDG versus `~/Library` versus
  `%LOCALAPPDATA%`) and their permission model — to be fixed when managed CPython
  lands.
- Whether self-update is desirable at all versus delegating to OS package managers
  (self-update is an attractive attack target; some tools deliberately omit it).

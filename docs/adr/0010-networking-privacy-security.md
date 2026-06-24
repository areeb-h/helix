# ADR 0010 — Networking, privacy & security

- **Status:** 📝 proposed (no networking exists yet; this governs the first code that adds it)
- **Date:** 2026-06-24
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0009 — Distribution & installation](0009-distribution-and-install.md),
  [ADR 0008 — CPython interop](0008-cpython-interop.md),
  [distribution research](../research/2026-06-24-distribution-toolchain.md)

## Context

Helix today has **zero networking** — no HTTP client, no sockets, no TLS. The core
(interpreter, VM, JIT, Polars-on-local-files) is fully offline. This is a deliberate
strength: local-first, air-gapped-friendly, no surprise outbound connections.

But five *optional, future* features each need the network: **managed-Python
downloads**, **package fetching**, **toolchain auto-download** (the Go-style version
directive), **self-update**, and **opt-in telemetry**. None need a server, async I/O,
or a persistent connection — they are downloads plus, at most, one small upload. The
moment any of them ships, Helix takes on a network attack surface and (for telemetry)
a privacy/GDPR surface. This ADR sets the rules *before* that code is written, so the
first PR that adds a download already does it correctly.

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
  atomically; support rollback. (A self-updater that trusts TLS alone is remote code
  execution on every user.)
- Reject any "download and run/extract without verification" path.

### D2 — Networking stack: HTTPS-only, pure-Rust, self-contained

- **HTTPS only.** No plaintext HTTP, no disabling cert verification, ever.
- **`ureq` + `rustls`**, *not* `reqwest` + `tokio` (drags an async runtime) and
  **never system OpenSSL** — linking OpenSSL re-introduces a system dependency and
  **breaks the self-contained-binary property**. `rustls` is pure-Rust TLS.
- Use **`rustls-platform-verifier`** to honor the **system certificate store**, so
  installs work behind corporate MITM proxies (whose custom CA must be trusted).
- Respect `HTTPS_PROXY` / `NO_PROXY`; ship an **offline mode** that makes **zero**
  network calls, plus **offline bundles** (binary + a pinned Python) for air-gapped
  machines. "No network unless you ask" is both DX and a security posture.

### D2a — Transfer protocols: HTTPS + JSON only; no gRPC; networking out of the language

- **Toolchain wire protocol is just HTTPS.** Every networked feature is a **GET**
  (download a Python / package / toolchain / self-update binary) plus, at most, one
  small **POST** (opt-in telemetry). No server, no streaming, no persistent
  connection.
- **JSON for metadata** (release manifests, a package index, telemetry payloads) — the
  universal, CDN-friendly, human-readable choice. (The *lockfile* is TOML — it's
  human-facing config, not a wire format.)
- **gRPC is rejected** for the toolchain: it needs protobuf + HTTP/2 + a running
  service + a heavy async stack (tonic/tokio), to do what is fundamentally "fetch a
  file and an index." Massive overkill; also re-introduces the async-runtime weight D2
  avoids. Likewise no websockets / SFTP / custom protocols — plain HTTPS from CDNs /
  GitHub Releases.
- **Language-level networking is a deliberate non-goal of the core.** A Helix *program*
  does not natively speak HTTP/gRPC/Kafka/etc. — the core stays local-first. When data
  must come over the network: (1) Polars already reads CSV/Parquet/JSON from URLs /
  object stores (can be enabled behind a feature later); (2) for arbitrary protocols,
  the escape hatch is **Python interop** (`import python.requests`,
  `import python.grpc`), exactly like the rest of the ecosystem strategy (ADR 0008).
  Native protocol support is delegated, not built into the language.

### D3 — Privacy / telemetry: opt-in, off by default, no personal data

Telemetry is the only GDPR-relevant surface (downloads create only standard CDN
access logs). The posture (already chosen in ADR 0009 §9):

- **Opt-in, off by default**, with a one-flag / env-var off switch and a fully
  offline mode. (Go *reversed* opt-out → opt-in after backlash; our audience is often
  air-gapped or regulated.)
- **Collect no personal data, ever** — no IP storage, no machine/user IDs, no file
  paths, no usernames. **Aggregate counts only** ("command X ran N times this week",
  Go-telemetry model).
- **Consent + transparency:** explicit opt-in, a plain-language privacy document
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

- **TLS-vs-verification is the lesson behind most supply-chain incidents** — the wire
  was encrypted; the *content* was malicious. Hash/signature verification is the only
  thing that survives a hostile mirror or an intercepting proxy.
- **`rustls` over OpenSSL** preserves the single-self-contained-binary value prop that
  the whole distribution strategy (ADR 0009) rests on.
- **Opt-in, no-PII telemetry** keeps GDPR burden near-zero *and* matches the trust
  expectations of scientists on institutional machines — the same population that
  needs air-gapped support.

## Rejected alternatives

- **Plaintext HTTP or "TLS is enough".** Defeated by mirrors and intercepting proxies;
  verification is mandatory.
- **`reqwest` + `tokio`.** A full async stack for occasional downloads — needless
  weight and complexity for a CLI; and the common OpenSSL backend breaks
  self-containment.
- **Opt-out (or always-on) telemetry.** The documented backlash pattern
  (Homebrew, gh-CLI, Go's reversal); wrong for an air-gapped/regulated audience.
- **A bespoke download protocol.** Reinventing HTTPS; no benefit.

## Consequences

- The first networked feature must add the `[net]` layer with verification built in —
  it cannot be retrofitted safely later. **Managed CPython (ADR 0008/0009) is the
  first consumer and the place this is enforced first.**
- New runtime dependencies appear (`ureq`, `rustls`, `rustls-platform-verifier`) —
  but only behind the features that need them, keeping the pure-language core dep-free.
- A published **privacy policy** and **security disclosure process** (e.g.
  `SECURITY.md`) become commitments once any networked feature or telemetry ships.

## Open questions

- Signature scheme **per artifact class**: minisign (binaries/self-update) vs Sigstore
  (provenance) vs lockfile hashes (packages) — likely all three, scoped by use.
- Where managed runtimes/caches live per OS (XDG vs `~/Library` vs `%LOCALAPPDATA%`)
  and their permission model — to be fixed when managed CPython lands.
- Whether self-update is even desirable vs delegating to OS package managers
  (self-update is a tempting attack target; some tools deliberately omit it).

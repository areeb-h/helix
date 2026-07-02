# ADR 0022 — HTTP version roadmap: keep-alive now, HTTP/2 & HTTP/3 via established stacks

- **Status:** Proposed
- **Date:** 2026-07-02
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0010 — Networking, privacy, security](0010-networking-privacy-security.md),
  [ADR 0021 — Capability sandbox](0021-capability-sandbox.md)

## Context

Helix ships a from-scratch HTTP server on `std::net` (`listen`/`accept`/`respond`,
`poll`, SSE `sse`/`send`, `SO_REUSEPORT` sharding). It is deliberately minimal:
**HTTP/1.0-style, connection-per-request** — every response carries `Connection: close`
and the socket is dropped after one exchange. No keep-alive, no HTTP/2, no HTTP/3.

Measured on this machine (localhost, plaintext hello):

| Server | conn-per-request | keep-alive |
|---|---|---|
| Helix (6-shard) | ~43k req/s | ~44k (no effect — server closes) |
| Go `net/http` (6-core) | ~38k req/s | **~157k req/s** |

The benchmark is unambiguous: **connection-per-request is the wall.** Even Go can't beat
~40k without keep-alive; *with* it, Go reaches ~157k. Helix already matches Go's
conn-per-request number (43k ≈ 38k), so the VM is not the bottleneck there —
**connection reuse is the single highest-value lever**, and the only path to the
"Rust axum/actix tier" (100k–500k+) is keep-alive + multi-shard.

The modern protocol landscape (for reference — semantics per RFC 9110, shared across
versions):

| Version | Transport | Characteristic |
|---|---|---|
| HTTP/1.1 | TCP | Persistent connections (keep-alive), one request at a time per connection |
| HTTP/2 (RFC 9113) | TCP + TLS | Multiplexed streams over one connection; HPACK header compression |
| HTTP/3 (RFC 9114) | **QUIC over UDP** | Independent streams (no TCP head-of-line blocking); 0/1-RTT setup; connection migration; TLS 1.3 built in; QPACK |

## Decision

Adopt the standard layered strategy — **HTTP/3 when available, HTTP/2 as the normal
fallback, HTTP/1.1 for compatibility** — but stage it by tractability and, critically,
**never hand-roll the hard transports**:

> Do not build an HTTP/3 (QUIC) or HTTP/2 stack from scratch for production. QUIC in
> particular is where weekend projects go to acquire packet-loss trauma. Use an
> established implementation.

### Stage 1 — HTTP/1.1 keep-alive: needs a cooperative event loop, NOT the blocking model

**This was prototyped and benchmarked, and the naive form is a regression — recorded here
so it isn't re-attempted.** The obvious design (`respond` keeps the socket open + sends
`Connection: keep-alive`; `request()` re-reads the next request from a persistent buffered
reader; the program loops `accept → while request(): respond`, constant-space via TCO;
+ `TCP_NODELAY` to kill the Nagle/delayed-ACK ~40 ms stall) was implemented in full.

Measured (6 shards, localhost, this box):

| Load | req/s | failures |
|---|---|---|
| conn-per-request, conc=100 | **47,003** | 0 |
| keep-alive, conc=100 | 22,354 | **94** |
| keep-alive, conc=300 | 27,237 | **294** |

The blocking model **serializes** keep-alive: a shard that enters `while request(): respond`
on a persistent connection is *pinned* to that one client until it closes, so the other
`conc − shards` connections **starve** (the 94 / 294 failures) and throughput roughly
*halves*. Go/actix reach 157k because they interleave every connection on an async runtime
(thread-per-connection / task-per-connection). A blocking server cannot — closing after each
request (conn-per-request) is actually **better** for concurrent load, because it cycles
through all connections instead of pinning one per shard. So naive keep-alive was reverted.

Keep-alive is only a win when connections are **interleaved**. Two ways to get that:
- **Cooperative event loop, in-model (no deps):** the same pattern Helix already uses for
  SSE (`poll()` + `send` to many). It needs a **non-blocking `request()`** — peek a
  connection for a *ready* request (parse if bytes are buffered, else `missing`) — plus a
  program that keeps a list of open connections and services whichever is ready each tick.
  This multiplexes many keep-alive connections on one thread with no async runtime. This is
  the real Stage 1, and it is a genuine new primitive + event-loop shape, not a small patch.
- **Async runtime (Stage 2/3 stack):** falls out for free once on tokio+hyper (below).

Net for the blocking `std::net` server: **stay conn-per-request** (sharded ≈ 47–51k here —
a fine floor), and pursue interleaved keep-alive via the cooperative loop or the async stack.

### Stage 2 — HTTP/2, and Stage 3 — HTTP/3, via an async stack

HTTP/2 (binary framing, HPACK, stream multiplexing, flow control) and HTTP/3 (QUIC)
both realistically require an **async runtime** and mature crates:

- **HTTP/2:** `h2` / `hyper` (Tokio-based).
- **HTTP/3:** a real QUIC implementation — **Quinn** (pure-Rust, Tokio), or `quiche`
  (Cloudflare), `ngtcp2`, `msquic`. Paired with `h3`/`h3-quinn` for the HTTP/3 layer.

This is a genuine architectural fork: Helix's server today is **synchronous** (blocking
`accept` + cooperative `poll`, share-nothing shards, no `Arc`, no runtime). HTTP/2 and
HTTP/3 pull in **Tokio + hyper + quinn**, which is a large dependency and a different
concurrency model. That is the right trade for a production web tier, but it is a
**deliberate major-version decision**, not an incremental patch, and it must:

- keep the sandbox story (ADR 0021) — `listen` stays `Net`-gated; the async listener is
  attenuated the same way;
- keep TLS confined to the vetted crates (rustls under hyper/quinn) — Helix never
  implements crypto transport itself;
- feature-gate it (like the existing `http` client feature) so a network-free / minimal
  build stays dependency-light;
- preserve the pure-Helix `accept`/`poll`/SSE surface for the simple case; the async
  stack backs a new high-throughput server entry point, it doesn't replace the minimal one.

### Non-negotiable principle

**Helix implements HTTP *semantics and its own 1.x parsing*; it does not implement QUIC,
TLS, or the HTTP/2 framing layer from scratch for production.** Those are delegated to
audited, widely-deployed crates. This mirrors the language's existing stance (delegate
bio formats to `noodles`/`needletail`, matmul to `faer`, crypto to vetted crates) — build
the parts that are the product, borrow the parts that are a decade of hardening.

## Consequences

- **Now:** the blocking `std::net` server stays **conn-per-request** (sharded ≈ 47–51k
  req/s here — a solid floor). Naive keep-alive was prototyped, measured to *regress*
  concurrent load (serialization/starvation), and reverted.
- **Next (in-model, no deps):** interleaved keep-alive via a **cooperative event loop** — a
  non-blocking `request()` primitive + a poll-based multi-connection serve loop (the same
  shape as SSE). This is the real path to higher concurrent throughput without an async
  runtime; it's a new primitive, so its own focused change.
- **Later, as a major version:** Stages 2–3 behind a feature flag, on Tokio + hyper +
  Quinn, TLS via rustls, sandbox-preserving. Never a hand-rolled QUIC/TLS/H2 core. The
  async stack gets HTTP/1.1 keep-alive, HTTP/2, and HTTP/3 all at once.
- The minimal `std::net` server stays for dev/simple use and the SSE cooperative model;
  the async stack is additive.

## Sources

- RFC 9110 (HTTP semantics), RFC 9112 (HTTP/1.1), RFC 9113 (HTTP/2), RFC 9114 (HTTP/3),
  RFC 9000 (QUIC).
- Quinn (https://github.com/quinn-rs/quinn), quiche, ngtcp2, msquic; `hyper`/`h2`/`h3`.
- Benchmark data: measured in this repo's session (Go `net/http` control vs Helix,
  localhost, keep-alive on/off).

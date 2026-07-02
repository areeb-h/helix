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

### Stage 1 — HTTP/1.1 keep-alive (now; in the current sync model)

The 4×+ throughput lever, and it needs **no new dependency and no async runtime**. It fits
the existing blocking/`poll` server once tail-call optimization is in place (ADR-less perf
commit — done: the per-connection loop is now constant-space, so a keep-alive loop can't
leak).

Design (a connection-oriented shape layered over today's request/respond):
- `respond` sends `Connection: keep-alive` (+ `Content-Length`, already present) and
  **keeps the socket open** instead of dropping it, unless the request asked to close
  (`Connection: close`) or an idle/2xx-count budget is hit.
- The connection becomes re-readable: after `respond`, the program pulls the **next**
  request on the same connection (`conn.request()` re-parses from the socket; `missing`
  when the client closes or an idle timeout fires).
- The accept loop gains an inner per-connection loop:
  `accept → while conn.request(): respond`. Constant-space thanks to TCO.
- Bounds: per-connection request cap + idle read timeout (already have `READ_TIMEOUT`),
  so a kept-alive connection can't be held forever (slowloris).

This is an API change to the connection model, so it lands as its own focused,
oracle/gate-verified change and the `web` lib's `serve_loop` is updated in lockstep.

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

- **Now:** implement Stage 1 (keep-alive) — the measured 4×+ win, no new deps. Sharded
  across 6 cores this targets ~100k+ req/s on this box (the low end of the Rust tier).
- **Later, as a major version:** Stages 2–3 behind a feature flag, on Tokio + hyper +
  Quinn, TLS via rustls, sandbox-preserving. Never a hand-rolled QUIC/TLS/H2 core.
- The minimal `std::net` server stays for dev/simple use and the SSE cooperative model;
  the async stack is additive.

## Sources

- RFC 9110 (HTTP semantics), RFC 9112 (HTTP/1.1), RFC 9113 (HTTP/2), RFC 9114 (HTTP/3),
  RFC 9000 (QUIC).
- Quinn (https://github.com/quinn-rs/quinn), quiche, ngtcp2, msquic; `hyper`/`h2`/`h3`.
- Benchmark data: measured in this repo's session (Go `net/http` control vs Helix,
  localhost, keep-alive on/off).

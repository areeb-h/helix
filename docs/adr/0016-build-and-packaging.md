# ADR 0016 — Build performance, allocator & containerization

- **Status:** Accepted — implemented (allocator, musl, Docker, CI); PGO wired in CI
- **Date:** 2026-06-26
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0009 — Distribution & install](0009-distribution-and-install.md)
  (this implements its §8 allocator/musl decision),
  [ADR 0011 — Core/stdlib boundary](0011-core-stdlib-boundary.md)

## Context

The release profile was already maximal (`opt-level=3`, fat LTO, `codegen-units=1`,
`panic=abort`, strip), but three runtime-performance and packaging levers were unused:
no custom global allocator, no profile-guided optimization, and no static / container
artifact. The governing constraint for this work was **performance-improvements-only**:
every change must be a runtime-perf win or neutral, with no regression and no
portability loss in the default artifact — enforced by a measured before/after gate.

Three facts shaped the design:

- `helix` is a **bin crate** (no lib/dylib), so the PGO×fat-LTO bug
  ([rust#117220](https://github.com/rust-lang/rust/issues/117220)) does not apply.
- The **Cranelift JIT is gated to x86_64-linux** (`src/jit.rs`); elsewhere the bytecode
  VM runs everything. The JIT is the branchy, hot code PGO most rewards.
- A Rust `#[global_allocator]` overrides only Rust allocations; it does not interpose
  libc `malloc` or CPython's allocators, so a custom allocator composes safely with the
  embedded-CPython `python` feature (mimalloc's `override` feature stays off).

## Prior approaches and their documented shortcomings

| Approach | Documented pain |
|----------|-----------------|
| System (glibc) malloc, no override | Leaves allocation-heavy paths (Arrow buffers, AST/`Value` churn) on the table; Polars officially recommends a custom allocator. |
| musl-static with the default musl malloc | A documented multithreaded performance cliff (~10× on contended allocation) — a static binary that is *slow*. |
| No PGO | The interpreter/VM dispatch and JIT-build path benefit ~10–20% from profile-guided code layout; foregone. |
| `target-cpu=native` for SIMD | Produces a binary that **SIGILLs on older CPUs** — a portability *regression*, unacceptable for a distributed default artifact. |
| Distributing on a Debian/Ubuntu base image | Fat image, large OS/CVE surface, for what is a single static binary. |

## Decision

1. **mimalloc as the global allocator, everywhere** (default-on `mimalloc` feature,
   `default-features = false` = the fast config, no `secure` hardening), **with
   `purge_delay = 0`** set at startup (`libmimalloc-sys` `mi_option_set`, enum index 15
   in the v3 build). Helix processes are short-lived (CLI/serverless) and exit before
   mimalloc's default ~10 ms purge fires, leaving freed pages resident; immediate
   purging keeps the wall-time win while returning peak RSS to ~system-allocator levels
   on the data workloads. One allocator across glibc/musl/macOS/Windows; the documented
   fix for musl's malloc; safe with the `python` feature.
2. **Profile-guided optimization on x86_64-unknown-linux-gnu only** — the sole target
   where the JIT exists and cargo-pgo is fully supported natively. Instrument with thin
   LTO (throwaway), train on the representative `bench/crosslang` workloads (B1–B7, JIT
   and VM paths), optimize keeping the project's fat-LTO release profile. A regression
   gate makes the PGO build the **sole publisher of the gnu asset** (it can never be
   slower than a plain release). Other targets keep their plain release build.
3. **A static musl artifact** (`x86_64-unknown-linux-musl` + `+crt-static`, scoped to
   that target) plus a **multi-stage Dockerfile** that drops the static binary onto
   `distroless/static` — an image that is essentially just the binary.
4. **A measured regression gate** (`scripts/perf-verify.sh`) — best-of-3 wall time and
   peak RSS per workload, candidate vs baseline — is the "only performance improvements"
   guarantee and runs in CI before the PGO asset ships. An RSS regression requires
   **both** a ratio breach **and** a meaningful absolute increase (> 50 MB), so a custom
   allocator's small fixed arena overhead does not fail tiny benchmarks (only a
   proportional blow-up counts).

## Rationale

- **mimalloc-universal** is "one obvious way" (ADR 0009's single-binary philosophy),
  musl-friendly (the whole point of the static image), and a win or neutral everywhere;
  Helix's hottest paths (JIT loops, faer/ndarray) are allocator-cold, so the data
  workloads (B3/B6) are where it helps and where the gate watches.
- **PGO on gnu only** spends the build complexity where the hot code (the JIT) actually
  exists and where cargo-pgo is reliable; cross-compiled aarch64 can't run its own
  instrumented binary to train, and macOS/Windows lack the JIT.
- **bin crate** sidesteps the one real PGO×LTO hazard, so the shipped binary keeps fat
  LTO *and* gains PGO.
- **distroless/static + mimalloc** is a legitimately more modern container story than
  the Python-on-Debian or JVM norms: tiny, ~zero CVE surface, fast cold start, no Python
  layer — and the allocator removes the only reason a static musl build would be slow.

## Rejected alternatives

- **jemalloc, or platform-conditional jemalloc(glibc)+mimalloc(else)** — rejected for
  v1: marginal OLAP upside over mimalloc, a 3-way cfg matrix, and jemalloc is painful on
  musl / unsupported on MSVC. Kept as a documented fallback if the gate ever shows
  jemalloc beating mimalloc on B3/B6 by a real margin (a one-line swap behind the gate).
- **`target-cpu=native` / a v3 default** — rejected: SIGILLs on older CPUs = a regression
  for the default artifact. A v3 build may ship later only as an *extra* artifact with
  install-time CPU detection.
- **`opt-level="z"` for size** — rejected (ADR 0009): sacrifices speed; this project is
  performance-first.
- **BOLT now** — deferred: +2–5% on Linux-x86_64 only, marginal after PGO; revisit once
  PGO is proven.

## Consequences

- **Easier:** every shipped binary is faster (allocator + PGO on the primary Linux
  target); a tiny distroless image and a static air-gapped binary now exist; the gate
  makes performance regressions a CI failure rather than a silent drift.
- **Harder / committed to:** the release workflow has three Linux jobs (gnu-PGO, musl,
  and the cross/macOS/Windows matrix); PGO adds build time (instrument + train +
  optimize); the gate must be kept representative as workloads evolve.

## Open questions / deferred

- **BOLT** (+2–5%, Linux-x86_64) once PGO is proven.
- **An x86-64-v3 *extra* artifact** + install.sh CPU detection (never the default).
- **jemalloc-on-glibc** fallback if the gate shows it wins the data workloads.
- **Serverless examples** (AWS Lambda `bootstrap`, Cloud Run) and a **WASM/WASI edge
  build** (interpreter+VM only — no JIT, limited Polars) — both noted in
  [docs/deployment.md](../deployment.md).

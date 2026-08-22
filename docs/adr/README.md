# Architecture Decision Records

Each ADR captures one foundational Helix design decision: the context, the
decision, the rationale, and the rejected alternatives (with reasons). These are the
decisions that are expensive or impossible to reverse once code depends on them, so
they are made deliberately and in the open.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-missing-data.md) | Missing data & absence | Implementing — scalar done; Arrow-column part pending |
| [0002](0002-type-system.md) | Type system & inference | Accepted — first iteration implemented (`src/types.rs`) |
| [0003](0003-collection-api.md) | Collection API unity | Implementing — `where` spans Array + DataFrame; full trait pending |
| [0004](0004-functions-errors-mutability.md) | Functions, errors & mutability | Implementing — functions implemented; errors/COW pending |
| [0005](0005-syntax-conventions.md) | Syntax & surface conventions | Accepted |
| [0006](0006-concurrency-and-scale.md) | Concurrency, parallelism & scale | Proposed — DataFrame layer live |
| [0007](0007-tensor-backend.md) | Tensor backend (ndarray now, GPU later) | Implementing — CPU core implemented |
| [0008](0008-cpython-interop.md) | CPython interop (Helix → Python) | Implemented (v1) — feature-gated |
| [0009](0009-distribution-and-install.md) | Distribution & installation | Implementing — CLI + source install; releases wired |
| [0010](0010-networking-privacy-security.md) | Networking, privacy & security | Proposed — governs the first network code |
| [0011](0011-core-stdlib-boundary.md) | Core / stdlib boundary | Accepted — namespaces superseded by 0017 (registry + small-core stand) |
| [0012](0012-dataframe-backend-seam.md) | DataFrame backend seam | Accepted — Phase 1 implemented |
| [0013](0013-package-manager.md) | Package manager & lockfile | Implemented (v1) — path + url deps, hash-pinned lock |
| [0014](0014-gpu-tensor-backend.md) | GPU tensor backend (wgpu, seam-first) | Proposed — design only |
| [0015](0015-sequence-alignment.md) | Sequence alignment | Accepted — v1 implemented (hand-rolled affine-gap aligner) |
| [0016](0016-build-and-packaging.md) | Build perf, allocator & containerization | Accepted — mimalloc + musl + Docker implemented; PGO wired in CI |
| [0017](0017-methods-and-functions.md) | Methods on data + free functions (no namespaces) | Accepted — implemented; supersedes 0011's namespaces |
| [0018](0018-random.md) | Reproducible random numbers (seeded, pure, SplitMix64) | Accepted — implemented (`random`/`randn`/`random_int` + shuffle/sample/choice) |
| [0019](0019-module-system.md) | Module system (`import`, path resolution) | Accepted — implemented |
| [0020](0020-dict-type.md) | `Dict`: keyed map with O(log n) lookup | Accepted — implemented |
| [0021](0021-capability-sandbox.md) | Capability sandbox: deny-by-default authority | Proposed — phase 1 in progress |
| [0022](0022-http-version-roadmap.md) | HTTP version roadmap (keep-alive now, HTTP/2 & HTTP/3 via established stacks) | Accepted — Stage 1 (keep-alive) implemented; HTTP/2 & HTTP/3 proposed |
| [0023](0023-hbc-emitter-artifact-format.md) | `.hbc` emitter & portable core-bytecode artifact format (`helix emit-hbc`) | Accepted — implemented; runs in ctype ring 0, cross-producer verified |
| [0024](0024-total-runtime-no-host-panics.md) | Total runtime: user input never aborts the host | Accepted — implemented + regression-tested; CI lint gate pending |
| [0025](0025-ordering.md) | One order, one domain: `sort` / `argsort` / `min`-`max` / the `_by` family | **Accepted + implemented** — all four (a1/b1/c1/d1); 34 matrix cells moved across three commits, each diff the review; ships in v0.2.0 |
| [0026](0026-library-performance-boundary.md) | Is library code meant to be fast? The indirect-call boundary | **Accepted** — libraries are first-class; monomorphize at the call site; scheduled after the append wall |
| [0027](0027-builtin-shadowing.md) | When does `fn round(x)` start being `round`? | **Accepted** — a shadow is file-scoped and retroactive; **implemented** (`4b74056`) |
| [0028](0028-query-name-resolution.md) | In a DataFrame query, does a bare name mean the column or the binding? | **Accepted** — a binding in scope wins, `@name` still pins the column; **implemented**; breaking, ships in v0.2.0 |
| [0029](0029-linear-accumulation.md) | Is a fold that rebuilds its accumulator allowed to be quadratic on any engine? | **Accepted** — amortized-linear is a language guarantee via Rc-uniqueness take-append-store; not yet implemented; plan in docs/linear-accumulation-plan.md |
| [0030](0030-time.md) | Can a reproducible language tell the time? | **Proposed** — monotonic  as a declared  effect, durations only, judgments-not-raw-times idiom; awaiting acceptance |
| [0031](0031-http-client-hardening.md) | What does an HTTP client owe a program that trusts it? | **Accepted & Implemented** — all four steps landed; redirect boundary rules (credentials stripped cross-origin, no https→http, QUERY keeps its method per RFC 10008), an explicit cookie jar with the Public Suffix List, per-request timeouts and limits, and headers as a case-insensitive type that refuses CRLF |
| [0032](0032-appliance-profile.md) | The appliance profile — a small binary without a smaller language | **Steps 1+2 implemented** — dataframes+bio gates landed: 13.4 MB appliance binary (82% off), full speed, defaults unchanged; jit/tensor open pending re-measure; dtype-full settled (stays) |
| [0033](0033-native-dataframe-engine.md) | A native DataFrame engine — replace polars, staged, with polars as the oracle | **Accepted — Stages 0+1 implemented** (frozen format; backend/native/ with 12/12 differential parity, appliance ships frames at 9.3 MB); Stages 2-3 open |
| [0034](0034-native-frame-semantics.md) | Native frame semantics — frames follow the language | **Accepted** — scalar-kernel evaluation, decided deltas (% euclidean, / true division, /0 errors), aggregation doctrine, CSV policy |

ADRs 0001–0007 are grounded in [verified deep research](../research/2026-06-21-foundational-design.md)
(23 of 25 claims survived 3-vote adversarial verification); ADR 0008 in a
[2026-06-24 interop research pass](../research/2026-06-24-python-interop.md).

Status legend: Researching · Proposed · Implementing · Accepted · Superseded

Decisions are grounded in [deep research](../research/) into what existing
languages did and the documented mistakes that resulted, in order to learn from
them rather than copy them.

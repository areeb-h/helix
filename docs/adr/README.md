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
| [0011](0011-core-stdlib-boundary.md) | Core / stdlib boundary | Accepted |
| [0012](0012-dataframe-backend-seam.md) | DataFrame backend seam | Accepted — Phase 1 implemented |
| [0013](0013-package-manager.md) | Package manager & lockfile | Implemented (v1) — path + url deps, hash-pinned lock |
| [0014](0014-gpu-tensor-backend.md) | GPU tensor backend (wgpu, seam-first) | Proposed — design only |
| [0015](0015-sequence-alignment.md) | Sequence alignment | Proposed — hand-rolled affine-gap aligner |

ADRs 0001–0007 are grounded in [verified deep research](../research/2026-06-21-foundational-design.md)
(23 of 25 claims survived 3-vote adversarial verification); ADR 0008 in a
[2026-06-24 interop research pass](../research/2026-06-24-python-interop.md).

Status legend: Researching · Proposed · Implementing · Accepted · Superseded

Decisions are grounded in [deep research](../research/) into what existing
languages did and the documented mistakes that resulted, in order to learn from
them rather than copy them.

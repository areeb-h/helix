# Architecture Decision Records

Each ADR captures one foundational Helix design decision: the context, the
decision, *why*, and the alternatives we rejected (with reasons). These are the
decisions that are expensive or impossible to reverse once people write code, so
we make them deliberately and in the open.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-missing-data.md) | Missing data & absence | 🛠 scalar done; Arrow-column part pending |
| [0002](0002-type-system.md) | Type system & inference | ✅ first iteration implemented (`src/types.rs`) |
| [0003](0003-collection-api.md) | Collection API unity | 🛠 `where` spans Array + DataFrame; full trait pending |
| [0004](0004-functions-errors-mutability.md) | Functions, errors & mutability | 🛠 functions implemented; errors/COW pending |
| [0005](0005-syntax-conventions.md) | Syntax & surface conventions | ✅ accepted |
| [0006](0006-concurrency-and-scale.md) | Concurrency, parallelism & scale | 📝 proposed (DataFrame layer live) |
| [0007](0007-tensor-backend.md) | Tensor backend (ndarray now, GPU later) | 🛠 CPU core implemented |

Each is grounded in [verified deep research](../research/2026-06-21-foundational-design.md)
(23 of 25 claims survived 3-vote adversarial verification).

Status legend: 🔬 researching · 📝 proposed · 🛠 implementing · ✅ accepted · ❌ superseded

Decisions are grounded in [deep research](../research/) into what existing
languages did and the documented mistakes that resulted — we learn from them
rather than copy them.

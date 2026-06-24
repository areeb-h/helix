# ADR 0007 — Tensor backend

- **Status:** proposed
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude

## Context

Phase 4 adds a native `Tensor` type for dense n-dimensional numeric arrays —
the substrate for linear algebra, scientific computing, and (later) ML. Like the
DataFrame decision (ADR 0003), the principle is **don't reinvent**: pick a mature
engine for the hard parts (n-dim storage, broadcasting, BLAS, later GPU/autodiff)
and keep a clean Helix surface API on top.

The roadmap is explicit: **Phase 4 = tensors (CPU), Phase 6 = GPU**, with ML in
between. So the tensor surface must be stable across a CPU-now / GPU-later split.

## Options

| Backend | What it gives | Cost |
|---|---|---|
| **ndarray** | Mature pure-Rust n-dim arrays, NumPy-like, optional BLAS, great ergonomics | CPU only; no autodiff/GPU |
| **candle-core** | Tensors with CPU/CUDA/Metal + autodiff; built for ML/LLMs | Heavier, `Result`-everywhere + `Device`/`DType` ceremony; friction for simple scientific use |
| **burn** | Full DL framework, backend-agnostic, autodiff | Framework-y; overkill for a language's base tensor type |

## Decision

**Use `ndarray` for the Phase 4 CPU tensor core**, behind a stable Helix
`Tensor` surface API. Re-evaluate **candle** (or burn) for the **Phase 6
GPU + autodiff backend**, plugged in behind the same API.

- Value: `Tensor(Rc<ArrayD<f64>>)` — dynamic rank, `f64` (the scientific
  default; other dtypes later).
- Surface API (backend-independent): `tensor(...)`, `zeros`/`ones`/`eye`,
  `shape`/`reshape`/`transpose`, elementwise arithmetic with **NumPy-style
  broadcasting**, reductions (`sum`/`mean`/`min`/`max`), `matmul`, and the math
  stdlib (`sqrt`/`exp`/… broadcast over tensors).

## Rationale

- **Ergonomics for the actual audience.** Most scientific tensor use isn't NN
  training — it's array math and linear algebra. ndarray's NumPy-like surface fits
  that far better than candle's `Device`/`DType`/`Result` ceremony, which would
  make `tensor([[1,2],[3,4]]).sum()` needlessly heavy.
- **Ships now, pure Rust.** No system deps (BLAS is optional), fast to build,
  composes with the existing immutable `Rc`-shared value model.
- **The surface API is backend-independent.** `shape`/`reshape`/`matmul`/
  elementwise ops mean the same thing on CPU or GPU, so a later candle/GPU backend
  slots in behind them — exactly the Phase 4→6 split the roadmap already commits to.
- **Autodiff isn't needed yet.** It becomes load-bearing only for ML *training*
  (a later phase); that requirement — not general tensor math — is what may force
  candle/burn, and we'll adopt it *when* it does, not speculatively.

## Rejected alternatives

- **candle-core now** — autodiff/GPU are real, but the ceremony taxes every simple
  tensor op today, for a capability Phase 4 doesn't need. Adopt for Phase 6.
- **burn now** — a whole DL framework as the base numeric type is overkill and
  framework-coupling we don't want yet.
- **Hand-rolled tensor** — exactly the "don't reinvent vectorization/BLAS" mistake
  we avoided for DataFrames; rejected.
- **Reusing Polars/Arrow for tensors** — columnar ≠ dense n-dim numeric; wrong
  data model.

## Consequences

- Adds the `ndarray` dependency.
- `Value` gains a `Tensor` variant; `eval_binary` broadcasting and the math
  stdlib (`broadcast_unary`) extend to tensors.
- A future GPU backend must preserve identical surface semantics (a CPU and GPU
  `matmul` must agree to numerical tolerance).
- `f64`-only for now; mixed dtypes (`f32`, int tensors) are a later addition.

## Open questions

- When ML training lands, do we add candle as a *second* backend (CPU stays
  ndarray, GPU/autodiff via candle) or migrate wholesale? Surface stability is
  what buys us the option.
- Broadcasting/dtype-promotion rules between `Tensor` and `Array`/scalars — keep
  them identical to the arithmetic broadcasting already shipped for arrays.

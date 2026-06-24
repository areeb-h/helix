# ADR 0007 — Tensor backend

- **Status:** Proposed
- **Date:** 2026-06-21
- **Deciders:** Areeb + Claude

## Context

Phase 4 adds a native `Tensor` type for dense n-dimensional numeric arrays —
the substrate for linear algebra, scientific computing, and (later) ML. As with the
DataFrame decision (ADR 0003), the principle is **do not reinvent**: select a mature
engine for the difficult parts (n-dim storage, broadcasting, BLAS, later GPU/autodiff)
and maintain a clean Helix surface API on top.

The roadmap is explicit: **Phase 4 = tensors (CPU), Phase 6 = GPU**, with ML in
between. The tensor surface must therefore remain stable across a CPU-now /
GPU-later split.

## Options

| Backend | What it gives | Cost |
|---|---|---|
| **ndarray** | Mature pure-Rust n-dim arrays, NumPy-like, optional BLAS, strong ergonomics | CPU only; no autodiff/GPU |
| **candle-core** | Tensors with CPU/CUDA/Metal + autodiff; built for ML/LLMs | Heavier; `Result`-everywhere plus `Device`/`DType` ceremony; friction for simple scientific use |
| **burn** | Full DL framework, backend-agnostic, autodiff | Framework-oriented; excessive for a language's base tensor type |

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

- **Ergonomics for the intended audience.** Most scientific tensor use is not NN
  training; it is array math and linear algebra. The ndarray NumPy-like surface fits
  that far better than candle's `Device`/`DType`/`Result` ceremony, which would
  make `tensor([[1,2],[3,4]]).sum()` unnecessarily heavy.
- **Ships now, pure Rust.** No system dependencies (BLAS is optional), fast to build,
  and composes with the existing immutable `Rc`-shared value model.
- **The surface API is backend-independent.** `shape`/`reshape`/`matmul`/
  elementwise ops mean the same thing on CPU or GPU, so a later candle/GPU backend
  fits behind them — precisely the Phase 4→6 split the roadmap already commits to.
- **Autodiff is not required yet.** It becomes essential only for ML *training*
  (a later phase); that requirement — not general tensor math — is what may force
  candle/burn, and it will be adopted *when* required, not speculatively.

## Rejected alternatives

- **candle-core now** — autodiff/GPU are valuable, but the ceremony taxes every simple
  tensor op today, for a capability Phase 4 does not need. Adopt for Phase 6.
- **burn now** — a full DL framework as the base numeric type is excessive and
  introduces framework coupling that is premature.
- **Hand-written tensor** — precisely the "do not reinvent vectorization/BLAS"
  mistake avoided for DataFrames; rejected.
- **Reusing Polars/Arrow for tensors** — columnar storage is not dense n-dim
  numeric storage; the wrong data model.

## Consequences

- Adds the `ndarray` dependency.
- `Value` gains a `Tensor` variant; `eval_binary` broadcasting and the math
  stdlib (`broadcast_unary`) extend to tensors.
- A future GPU backend must preserve identical surface semantics (a CPU and GPU
  `matmul` must agree to numerical tolerance).
- `f64`-only for now; mixed dtypes (`f32`, integer tensors) are a later addition.

## Open questions

- When ML training lands, whether to add candle as a *second* backend (CPU remains
  ndarray, GPU/autodiff via candle) or migrate wholesale. Surface stability
  preserves the option.
- Broadcasting and dtype-promotion rules between `Tensor` and `Array`/scalars —
  to remain identical to the arithmetic broadcasting already shipped for arrays.

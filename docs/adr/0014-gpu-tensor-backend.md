# ADR 0014 — GPU tensor backend (wgpu), seam-first

- **Status:** Accepted (decision made; Phase 1 — the seam — is the next implementation task,
  Phase 2 — the wgpu kernels — needs real GPU hardware to develop and verify).
- **Supersedes/extends:** ADR 0007 (the tensor surface), and mirrors ADR 0012 (the
  DataFrame backend seam) for tensors.

## Context

`Value::Tensor` is `Rc<ArrayD<f64>>` — a dense CPU `ndarray`. Every op lives in
`src/tensor.rs` (elementwise/broadcast, reductions, `matmul`/`dot`, `norm`, and hand-rolled
`det`/`inv`/`solve`). `matmul` uses ndarray's naive `.dot()` (no BLAS, by design — no system
dep). `tensor.rs`'s own doc already states the intent: *"the Helix surface is
backend-independent, so a GPU/autodiff backend can slot in behind it later."* But there is
no actual seam yet — ndarray is called directly.

Large **dense** linear algebra (matmul, elementwise maps, reductions) is the canonical
GPU-friendly workload, and it's the natural next axis after this session's JIT-kernel /
fusion perf work. We want a GPU tensor backend **without giving up what makes Helix
Helix**: a single self-contained binary, no system toolkit, cross-platform, reproducible.

## Decision

### API: wgpu (not CUDA, not a DL framework)

**wgpu** is the only option that preserves the self-contained, cross-platform identity: one
Rust codebase → Vulkan / Metal / DX12 / GL / WebGPU across NVIDIA + AMD + Intel + Apple,
with **no CUDA toolkit on the target** — just a GPU driver (universally present). External
validation: [Burn](https://burn.dev/blog/cross-platform-gpu-backend/), a serious Rust DL
framework, chose wgpu *first* for exactly this reason. The trade-off accepted: wgpu is
lower-level (we write WGSL compute shaders for our handful of ops) and async.

- **Rejected — CUDA (cudarc):** NVIDIA-only + a CUDA toolkit dependency on every target.
  Breaks cross-platform *and* the "scp the binary, no deps" property that is Helix's thesis.
  Better raw ML perf, wrong identity.
- **Rejected — adopt burn/candle wholesale:** would hand us GPU matmul (and, for burn,
  autodiff) "for free", but means taking on an entire opinionated DL framework — heavy
  binary, framework lock-in — to accelerate a handful of dense ops. We may *reference* their
  WGSL shaders, not depend on the frameworks.

### Seam-first: ship the backend seam before any GPU code

An abstraction built before its second implementation tends to be the wrong abstraction.
The DataFrame seam (ADR 0012) worked because Polars' shape was known; for tensors the
critical GPU design points (lazy buffers, transfer placement, async device handling) are
exactly what we learn while writing wgpu. So Phase 1 builds the **CPU-only seam** (fully
testable here, no GPU), and Phase 2 implements the wgpu backend against it on real hardware.

## Phase 1 — the `TensorVal` seam (CPU/ndarray) — the concrete blueprint

Mirror the DataFrame→Polars seam, but with an **enum** (the realistic backend set is CPU +
GPU, not an open set), matching how `ArrayData` is structured.

In `src/tensor.rs`:
```rust
pub type Tensor = ArrayD<f64>;          // the dense CPU representation (unchanged)

#[derive(Debug, Clone)]
pub enum TensorVal { Cpu(Tensor) /* , Gpu(GpuTensor) later */ }

impl TensorVal {
    /// Borrow the dense CPU array, downloading from a device backend if needed.
    /// Zero-cost (a borrow) for Cpu.
    pub fn dense(&self) -> Cow<'_, Tensor> { match self { Cpu(t) => Cow::Borrowed(t) } }
    pub fn shape(&self) -> Vec<usize> { /* tracked directly even on a device */ }
}
impl Display for TensorVal { /* write the dense array */ }
```
In `src/value.rs`:
- `Value::Tensor(Rc<crate::tensor::TensorVal>)` (was `Rc<ArrayD<f64>>`).
- a constructor `Value::tensor(t: ArrayD<f64>) -> Value` wrapping `TensorVal::Cpu`.
- Display/Debug arms already work (`TensorVal` has `shape()` + `Display`).

Rewire (~34 `Value::Tensor` sites across 8 files — `tensor.rs`, `value.rs`, `interp.rs`,
`interp/{ops,methods,builtins,access}.rs`, `python.rs`): construction sites use
`Value::tensor(arr)`; read sites bind `let arr = handle.dense();` then use `&arr` (Cow
derefs to `&ArrayD`). `tensor::method`/`index_first`/`slice_first` take `&Tensor` (the dense
array). **Compiler-guided + parity-verified** (the tensor tests in `interp/tests.rs` and the
parity oracle catch any divergence). Behaviour is byte-identical — this is pure plumbing.

This is the genuine prerequisite: once it lands, the GPU backend is *just another
`TensorVal` variant*.

## Phase 2 — the wgpu backend (on GPU hardware)

- `Cargo.toml`: `wgpu` behind a `gpu` feature (off by default, like `managed`/`python`), so
  the default binary stays lean and the air-gapped build is unaffected.
- `src/tensor/gpu.rs`: device/adapter init (async, picked once and cached), buffer
  upload/download, and WGSL compute shaders for the **GPU-worthwhile** ops:
  **matmul** (the prime target — O(n³), tiled/workgroup-shared-memory kernel),
  **elementwise** (add/sub/mul/div/pow over the broadcast result), and **reductions**
  (sum/mean/min/max). `TensorVal::Gpu` holds a device buffer + shape.
- **Lazy buffer residency (the key perf decision):** a chain like `a.matmul(b).map(...)`
  must keep the result *on the GPU* between ops — transferring CPU↔GPU every op is the #1
  GPU performance trap and usually makes the GPU *slower*. So GPU ops produce `TensorVal::Gpu`
  (data stays on device); a download to CPU happens only at a CPU-only op or at
  `print`/materialization. This is the tensor analogue of the DataFrame lazy plan.
- **CPU-only ops** (`det`/`inv`/`solve`/`reshape`/indexing) `dense()` (download) and run on
  ndarray — fine; they're not the GPU-bound workload.
- **Fallback:** no GPU adapter (or `gpu` feature off) → everything is `TensorVal::Cpu`,
  unchanged.

## Invariants & honest caveats

- **Parity holds across backends.** A GPU result must match the CPU result within
  floating-point tolerance; the differential oracle (tree-walker, always CPU) is the
  reference. (Note: GPU f32-vs-f64 and reduction-order differences are real — the backend
  uses f64 where the hardware supports it, and documents tolerances where it can't.)
- **GPU only wins above a size crossover.** Like the thread-parallelism finding: transfer +
  launch overhead means small/medium tensors are *faster on CPU*. The backend measures and
  routes by size (and is opt-in via the feature), rather than blindly offloading. Be honest
  in docs about where it pays (large dense matmul / big elementwise), not a blanket "GPU = fast".
- **Testability:** wgpu kernel *logic* can run in CI via a software adapter
  (lavapipe/SwiftShader) on Linux; *performance* needs real hardware. The seam (Phase 1) is
  fully testable with no GPU at all.

## Autodiff (a separate fork)

If `grad()` is wanted, `burn-tensor` would provide GPU **and** autodiff together — but as a
heavy framework dependency. The wgpu-direct path here keeps the runtime lean and treats
autodiff as a *separate* effort: a source-to-source AST transform on the pure-`i64`/`f64`
kernel expressions (the same eligible bodies the JIT compiles), compiled natively — see the
fusion plan's "deferred swings". Decided independently of this ADR.

## Consequences

- Helix gains real GPU reach for dense numeric/ML-lite math **without** sacrificing the
  single-binary, cross-platform, no-system-deps identity (GPU is an off-by-default feature).
- The tensor surface is finally backed by an actual seam, as ADR 0007 always intended —
  enabling not just GPU but any future backend.
- A clear, honest performance story (size-routed, measured), not a GPU-marketing claim.

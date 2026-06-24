# Research — CPython interop design (2026-06-24)

Cited research that grounds [ADR 0008 — CPython interop](../adr/0008-cpython-interop.md).
Four parallel research passes: peer languages (Mojo), best-in-class bridges
(Julia PyCall→PythonCall, pythonnet, Ruby PyCall), the zero-copy data layer
(Arrow C Data Interface, DLPack, NumPy buffer protocol), and the embedding +
GIL-future reality (PyO3, PEP 703/684/734, GraalPy). The point was to **learn
from the documented mistakes** rather than copy any one design.

This note records the findings verbatim-in-substance so the decisions in ADR 0008
remain traceable after memory of this session is gone.

---

## 1. Mojo (Modular) — the closest peer

A new language explicitly designed to interoperate with Python. What it does and
where it hurts:

- **API shape.** `Python.import_module("numpy")` returns a single `PythonObject`
  wrapper; attribute/method access goes through dunders (`__getattr__`,
  `__getitem__`). Whole-module import only (no `from x import y`); no top-level
  code, so imports live inside functions.
  [docs](https://docs.modular.com/mojo/manual/python/python-from-mojo/)
- **Object model.** One opaque `PythonObject`, register-passable, **no GC** —
  CPython refcounts bridged to Mojo's value lifecycle (copy → `Py_INCREF`,
  destroy → `Py_DECREF`).
  [stdlib](https://docs.modular.com/mojo/stdlib/python/python_object/PythonObject/)
- **Containment.** Most Mojo APIs do **not** accept `PythonObject`; you convert
  explicitly (`String(py=...)`, `Int(py=...)`). `PythonObject` dropped
  `Intable`/`Indexer` for `IntableRaising` — forcing explicit, fallible
  conversion over silent coercion. [changelog](https://docs.modular.com/mojo/changelog/)
- **Distribution — the headline footgun.** Mojo dynamically loads an *unmodified*
  system CPython; it does **not** bundle one. Compiled binaries are **not
  self-contained** and fail at a user's runtime with *"Unable to locate a suitable
  libpython, set `MOJO_PYTHON_LIBRARY`"*; `mojo build` doesn't bundle Python
  packages → "two dependency graphs."
  [#551](https://github.com/modular/modular/issues/551),
  [late-2025 review](https://medium.com/deep-engineering/deep-engineering-21-mojo-python-interop-in-late-2025-with-ivo-balbaert-76b654f9e806)
- **GIL.** Held and serializes cross-language calls; no documented free-threading
  story. **Zero-copy** NumPy/DLPack is community-workaround territory, not shipped.
  [forum](https://forum.modular.com/t/zero-copy-dlpack-interop/2834)

**Steal:** one opaque refcount-bridged handle; implicit-out / explicit-in
conversion; "cross the boundary once" idiom; fallible boundary ops; Arrow as the
data substrate (the thing Mojo hand-rolls). **Avoid:** non-self-contained binaries
(libpython-at-runtime failure); unsynced Python package graph; import-inside-every-
function ergonomics; an implicit GIL story; deferring zero-copy to "later."

## 2. Julia PyCall.jl → PythonCall.jl — the canonical "what a bridge got wrong"

The same community built a Python bridge (PyCall), learned what hurt, and rebuilt
it (PythonCall) with the **opposite default**. The official "Comparison to PyCall"
page is essentially a written confession.

- **The core mistake: aggressive auto-conversion.** PyCall auto-converted every
  Python result to a Julia type. PythonCall returns an **opaque `Py` wrapper** and
  converts only on explicit request (`pyconvert(T, x)`). Why auto-conversion was
  judged wrong:
  - **Type instability** — the result's Julia type depended on runtime values.
  - **Lossy + irreversible** — `convert(T, ::PyObject)` considered only the target
    `T`; you couldn't recover the original to convert differently.
  - **Destroys identity + mutability** — the killer example: if `obj.some_list`
    auto-converts to a Julia `Vector`, then `.append(3)` fails and mutations stop
    propagating to the underlying Python list.
    [comparison](https://juliapy.github.io/PythonCall.jl/v0.2/pycall/),
    [discourse](https://discourse.julialang.org/t/pythoncall-jl-style-regarding-type-conversion/117626)
- **The refined rule:** immutable → convert (copy); mutable → **wrap** (so identity
  + mutation survive). Dispatch conversion on **(target type T × runtime Python
  type)**, not on `T` alone.
  [conversion-to-julia](https://juliapy.github.io/PythonCall.jl/stable/conversion-to-julia/)
- **Visible boundary:** they deliberately use `pyconvert`, not Julia's `convert`,
  to signal "Python objects are fundamentally different things."
- **Two-GC lifetime footgun:** Julia's tracing GC only "sees" the small wrapper,
  not the large Python object behind it → Python memory balloons; needed
  `pydel!`/manual `GC.gc()`. **A deterministic refcount (Rust `Rc`/`Drop`)
  sidesteps this entirely.**
  [discourse-gc](https://discourse.julialang.org/t/pythoncall-jl-and-garbage-collection/136234)
- **GIL + finalizers** historically deadlocked (decref on arbitrary threads);
  fixed with deferred finalization that decrefs only when the GIL is safely held.
- **The honest cost:** explicit conversion is verbose (`pyconvert(Date, ...)` vs a
  bare field access) — the safety/ergonomics tension Helix must navigate.
- **pythonnet (.NET)** independently removed implicit `__float__`/collection
  conversion ([#1584](https://github.com/pythonnet/pythonnet/pull/1584)) because it
  broke overload resolution. **Ruby PyCall** took the opposite (auto-convert) bet —
  the same default PythonCall rejected.

## 3. Zero-copy data interchange — Helix's structural edge

Helix is Polars/Arrow-backed (DataFrames) and ndarray-backed (Tensors) — exactly
the two layouts the Python ecosystem standardized zero-copy protocols around.

- **Arrow C Data Interface** (`ArrowSchema`/`ArrowArray` + a `release` callback;
  consumer-allocated structs, producer-owned data). Enables zero-copy
  Polars↔pandas↔pyarrow. Exposed in Python via the **PyCapsule Interface**
  (`__arrow_c_schema__`/`__arrow_c_array__`/`__arrow_c_stream__`; capsule names
  `"arrow_schema"`/`"arrow_array"`/`"arrow_array_stream"`). Polars implements it
  since v1.3. **Crucially, Arrow's validity bitmap *is* Helix's `missing` mask —
  round-trips with no translation.**
  [CDataInterface](https://arrow.apache.org/docs/format/CDataInterface.html),
  [PyCapsule](https://arrow.apache.org/docs/format/CDataInterface/PyCapsuleInterface.html)
- **DLPack** (`DLManagedTensor` + `deleter`) — the tensor exchange protocol for
  PyTorch/NumPy/JAX/CuPy. A dense f64 ndarray maps directly. **No null support**
  (route `missing`-bearing data through Arrow). The consume-once `"dltensor"` →
  `"used_dltensor"` rename is a real shipped leak (PyTorch
  [#117273](https://github.com/pytorch/pytorch/issues/117273)).
  [DLPack spec](https://dmlc.github.io/dlpack/latest/python_spec.html)
- **NumPy buffer protocol / `__array_interface__`** — simplest strided-array
  fallback. [numpy](https://numpy.org/doc/stable/reference/arrays.interface.html)
- **Rust crates already exist:** arrow-rs `arrow::ffi` (`to_ffi`/`from_ffi`),
  **`pyo3-polars`** (`PyDataFrame`/`PySeries`), **`pyo3-arrow`**, **`rust-numpy`**
  (zero-copy ndarray↔NumPy with a runtime borrow-flag system). So the zero-copy
  phase is mostly wiring existing crates + implementing the dunder protocols.
- **Footguns:** dangling buffers if one side frees (pin lifetime via an `Arc` held
  by the release callback; never realloc an exported buffer); chunked Series →
  use the stream interface (avoid a rechunk copy); strides/offset confusion (Arrow
  `offset` vs NumPy byte-strides vs DLPack element-strides); GIL inside
  release/deleter callbacks; export **immutable snapshots** (cheap with Arc-shared
  buffers) since there's no borrow checker across FFI.

## 4. Embedding CPython + the GIL future (PyO3, PEP 703/684/734, GraalPy)

- **PyO3 embedding:** `Python::with_gil`/`attach` + the modern `Bound<'py,T>` API
  (never the legacy GIL-Refs). `auto-initialize` lazily starts the interpreter
  (disabled under static linking). `PyErr` → `get_type().name()` + `value().str()`
  + `traceback().format()` for first-class diagnostics.
  [pyo3.rs](https://pyo3.rs/main/python-from-rust.html)
- **Distribution:** embedding needs libpython + a stdlib at runtime. The
  self-contained answer is **python-build-standalone + PyOxidizer/`pyembed`**
  (`$ORIGIN`-relative `sys.path`). **abi3 caveat:** for an *embedder bundling a
  fixed Python*, abi3 buys little and costs API surface; reserve abi3 for a
  "run against the user's arbitrary libpython" mode.
  [building](https://pyo3.rs/main/building-and-distribution)
- **GIL future:** free-threaded CPython (PEP 703) is officially *supported* in 3.14
  (not default). Design as if the GIL is **not** a mutex: `Bound`/`attach`,
  `detach()` around blocking work, `pyo3::sync` primitives. Per-interpreter GIL /
  sub-interpreters (PEP 684/734) are blocked today by C-extension unsafety (NumPy
  et al.) — use process isolation for parallelism for now.
  [PEP 703](https://peps.python.org/pep-0703/),
  [PEP 684](https://peps.python.org/pep-0684/)
- **GraalPy/Truffle (novel):** runs Python on a polyglot VM with zero-copy object
  sharing via a structural interop protocol. Not transferable wholesale (whole-VM
  bet, weak C-extension story), but the *ideas* — a single lazy `foreign` handle +
  structural ("behaves as array/iterable?") conversion instead of eager deep-copy —
  are. [GraalPy](https://docs.oracle.com/en/graalvm/jdk/22/docs/reference-manual/python/Interoperability/)
- **In-process vs out-of-process:** in-process (PyO3) gives low latency + cheap
  data sharing but a C-level segfault takes the host down; out-of-process isolates
  crashes at a serialization cost. In-process is right for the zero-copy data goal;
  process isolation is the escape hatch.

---

## What this research changed in the plan

1. **Conversion default flipped** to opaque-by-default (was "auto-copy lists" — the
   documented anti-pattern). Scalars convert; containers/objects stay opaque;
   explicit `to_array` for native copies.
2. **Distribution committed** to bundling a relocatable CPython as the end-state;
   the v1 spike uses ambient Python but fails loudly, never cryptically.
3. **Lifetime** is refcount↔`Drop` (Rust `Rc` sidesteps PythonCall's GC leak).
4. **GIL-future-correct** embedding idiom from day one.
5. **Arrow/DLPack zero-copy** elevated to the explicit differentiator for the next
   phase, with the specific footguns catalogued.

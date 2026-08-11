# ADR 0008 — CPython interop (Helix → Python)

- **Status:** Implemented (v1) (`src/python.rs`, behind the `python` feature)
- **Date:** 2026-06-24
- **Deciders:** Areeb + Claude
- **Research:** [2026-06-24 Python interop](../research/2026-06-24-python-interop.md)

## Context

Helix is a capable compiler with **no scientific ecosystem** — no NumPy, SciPy,
Biopython, pysam, pandas, scikit-learn, or PyTorch. Rebuilding all of that natively is
unrealistic, so adoption is all-or-nothing. **CPython interop is the adoption
unlock**: a scientist writes most of a pipeline in native Helix and calls Python
only where a library is required, so Helix need only be *better at its chosen core
workloads* rather than reimplement the existing ecosystem first. It does **not** make
Helix a better language; it makes Helix *usable before its own ecosystem exists*.

This ADR records the v1 bridge (Helix → Python: import a module, access attributes,
call functions, convert values, translate errors) and the decisions behind it,
each weighed against the documented mistakes of prior attempts (see the research
note). The guiding frame, consistent with Helix's philosophy, is **an escape hatch
around a strong native core, not Python with different syntax.**

## Decisions

### D1 — Feature-gated, off by default (`--features python`)

Embedding CPython requires linking libpython, so a Python-enabled Helix is **not** a
self-contained binary. To protect Helix's "fast, safe, self-contained" value
proposition, all interop lives behind an **off-by-default Cargo feature**
(`python`); `pyo3` is an optional dependency. The default `helix` never links
libpython. Without the feature, the `python` value still exists and `python.import`
still parses, but calling it returns a clear *"Helix was built without Python
support — rebuild with `--features python`"* error.

*Rejected:* always-on interop (imposes a libpython dependency on every user);
making interop a separate binary/crate (fragments the language).

### D2 — One opaque `Value` variant; all pyo3 contact isolated

A single new runtime value: `Value::PyObject(Rc<crate::python::PyHandle>)`.
`PyHandle` is defined in `src/python.rs`; its payload (`Py<PyAny>`, or a
`Namespace` marker for the `python` entry point) is feature-gated, so `value.rs`
and the engines **never reference pyo3**. This follows GraalPy's "single lazy
`foreign` handle" approach and Mojo's "one `PythonObject`": keep Python values
opaque and live, and do not eagerly model them in Helix's type system.

*Rejected:* multiple Python-typed variants (more ripple through every exhaustive
`Value` match); leaking `pyo3` types into `value.rs` (would force the entire crate to
depend on pyo3).

### D3 — Opaque-by-default conversion (the essential decision)

This is the single most important decision, and the one the research most sharply
corrected. The **PyCall.jl → PythonCall.jl** rewrite is the canonical lesson:
auto-converting Python results into native values is **type-unstable, lossy, and
destroys identity and mutability** (a Python `list` copied into a native array loses
`.append` and stops propagating mutations). pythonnet removed implicit conversion
for the same reason.

**Helix rule:**
- **Helix → Python** (implicit, lossless — values cross *out*): `Int`→`int`,
  `Float`→`float`, `Bool`→`bool`, `Str`→`str`, `Missing`→`None`, `Array`→`list`.
- **Python → Helix** auto-converts **only immutable scalars**: `bool`→`Bool`
  (**checked before `int`** — Python `bool` subclasses `int`), `int`→`Int`,
  `float`→`Float`, `str`→`Str`, `None`→`Missing`.
- **Everything else** (list, dict, tuple, ndarray, arbitrary objects) stays an
  **opaque `PyObject`**, preserving identity + mutability.
- **`to_array(x)`** is the explicit, visibly-named, on-demand materialization into
  a native Helix `Array` — the escape hatch when the user genuinely wants a copy.

*Rejected:* eager conversion of containers (the documented PyCall anti-pattern —
loses identity/mutability, type-unstable); fully-explicit conversion of *everything*
including scalars (PythonCall's strict default, which users found heavy:
`print(m.sqrt(16))` should not require a conversion call).

### D4 — Two surface syntaxes, one runtime path

Both requested forms are supported, and they converge:
- `import python.math as m` — the statement form (reuses the dotted-path plus `as`
  module syntax shipped immediately before this).
- `m = python.import("math")` — the expression form (dynamic module names).

The module loader **lowers** `import python.a.b as alias` into
`alias = python.import("a.b")` before the rest of the pipeline runs, so there is one
runtime path and the statement form is purely syntactic sugar.

*Rejected:* only the function form (less ergonomic for the common case); two
independent implementations (divergence risk).

### D5 — Keywords allowed as member names after `.`

`import` is a keyword, so `python.import("numpy")` would not parse (the `.import`
collides with the keyword). The postfix `.member` parser now accepts any reserved
word as a member name (`member_name()` beside `ident_name()`). This is a common,
safe rule — after `.`, a keyword can only be a member — and it incidentally enables
`x.in`, `obj.type`, and similar.

*Rejected:* renaming the method (for example, `python.load`) to avoid the clash (the
research and the requirements both specified `python.import`); making `import`
contextual in the lexer (more invasive, and the member rule is generally useful).

### D6 — Lifetime: bridge CPython refcounts to Rust `Drop`

`PyHandle::Object` wraps `Py<PyAny>`. Cloning the enclosing `Rc` shares one strong
CPython reference; dropping the last `Rc` drops the `Py<PyAny>`, which decrefs under
the GIL (pyo3 handles the attach). Helix's **deterministic `Rc`** avoids
PythonCall's two-GC leak, in which a tracing GC could not observe large Python
objects behind small wrappers and allowed memory to grow unbounded.

### D7 — Errors render-and-abort (not catch) for v1

A Python exception becomes a Helix-native diagnostic — `python error: <ExcType>:
<message>` at the call-site `line:col`, via `PyErr::get_type().name()` plus
`value().str()`. Because Helix has **no `try`/`catch` yet**, v1 **aborts**;
recoverable Python errors await a Helix error-handling design (ADR 0004).

### D8 — Embedding idiom is free-threading-correct from the outset

The bridge uses `Python::with_gil`/`attach` plus the modern `Bound<'py,T>` API, and
is written to `detach()` around blocking work, so it is already correct for
free-threaded CPython (PEP 703, supported in 3.14), even though v1 is
single-threaded. `pyo3` features: `auto-initialize` plus `abi3-py38` (one build spans
Python 3.8+). The interpreter initializes lazily.

*Rejected:* the legacy GIL-Ref API (deprecated, not free-threading-safe); assuming
the GIL is a mutex (false under 3.14+).

### D9 — Distribution: spike on ambient Python, commit to bundling

A widely-reported failure mode in Python-adjacent toolchains is a runtime
*"can't locate libpython."*
Accordingly, the v1 spike links the **ambient** Python (the user `pip install`s
their own dependencies) but **fails loudly at startup**, never cryptically
mid-program. The committed end-state is to **bundle a relocatable CPython**
(python-build-standalone / PyOxidizer `pyembed`, `$ORIGIN`-relative `sys.path`) so
the feature build ships self-contained, with the Python environment pinned in
Helix's future lockfile.

*Rejected:* depend on a system Python and document it (Mojo's documented worst
failure mode); bundle from day one (significant packaging work before the first
working demonstration — deferred, not abandoned).

## How `Unknown` makes the type checker a no-op

Helix's permissive type system already provides the dynamic boundary interop
requires: `Unknown` is compatible with everything and propagates through every
method/field/index (it is how DataFrame columns are typed). `python` is seeded as
`Type::Unknown`, so `python.import("numpy").mean([...]).reshape(...)` type-checks
with **no checker changes** — Python values use the same escape hatch as
DataFrame columns, while native Helix code remains statically checked.

## Implementation map (what changed)

- **NEW `src/python.rs`** — `PyHandle` (namespace/object, refcount↔`Drop`),
  `method`/`getattr`/`to_array`, `to_py`/`from_py` (opaque-by-default),
  `PyErr`→`HelixError`; real body `#[cfg(feature = "python")]`, stubs otherwise.
- **`src/value.rs`** — `Value::PyObject` variant + `type_name`/`Display`
  (`<python list>`)/`Debug` arms.
- **`src/interp/methods.rs` + `src/interp/access.rs`** — one dispatch arm each;
  because the VM calls these shared helpers, **both engines are covered with no
  `vm.rs`/`bytecode.rs` changes.**
- **Seeds (three places):** `Interp::new` env, `bytecode.rs` compiler
  `globals`/`global_init` (so the VM resolves `python` via `LoadGlobal` like
  `pi`/`e`/`inf`), and `Checker::new` (`Type::Unknown`).
- **`to_array` builtin** — registered in *both* builtin lists
  (`interp.rs::BUILTIN_FNS` and `types.rs::BUILTIN_FNS` — they are separate),
  `call_builtin`, and `signatures.rs` (→ `Array(Unknown)`).
- **`src/module.rs`** — lowers `import python.*` to an assign before file resolution.
- **`src/parser.rs`** — `member_name()` allows keywords after `.`.
- **`Cargo.toml`** — `python` feature + optional `pyo3` (`auto-initialize`,
  `abi3-py38`). **`src/main.rs`** — `mod python;` (always compiled; body gated).

## Verification

- **Default build** (`cargo test`): 117 unit + 15 integration green, **no pyo3
  linked**, zero warnings; a test asserts `python.import("math")` errors with the
  rebuild hint.
- **Feature build** (`cargo test --features python`): 117 + 17 green, zero warnings.
  Both engines agree (VM ≡ `HELIX_NOVM`) on import + attribute + call; a Python list
  stays opaque until `to_array`; `ModuleNotFoundError`/`AttributeError` translate to
  Helix diagnostics. Verified on Linux/WSL with Python 3.12. `examples/python/
  interop.helix` demonstrates it.

## Consequences

- A Python-enabled Helix depends on a compatible libpython at runtime (until the
  bundling end-state lands). The default Helix is unaffected.
- Every exhaustive `match` on `Value` gains a `PyObject` arm.
- `to_array` is now a reserved builtin name.
- The opaque-by-default rule is a **user-visible contract**: Python containers are
  handles, not native collections, until explicitly converted. This must be
  documented prominently (see `docs/python-interop.md`).

## Open questions / roadmap

1. **Zero-copy scientific bridge.**
   - **DataFrame ↔ Python polars — shipped.** A Helix DataFrame crosses to/from
     Python's `polars` by sharing the Arrow buffers (via `pyo3-polars 0.27`, which
     unifies on polars 0.54 plus pyo3 0.28). `to_py` collects the LazyFrame and
     hands over a `PyDataFrame`; the `to_dataframe(x)` builtin extracts a
     `PyDataFrame` back into a Helix `DataFrame`. `missing` ↔ Arrow validity bitmap
     with no translation. Round-trip verified on both engines.
   - **Tensor ↔ Python NumPy — shipped.** A Helix Tensor crosses to/from NumPy
     `f64` arrays via `rust-numpy 0.28` (which unifies on pyo3 0.28 and ndarray
     `<=0.17`, here 0.17). `to_py` copies the ndarray into a NumPy array; the
     `to_tensor(x)` builtin copies a NumPy `f64` array into a Helix Tensor.
     Deliberately **copy-based, not buffer-sharing**: NumPy arrays are mutable and
     Helix tensors are immutable and `Rc`-shared, so aliasing would break the
     immutability guarantee — each side receives its own buffer. Round-trip verified
     on both engines.
   - **Pending:** genuinely buffer-sharing tensors via **DLPack** (GPU/large arrays
     where mutability permits); pandas/pyarrow via the Arrow C stream interface;
     chunked Series via the stream interface (avoiding a rechunk copy). Pitfalls to
     observe: the DLPack consume-once (`"used_dltensor"` rename) leak; null loss
     through DLPack/NumPy; export of immutable `Arc`-snapshots; the GIL in release
     callbacks. The DataFrame and Tensor bridges are capabilities Mojo handles
     poorly and that Helix's Polars/Arrow/ndarray backing makes natural.
2. **Bundled CPython** (python-build-standalone plus `pyembed`) → self-contained
   feature build; Python environment in the lockfile (ties into the package-manager
   work).
3. **Recoverable Python errors** — depends on Helix `try`/`catch` (ADR 0004).
4. **Python → Helix** — Helix as an installable CPython extension (`PyInit_*`,
   `METH_FASTCALL`), so Python users can call Helix for hot paths.
5. **Free-threading / sub-interpreters** — to be revisited once the bio-relevant
   native stack (NumPy and related libraries) is verifiably free-threaded and
   sub-interpreter-safe.

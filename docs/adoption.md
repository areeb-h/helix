# Adoption assessment: requirements for switching to Helix

A candid assessment, written against the current state of Helix (a fast
bytecode-VM and Cranelift-JIT scientific language with its own DataFrame engine,
tensors, a missing-data model, and a permissive static type checker). The objective
is not to discourage — the implementation is sound — but to state precisely the gap
between a well-designed language and a tool selected for production work.

## Summary verdict

Helix is a credible language implementation with **no package ecosystem and only a
v1 of Python interop**,
which in scientific computing means it is **not yet adoptable for production work**;
engine speed and syntax design do not change this. The path forward is not to compete
with Python directly, but to fully address one narrow niche, with Python interop as
the escape hatch.

## Why Python prevails, and why these strengths do not displace it

Python is a modest language with a dominant ecosystem. It persists because it is the
**control plane** for science, not because of its design:

- NumPy, pandas, Polars, PyTorch, JAX, scikit-learn, SciPy, Biopython, statsmodels.
- Jupyter — the notebook is the de facto IDE of science.
- Twenty years of Stack Overflow answers, a large hiring pool, `pip install anything`.

Helix's strengths — C/Go-parity scalar speed, low-symbol syntax, expression
orientation, no truthiness coercion, a single set of `where`/`select`/`map` verbs
across Array and DataFrame, a coherent `missing` model, DataFrames measured faster than
our own polars backend on every verb — are all **language-quality** advantages. None of them answers the primary
question a scientist asks: whether the tool can run the libraries their field already
depends on. At present the answer is no, so these qualities do not yet affect
adoption.

## Current limitations

Grounded in the current source, not aspirations:

1. **Python interop has a working v1 (the most significant gap, now addressed).**
   A feature-gated CPython bridge (`cargo build --features python`) embeds Python via
   PyO3: `import python.math as m` / `python.import("numpy")` loads a module,
   attribute access and method calls forward to Python, scalars convert back
   natively, containers/objects remain opaque until `to_array(...)`, and Python
   exceptions surface as Helix errors. Helix can therefore call real Python
   libraries. Since then the DataFrame↔polars crossing (zero-copy via Arrow) and the Tensor↔NumPy crossing (copying)
   have landed ([ROADMAP Phase 7](ROADMAP.md)); still absent: DLPack-level buffer
   sharing, a bundled
   interpreter (it uses the ambient Python), and the Python→Helix direction. The
   default build remains self-contained (no libpython) and prints a rebuild hint if
   `python` is used without the feature.
2. **A module system exists, but no packages.** `import name` loads a sibling
   `name.helix` and reaches its definitions as `name.member`; modules can also reside
   in subdirectories (`import lib.stats` → `lib/stats.helix`) and be aliased
   (`import lib.stats as st`), so a codebase can span files and folders (done).
   What remains absent is the *distribution* half — a package manager, a registry,
   versioned dependencies, reproducible environments — without which a third-party
   ecosystem cannot form.
3. **Error handling has a v1.** `try EXPR` evaluates `EXPR` and catches any runtime
   error, yielding a record `{ok, value, error}`, so failures are recoverable
   instead of aborting the program. Error recovery is **native in the VM** since
   v0.7.0 (`Op::TryBegin`/`TryOk`/`TryErr` and a handler unwind), so a `try` anywhere
   no longer demotes the program to the tree-walker — verified with `jit-explain`: a
   program containing one still has its kernels JIT-compiled. A surfaced `Result`/`?`
   form is not yet provided.
4. **A standard library of 161 builtins** (`helix doc builtins`) plus the
   Array/String/Dna/Tensor/DataFrame methods: IO (CSV/Parquet and the genomics
   formats), a math and statistics core (t-tests, regression), JSON, a hardened
   HTTP client and a native HTTP server, and a substantial string surface. Still
   absent: plotting, dates ([ADR 0030](adr/0030-time.md) remains Proposed), and
   regex.
5. **Tensors without autodiff or GPU.** An ndarray-backed tensor type exists, but
   without gradients or an accelerator, so it does not compete with PyTorch/JAX for
   the workloads tensors are intended to serve.
6. **No notebook support.** A line-at-a-time REPL exists; there is no Jupyter kernel,
   no inline plots, and none of the exploratory workflow that science relies on.
7. **Unstable by construction.** The version is `0.4.0`; the execution engine, type
   system, and semantics are still changing. Durable work cannot be built on a
   moving target, and there is no compatibility policy yet.

## Current strengths

Stated so the strengths are clear alongside the limitations:

- **Performance.** The JIT-compiled numeric/loop core reaches C/Go parity and
  substantially outperforms CPython; automatic memoization makes pure overlapping
  recursion effectively instant.
- **One coherent runtime.** A single engine (the tree-walker now serves only as a
  test oracle and REPL), no silent fallbacks, heap-framed recursion, fuzzer-verified.
- **A complete missing-data model.** One `missing` value, propagating through
  arithmetic and aggregation, distinct from float NaN, with a column validity bitmap;
  cleaner than pandas' combination of NaN, None, and NaT.
- **DataFrames that reuse the array verbs.** `where`/`select`/`sort`/`group` run on
  Helix's own engine, following the language's own scalar semantics rather than a
  library's; the same verbs apply to arrays, and to a SQL result. Less API surface than
  pandas.
- **A permissive type checker** that catches real mistakes (undefined names,
  incorrect arities, `5 + "x"`) before execution with clear messages, and does not
  interfere with dynamic/dataframe-shaped code (zero false positives).
- **SQL that returns a frame.** SQLite bundled and PostgreSQL spoken directly over the
  wire, so a query result continues into the same `where`/`select`/`group` verbs.
  Parameters are values rather than interpolated text, the session is read-only, and TLS
  is on by default in a way the server cannot override — which is a stronger default than
  `libpq` or Go's `pgx` ship with.

This is a strong *foundation*. It is not yet a *product*.

## The realistic focus area, and why the obvious ones are unsuitable

- **General ML — no.** PyTorch/JAX dominate it and require autodiff, GPU, and a
  decade of ecosystem. Helix cannot enter this race.
- **General data science — no.** pandas and Polars-from-Python already own tabular
  work. Helix's DataFrames are now its own engine rather than a wrapper — faster than
  our polars backend on every verb, and semantically the language's own — but a better
  engine is still not sufficient to displace existing notebooks and libraries, which is
  an ecosystem question rather than a speed one.
- **Computational biology / bioinformatics scripting — possibly.** This is the
  hypothesis worth examining. Bioinformatics consists largely of ad-hoc scripts over
  **sequences and tabular data**, where Python (Biopython) is slow and awkward and
  shell pipelines are fragile. A fast, statically-checked, missing-aware language with
  **first-class DNA, DataFrames, and tensors** could be a better tool for a defined
  slice: sequence QC, k-mer/motif work, variant and expression tables, reproducible
  CSV/Parquet transforms. The caveat: even here, the obstacle is interop (pysam,
  samtools, Bioconductor) and entrenchment.

The narrowest accurate framing of the focus area: **a fast, typed, reproducible
scripting language for CPU-bound tabular and sequence pipelines that are currently
difficult in Python and do not require the ML stack.** Address that, then expand.

## The viability requirements (in order)

1. **Modules, a package manager, and reproducible environments.** Prerequisites; no
   ecosystem without them. (Modules — `import` and namespacing — now exist; the
   package manager, registry, and lockfiles remain.)
2. **Python interop (calling CPython).** The highest-leverage single feature. With
   it, "Helix cannot do X" becomes "use Python for X", so adoption is no longer
   all-or-nothing. Without it, adoption in science is nearly impossible.
3. **Depth on ONE focus area's five libraries**, rather than breadth across fifty
   shallow ones.
4. **A Jupyter kernel** — to support science in its existing environment.
5. **Freeze semantics and publish a compatibility policy.** A small stable language
   is preferable to a large moving one.
6. **One reproducible benchmark on a real workflow** — for example, a Parquet →
   group → aggregate pipeline that is 10× faster and a third of the code relative to
   pandas, kept green in CI.

## Conclusion

Helix today is a sound concept with a credible, well-engineered implementation —
approximately an 8/10 as a language experiment and a 3/10 as a foundation for a lab's
production work, and the entire gap is **product, not code**: interop, packaging, a
focus area, and stability. The codebase is no longer the constraint. The next
investment is not a further optimization or a cleaner abstraction; it is the decision
of whether to incur the multi-quarter product cost to make Helix *adoptable* for one
narrow niche — and if so, to build interop and packaging first, before any further
language features.

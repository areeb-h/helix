# Adoption assessment: requirements for switching to Helix

A candid assessment, written against the current state of Helix (a fast
bytecode-VM and Cranelift-JIT scientific language with Polars-backed DataFrames,
tensors, a missing-data model, and a permissive static type checker). The objective
is not to discourage — the implementation is sound — but to state precisely the gap
between a well-designed language and a tool selected for production work.

## Summary verdict

Helix is a credible language implementation with **no ecosystem and no interop**,
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
across Array and DataFrame, a coherent `missing` model, Polars DataFrames at 8–11×
pandas — are all **language-quality** advantages. None of them answers the primary
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
   libraries. This remains a deliberately scoped v1: no zero-copy DataFrame/Tensor
   bridge (Arrow C Data Interface / DLPack, the differentiator) yet, no bundled
   interpreter (it uses the ambient Python), and no Python→Helix direction. The
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
   instead of aborting the program. Programs that use `try` currently run on the
   tree-walker (the bytecode VM does not yet implement exception handling). A
   surfaced `Result`/`?` form is not yet provided.
4. **An approximately 40-function standard library.** `print`, `range`,
   `read_csv`/`parquet`/`fasta`, `write_parquet`, tensor constructors, and a math
   library, plus the Array/String/Dna/Tensor/DataFrame methods. No plotting, no
   statistics beyond array aggregates, no dates, no JSON, no HTTP, no regex, no
   comprehensive string library.
5. **Tensors without autodiff or GPU.** An ndarray-backed tensor type exists, but
   without gradients or an accelerator, so it does not compete with PyTorch/JAX for
   the workloads tensors are intended to serve.
6. **No notebook support.** A line-at-a-time REPL exists; there is no Jupyter kernel,
   no inline plots, and none of the exploratory workflow that science relies on.
7. **Unstable by construction.** The version is `0.1.0`; the execution engine, type
   system, and semantics are still changing (the current cycle replaced the runtime
   model). Durable work cannot be built on a moving target, and there is no
   compatibility policy yet.

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
- **DataFrames that reuse the array verbs.** `where`/`select`/`sort`/`group` lower to
  lazy Polars plans; the same verbs apply to arrays. Less API surface than pandas.
- **A permissive type checker** that catches real mistakes (undefined names,
  incorrect arities, `5 + "x"`) before execution with clear messages, and does not
  interfere with dynamic/dataframe-shaped code (zero false positives).

This is a strong *foundation*. It is not yet a *product*.

## The realistic focus area, and why the obvious ones are unsuitable

- **General ML — no.** PyTorch/JAX dominate it and require autodiff, GPU, and a
  decade of ecosystem. Helix cannot enter this race.
- **General data science — no.** pandas and Polars-from-Python already own tabular
  work. Helix's DataFrames are Polars with improved syntax — a genuine improvement,
  but not sufficient to displace existing notebooks and libraries.
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

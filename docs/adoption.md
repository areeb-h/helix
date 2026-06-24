# Would anyone switch to Helix — and what would it take?

A deliberately blunt assessment, written against what Helix actually is today (a
fast bytecode-VM + Cranelift-JIT scientific language with Polars-backed DataFrames,
tensors, a missing-data model, and a permissive static type checker). The point is
not to be discouraging — the implementation is genuinely good — but to be honest
about the gap between "good language" and "tool people choose for work."

## One-line verdict

Helix is a credible language implementation with **no ecosystem and no interop**,
which in science means it is **not yet adoptable for real work** — and no amount of
engine speed or syntax elegance changes that. The path forward is not "compete with
Python"; it is "own one narrow niche completely, with Python interop as the escape
hatch."

## Why Python wins (and why our strengths don't dent it)

Python is a mediocre language with an unbeatable ecosystem. It survives because it
is the **control plane** for science, not because it is well-designed:

- NumPy, pandas, Polars, PyTorch, JAX, scikit-learn, SciPy, Biopython, statsmodels.
- Jupyter — the notebook is the actual IDE of science.
- Twenty years of Stack Overflow answers, a vast hiring pool, `pip install anything`.

Helix's real strengths — C/Go-parity scalar speed, clean low-symbol syntax,
expression orientation, no truthiness, one set of `where`/`select`/`map` verbs across
Array/DataFrame, a coherent `missing` model, Polars DataFrames at 8–11× pandas — are
all **language-quality** wins. None of them answers the only question a scientist
actually asks: *"can it run the libraries my field already depends on?"* Today the
answer is no, so the quality is moot for adoption.

## Where Helix concretely fails today

Grounded in the current source, not aspirations:

1. **No Python interop / FFI.** Can't call NumPy, PyTorch, pysam, samtools, or any
   existing library. For ML this is dead on arrival; for bio it rules out the de
   facto toolchain. *This is the single most important gap.*
2. **No module system, no packages, no imports.** Programs are a single file. There
   is no way to split a codebase, share code, or build a third-party ecosystem — so
   an ecosystem cannot even begin to exist.
3. **No user-facing error handling.** No `try`/`catch`, no surfaced `Result`/`?` in
   the language — a runtime error aborts the program. Fine for scripts, unworkable
   for anything someone builds a system on.
4. **A ~40-function standard library.** `print`, `range`, `read_csv`/`parquet`/
   `fasta`, `write_parquet`, tensor constructors, and a math library — plus the
   Array/String/Dna/Tensor/DataFrame methods. No plotting, no statistics beyond
   array aggregates, no dates, no JSON, no HTTP, no regex, no real string library.
5. **Tensors without autodiff or GPU.** There is an ndarray-backed tensor type, but
   no gradients and no accelerator — so it does not compete with PyTorch/JAX for the
   workloads tensors exist to serve.
6. **No notebook story.** A line-at-a-time REPL exists; there is no Jupyter kernel,
   no inline plots, none of the exploratory loop science runs on.
7. **Unstable by construction.** It is `0.1.0`; the execution engine, type system,
   and semantics are still moving (this very cycle replaced the runtime model). No
   one can build durable work on a moving target, and there is no compatibility
   policy yet.

## What Helix is genuinely good at right now

So the bar is clear, not so the strengths get lost:

- **Fast.** The JIT'd numeric/loop core hits C/Go parity and crushes CPython;
  automatic memoization makes pure overlapping recursion instant.
- **One coherent runtime.** A single engine (the tree-walker is now only a test
  oracle / REPL), no silent fallbacks, heap-framed recursion, fuzzer-verified.
- **A real missing-data model.** One `missing` value, propagating through arithmetic
  and aggregation, distinct from float NaN, with a column validity bitmap — cleaner
  than pandas' NaN/None/NaT mess.
- **DataFrames that reuse the array verbs.** `where`/`select`/`sort`/`group` lower to
  lazy Polars plans; the same verbs work on arrays. Less API sprawl than pandas.
- **A permissive type checker** that catches real mistakes (undefined names, bad
  arities, `5 + "x"`) before execution with good messages, and stays out of the way
  on dynamic/dataframe-shaped code (zero false positives).

This is a strong *foundation*. It is not yet a *product*.

## The realistic wedge — and why the obvious ones are traps

- **General ML — no.** PyTorch/JAX own it and require autodiff + GPU + a decade of
  ecosystem. Helix cannot enter this race.
- **General data science — no.** pandas and Polars-from-Python already own tabular
  work. Helix's DataFrames are *Polars with nicer syntax* — a real improvement, but
  nowhere near enough to make anyone abandon their notebooks and libraries.
- **Computational biology / bioinformatics scripting — maybe.** This is the bet
  worth examining. Bioinformatics is a sea of ad-hoc scripts over **sequences +
  tabular data**, where Python (Biopython) is slow and awkward and shell pipelines
  are fragile. A fast, statically-checked, missing-aware language with **first-class
  DNA + DataFrames + tensors** could be a genuinely nicer tool for a defined slice:
  sequence QC, k-mer/motif work, variant and expression tables, reproducible CSV/
  Parquet transforms. The honest caveat: even here, the wall is interop (pysam,
  samtools, Bioconductor) and entrenchment.

The narrowest honest framing of the wedge: **the fast, typed, reproducible scripting
language for CPU-bound tabular + sequence pipelines that are currently painful in
Python and don't need the ML stack.** Win that, then widen.

## The bar to viability (in order)

1. **Modules + a package manager + reproducible environments.** Table stakes; no
   ecosystem without them.
2. **Python interop (call CPython).** The highest-leverage single feature. With it,
   "Helix can't do X" becomes "drop into Python for X" — adoption stops being all-or-
   nothing. Without it, adoption in science is ~impossible.
3. **Go deep on ONE wedge's five libraries**, not wide on fifty shallow ones.
4. **A Jupyter kernel** — meet science where it already works.
5. **Freeze semantics + publish a compatibility policy.** A small stable language
   beats a large moving one.
6. **One undeniable, reproducible benchmark on a real workflow** — e.g. "this
   Parquet → group → aggregate pipeline is 10× faster and a third of the code vs
   pandas," kept green in CI.

## Bottom line

Helix today is a *good idea with a credible, now well-engineered implementation* —
roughly an 8/10 as a language experiment and a 3/10 as something you could build a
lab's work on, and the entire gap is **product, not code**: interop, packaging, a
wedge, and stability. The codebase is no longer the constraint. The next real
investment is not another optimization or a cleaner abstraction; it is deciding
whether to pay the multi-quarter product cost to make Helix *adoptable* for one
narrow niche — and if so, building interop and packaging first, before any more
language features.

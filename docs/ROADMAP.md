# Helix Roadmap

Objective: provide a scientific programming language purpose-built for research
workflows.

**Positioning:** Helix is not a general-purpose Python replacement, but a scientific
computing language for the post-Pandas era, with **computational biology as the
flagship domain** (analogous to R's role in statistics). The tabular, statistical,
and array domains (data science, finance, climate, astronomy) follow with minimal
additional effort from the Polars/array core; computational biology is the
differentiated focus area. See positioning notes in memory and below.

## Flagship — Computational biology
Parsing is delegated to the Rust bio ecosystem (`needletail`, `noodles`, `rust-bio`)
in the same manner that DataFrames rely on Polars; Helix provides the consistent,
fast, memory-safe surface.
- [x] **`bio.read_fasta(path)`** → array of `{id, seq, length}` records via
      `needletail` (FASTA/FASTQ, gzip-aware). `seq` is a `Dna` (ambiguous bases
      like `N` preserved). Demo: [examples/genomics.helix](../examples/genomics.helix).
- [x] **Sequence ops**: `gc_content`, `complement`, `reverse_complement`,
      `kmers(k)`, `find(motif)` (→ index or `missing`), slicing; plus `Array.top(n)`
      (frequency histogram) so `seq.kmers(9).top(20)` works.
- [x] **`bio.read_vcf(path)` / `bio.read_bcf(path)` → DataFrame**: variant tables flow
      directly into the existing `where`/`group`/`count` verbs, demonstrating the unified model
      (`bio.read_vcf(...).where(@gene == "BRCA1").group(@consequence).count(@pos)`). The eight
      fixed columns plus every INFO field (`gene`, `consequence`, …) become columns, each
      **header-typed** (an `Integer`/`Float`/`Flag` INFO field becomes a numeric/bool column,
      so `where(@af > 0.001)` is a numeric comparison). Parsing delegates to `noodles`; plain
      `.vcf`, gzipped/BGZF `.vcf.gz`, and binary **BCF** all share one record model and
      column-building core. Demo: [examples/variants.helix](../examples/variants.helix). (No-arg
      grouped `count()` for rows-per-group is a small follow-up.)
- [x] **`read_fastq`** — FASTQ reads as records `{id, seq, qual, length}` (via
      `needletail`); `seq` is a DNA value and `qual` the Phred string. Demo:
      [examples/sequencing.helix](../examples/sequencing.helix).
- [x] **`read_gff` / `read_bed`** — feature/interval tables as DataFrames (GFF3 via
      `noodles-gff` with one column per attribute tag; BED hand-rolled for BED3/6/12).
- [x] **`read_sam` / `read_bam`** — sequence alignments as a DataFrame: the eleven
      mandatory SAM fields (`name`, `flag`, `ref`, `pos`, `mapq`, `cigar`, `rnext`,
      `pnext`, `tlen`, `seq`, `qual`) become columns (`ref`/`rnext` resolved to names
      from the header, CIGAR rendered to its SAM string). BAM is the binary, BGZF-framed
      form; both share one record model and column-building core via `noodles-sam`/
      `noodles-bam`. Demo: [examples/alignments.helix](../examples/alignments.helix).
- [ ] CRAM via `noodles-cram` (reference-based compression); tabix/CSI-indexed region
      queries (read `chr17:43k-44k` without a full scan); FASTQ quality decoding.
- [ ] RNA (`fold`, `translate`), protein sequences; an ADR for the bio type model.
- [~] Python interop for adoption (calling into Biopython and existing pipelines).
      **v1 complete** (`import python.pysam`, etc.); see
      [Phase 7](#phase-7--adoption--ecosystem). Zero-copy BAM/array sharing pending.

## Phase 1 — Core interpreter (Done; current)

Lexer → parser → AST → tree-walking interpreter.

- [x] Significant-newline lexer with dot-chain continuation
- [x] Pratt/precedence-climbing expression parser
- [x] Immutable-by-default bindings, explicit `mut`
- [x] Int/Float/String/Bool/Array/Dna values (Rc-shared)
- [x] Arithmetic, comparison, word-based boolean logic
- [x] Method dispatch (Array stats, String, DNA bio ops)
- [x] Built-ins: `print`, `range`, `dna`
- [x] Friendly errors: caret + hint + "did you mean?"
- [x] File runner + REPL

### Syntax (drawn from Python and TypeScript)
- [x] **String interpolation** `"{expr}"` (always-on, `{{`/`}}` escapes;
      embedded expressions type-checked).
- [x] **`??` null-coalescing** for `missing` (lowers to Polars `fill_null` in
      DataFrame predicates).
- [x] **Records** `{name: "Ada", age: 41}` with `.field` access (`.method()` keeps
      parens). Structurally typed; field typos are caught at compile time. Nested,
      arrays-of-records, function-returning-record. Trailing commas allowed.
- [x] **Slicing** `xs[1:3]`, `xs[:n]`, `xs[::2]`, `xs[::-1]` (full Python
      semantics; negative indices/step; arrays, strings, DNA). Type-checked
      (preserves the collection type; a non-integer bound is a compile error).
- [x] Tensor **first-axis** indexing and slicing (`t[i]` row/scalar, `t[1:3]`
      sub-tensor, `t[i][j]` scalar, `t[::-1]`).
- [ ] Tensor **multi-axis** subscript `t[i, j]`, `t[1:3, :]`; `xs[i] = v` assignment.
- [ ] String-keyed / dynamic dicts `{"col": v}` with `r["key"]` access.
- [x] **Tuples and destructuring** `(a, b)`, `a, b = pair`, `mut a, b = …`, tuple
      indexing; `zip`/`enumerate` yield tuples. Destructure arity type-checked.
- [x] **Lambda-param destructuring** `pairs.map((a, b) => a + b)` (over tuples
      from `zip`/`enumerate`, or any tuple/array element). Type-checked.
- [x] **Optional chaining unneeded** — `.` is already missing-safe (propagates
      through field and method access), so `user.name ?? "anon"` needs no `?.`.
- [ ] String-keyed / dynamic dicts `{"col": v}` with `r["key"]` access.

### Local bindings and blocks
- [x] **`let a = x, b = y in body`** — local bindings as expressions (sequential,
      scoped). Selected over indented blocks because indentation collides
      with multi-line dot-chains (see [ADR-0004](adr/0004-functions-errors-mutability.md)).
- [ ] Multi-statement function bodies beyond `let` (only if a concrete need emerges).

### Next within Phase 1
- [x] Control flow — **decided: method/comprehension style.** `if cond then a
      else b` is an expression; iteration is `map`/`filter`/`where`/`reduce`
      with `it` (and `acc`) bound per element. No statement keywords, no braces,
      and `where` is the same verb DataFrames reuse.
- [x] Comprehension methods: `map`, `filter`, `where`, `reduce`.
- [x] Named element binders via `=>` (`grid.map(row => row.map(v => v + 1))`,
      `xs.reduce(0, (acc, x) => acc + x)`); `it` remains the default. See
      [ADR 0005](adr/0005-syntax-conventions.md).
- [x] Surface conventions finalized: `then` retained, `count` over `len`, parens always.
- [x] User-defined functions (`fn name(a, b) = expr`, recursion, first-class
      `=>` values). See [ADR 0004](adr/0004-functions-errors-mutability.md).
- [x] `missing` value (scalar part of [ADR 0001](adr/0001-missing-data.md)):
      Option-style absence, Julia-style propagation and three-valued logic,
      `.is_missing()`, `.drop_missing()`, propagating aggregations.
- [x] Elementwise broadcasting for arithmetic (`xs - xs.mean()`, `xs + ys`).
- [x] Additional array methods: `take`, `drop`, `zip`, `enumerate`, `any`, `all`.
- [~] Error handling: `try EXPR` yields `{ok, value, error}` and catches runtime
      errors (done; runs on the tree-walker). A `Result` + `?` form
      ([ADR 0004](adr/0004-functions-errors-mutability.md)) and VM support remain.
- [x] A test suite (~135 in-crate unit tests + ~42 CLI integration tests), including a
      seeded VM-vs-tree-walker differential oracle and a CLI check that every example
      runs identically on both engines.

## Phase 2/5 — Type system and tooling
- [x] **Static type checker** (`src/types.rs`) — bidirectional, localized
      inference (not global Hindley-Milner), **permissive** (errors only on
      provable mistakes; `Unknown` top type for dynamic positions; zero false
      positives). Runs after parsing, before interpretation. See
      [ADR-0002](adr/0002-type-system.md).
- [x] Optional **typed function signatures** `fn area(w: Int, h: Int) -> Int`.
- [ ] Bidirectional refinement: flow element/expected types into lambdas; tighten
      Array-arithmetic and matmul/dot return types beyond `Unknown`.
- [ ] Column-level DataFrame typing within a pipeline (post load-boundary), and an
      optional compile-time "schema pin".
- [ ] `Maybe`/`Int?` nullable tracking (deferred from ADR-0001 — checker uses a
      bottom `Missing` type for now).
- [x] **Module system + imports** — see [Phase 7](#phase-7--adoption--ecosystem).
- [ ] Package manager — see [Phase 7](#phase-7--adoption--ecosystem).

## Phase 3 — DataFrame engine (lazy core shipped)
- [x] Back DataFrames with **latest Polars (0.54)** on **Rust 1.96 / edition
      2024** — no pin, no workaround (1.96 stabilized the features 0.50+ needs).
- [x] **Lazy `LazyFrame` threading**: verbs extend the plan; it materializes
      once at `print`/`count`. A chain fuses into one multi-threaded pass and
      scales beyond RAM (5M-row filter+group+sort+head in ~1s, debug build).
- [x] SQL-style verbs: `patients.where(@age > 40).select(@name).sort(@age)`.
- [x] `group(@keys).mean(@col)` grouped aggregations (also sum/min/max/count/std).
- [x] Column references via the `@name` sigil (unambiguously a column, never a
      variable) inside `where`/`select`: predicate AST → Polars expression
      (`dataframe::to_polars`), so `where` is one verb across Array and DataFrame.
- [x] `read_csv` + **`read_parquet`** (memory-mapped scan), `head`/`count`
      (`len()` pushdown) / `columns` (schema only, no scan).
- [x] `write_parquet` (eager). **Benchmarked** — see [benchmarks.md](benchmarks.md):
      50M-row query ~0.2s (Parquet) / ~2.3s (CSV) warm; Parquet `count()` is O(1)
      from metadata; interpreter overhead negligible.
- [x] **`DataFrame.cache()`** — materialize a lazy frame once into memory and
      re-wrap as lazy, so later queries reuse it instead of re-scanning the source.
      Explicit and eager, with no stale state and no interior mutability; safe by
      immutability. See [caching-and-memory.md](caching-and-memory.md). (Automatic
      reuse-detection may follow later, but explicit is the default.)
- [x] **Streaming sink** for `write_parquet` (Polars `sink` API): 50M-row write
      1.52 GB peak (down from 4.76 GB eager, 3.1× less). Not bounded-constant yet;
      the CSV scan side still buffers.
- [x] **Cold-cache benchmark** (`posix_fadvise` eviction, `scripts/coldbench.sh`):
      cold CSV ~8× slower (disk-bound); cold Parquet remains fast.
- [x] **Helix vs Python-Polars** comparison (`scripts/compare.sh`): identical
      query engine; Helix adds no query overhead and is faster end-to-end (no import
      cost).
- [ ] Streaming engine toggle for true out-of-core reads on large files.
- [ ] `head`-preview on print so printing a large frame does not materialize it fully.
- [ ] **Cross-statement caching** — reusing a DataFrame binding re-scans the file.
- [ ] Additional IO: Arrow IPC, JSON, FASTA; `write_csv`.
- [ ] DataFrame `missing` as the Arrow validity bitmap (unified with ADR 0001).
- [x] Derived columns (`df.with({bmi: @weight / @height})`) and joins
      (`df.join(other, @key)`, inner/left/right/outer; keys validated eagerly).
- [ ] Formal benchmarks: variance/CI, against pandas/DuckDB; 100M+ rows.

## Phase 3.5 — Math & numerics core (shipped)
- [x] Math standard library (broadcasts + propagates `missing`): sqrt/exp/ln/
      log/trig/hyperbolic/floor/ceil/round/abs/sign/hypot/atan2/min/max.
- [x] `**` power operator (right-assoc, Int-preserving), constants pi/e/inf.
- [x] Interpreter perf: `rustc-hash` env, edition-2024, deps optimized in dev.
- [ ] Complex numbers as a first-class type (`2 + 3i`), if demand warrants.
- [ ] Parallel array combinators (rayon `map`/`filter`/`reduce`) for large arrays.

## Phase 3.6 — Statistics core (descriptive, bivariate, inferential shipped)
The "R-for-statistics" surface (`src/stats.rs`). Descriptive statistics are
missing-propagating and population-based so `var == std²` and array verbs agree with
the DataFrame group aggregations; inferential statistics use the sample (`n - 1`)
estimators they require.
- [x] Descriptive array methods: `median`, `var`, `quantile(p)` (type-7 linear
      interpolation), and `summary()` → a `{count, mean, std, min, median, max}`
      record (the `describe()` analogue), alongside the existing `mean`/`std`.
- [x] Bivariate: `stats.correlation(xs, ys)` (Pearson r; symmetric, undefined on a
      constant series).
- [x] DataFrame-column statistics: `df.column(name)` materializes a column as an
      array (Polars nulls → `missing`), so the array statistics and verbs apply to
      loaded data — e.g. `df.column("age").median()`, or `drop_missing()` first.
- [x] Special-functions layer: `erf`, log-gamma, and the regularized incomplete beta
      (Abramowitz & Stegun / Numerical Recipes), accurate to better than 1e-7.
- [x] Inferential: `stats.t_test(a, b)` — Welch's two-sample t-test → `{statistic, df,
      p_value}` — and the normal distribution functions `normal_cdf`/`normal_pdf`/`erf`
      (broadcasting math). Verified against R's reference values.
- [x] `stats.linear_regression(x, y)` — ordinary least-squares fit → `{slope, intercept,
      r_squared, slope_std_error, slope_p_value}` (slope inference on `n - 2` df).
      Predictions need no special method: broadcast `fit.slope * x + fit.intercept`.
- [x] `stats.multiple_regression(predictors, y)` — OLS on several predictors via the normal
      equations → `{coefficients, std_errors, p_values, r_squared, adj_r_squared}`
      (parameter-indexed arrays, intercept first). Rejects collinear predictors.
- [ ] More distributions: Student's-t / binomial / chi-squared pdf/cdf/quantile, and
      one-sample / paired t-tests, on the same special-functions layer.
- [ ] Whole-frame aggregation shorthands (`df.median(col)`, `df.stats.correlation(c1, c2)`)
      over the `column` accessor, if the explicit form proves verbose in practice.

## Phase 3.7 — Data access & APIs (shipped)
- [x] **JSON** — `json.parse(str)` (object→record, array→array, scalars, `null`→
      `missing`) and `json.stringify(value)`. Pure compute, always available. See
      [ADR 0010](adr/0010-networking-privacy-security.md).
- [x] **HTTP client** — `http.get(url)` → `{status, body}` for fetching REST APIs;
      body is typically fed to `parse_json`. Default-on (`http` feature;
      `--no-default-features` for a network-free binary). Demo:
      [examples/api/fetch.helix](../examples/api/fetch.helix).
- [x] **String-keyed record access `r["key"]`** — dynamic field access for JSON keys
      that aren't valid identifiers (e.g. `d["first-name"]`); an absent key is
      `missing` (the safe/optional accessor; `.field` stays the typo-catching one).
- [ ] `http_post`/headers/auth; reading CSV/Parquet/JSON straight from a URL.
- [ ] Serving APIs / gRPC / websockets stay out of the core — via Python interop.

## Phase 4 — Tensor engine (foundation shipped)
- [x] Native `Tensor` type (ndarray-backed, `f64`, dynamic rank) — see
      [ADR 0007](adr/0007-tensor-backend.md). `tensor(nested)`, `zeros`/`ones`/`eye`.
- [x] Elementwise arithmetic with **NumPy-style broadcasting** (`a + 10`,
      `a + tensor([10,20])`); math stdlib broadcasts over tensors.
- [x] `shape`/`ndim`/`reshape`/`transpose`; whole + **axis-wise** reductions
      (`sum`/`mean`/`min`/`max`, optional axis); `matmul`/`dot` (vec·vec,
      mat·mat, mat·vec).
- [x] **Linear algebra** — `det`, `inv`, `solve`, `norm` (pure-Rust Gaussian
      elimination; no BLAS/LAPACK system dependency).
- [ ] Slicing/indexing, stacking/concatenation, `arange`/`linspace`.
- [ ] Other dtypes (`f32`, int tensors); tensor and Array interop.
- [ ] BLAS-backed linalg for large matrices (optional feature) if performance requires it.
- [ ] Autodiff and GPU via candle/burn (Phase 6) behind the same surface API.
- [ ] `model.train(x, y)` / `model.predict(...)` ML surface.

## Phase 5 — Execution engine (performance)

The tree-walker is correct but approximately 100× slower than the delegated data
path for scalar and control-flow code (single-threaded, AST re-traversal,
`String`-hashed environment). The staged plan, each step independently valuable:

- [x] **Stage 1 — bytecode compiler and stack VM** (`src/bytecode.rs`, `src/vm.rs`).
      Compiles the scalar/function/recursion core to a flat instruction stream
      with **slot-resolved variables** (no per-access hashing) and a **heap call
      stack** (recursion bounded by memory rather than the native stack, which
      resolves the depth limit). Reuses the interpreter's value and
      arithmetic/boolean helpers, so it is observationally identical; any construct
      it cannot yet compile (arrays, methods/comprehensions, records, tensors,
      DataFrames, lambdas) falls back to the tree-walker per program. **~3× faster
      on `fib(30)` (debug); 100k-deep recursion runs on an ordinary stack.** Parity
      is regression-gated: VM-vs-tree-walker unit tests and an all-examples diff.
- [~] **Stage 1b — widen the VM to handle all constructs (the single-engine goal).**
      The end state: the bytecode VM is the *sole* executor; the tree-walker is
      removed; the JIT and memoization are optimization *tiers* on the one bytecode
      rather than parallel engines, which eliminates cross-engine divergence by
      construction. Completed so far: array literals, indexing, string interpolation
      (each reuses the tree-walker's own implementation, so parity is guaranteed).
      Remaining, in order:
      - tuples, records, field access, slicing, destructuring (mechanical, reusing
        existing implementations);
      - **value-methods** via a runtime-dispatched `Method` op (reusing `call_method`);
      - **comprehensions as bytecode *loops*** — `xs.map(it + 1)` compiles to a
        tight loop with `it` in a local slot and the body inlined: no closures, no
        per-element calls, and faster than the tree-walker. This is the key insight
        that makes the single-engine goal tractable;
      - DataFrame/GroupBy verbs (carrying the unevaluated predicate AST, reusing
        `eval_df_method` with a slot-aware variable resolver);
      - first-class lambdas (stored and passed as values) via closures, the final
        and rarest piece.
      Each step removes tree-walker fallback for more programs and is parity-tested.
- [ ] Stage 2 — **rayon-parallel** array combinators for large in-memory arrays.
- [x] **Stage 3 — Cranelift JIT, first iteration** (`src/jit.rs`). Compiles the
      integer-recursion core (`+`/`-`/`*`, `if` with comparison, `let`, calls to
      other eligible functions, ≤4 params) to **native machine code**; the VM calls
      it when all arguments are `Int`, and falls back to bytecode otherwise.
      Eligibility is a fixpoint (a function calling an ineligible function is
      ineligible). **fib(35): 0.04s — faster than Node/V8 (0.08s) and Python
      (0.69s), ~2× off Go, ~4× off C; 38× over the VM.** JIT≡VM verified
      (`jit_matches_vm` test). Monomorphization with a guard (specialized on `i64`).
      One contained `unsafe` (the native call).
- [x] Stage 3b — **`Float`/`f64` JIT specialization** (dual-spec: each function
      compiled for both `i64` and `f64`; the VM dispatches by argument type). `Div`
      is supported in the float path. Float recursion `fibf(35.0)` runs native:
      **0.05s vs 1.58s on the VM (32×)**, the same tier as integer code.
      Parity-tested.
- [ ] Stage 3c — widen further: `Mod`/`Pow`, `and`/`or` in conditions,
      forward-referenced mutual recursion (two-pass bytecode function registration),
      then array/loop kernels (the bridge to Track C).

## Phase 7 — Adoption and ecosystem

The viability requirements: the work that turns a capable compiler into a language
suitable for adoption. See [docs/adoption.md](adoption.md) for the gap analysis that
motivates this phase.

### Modules (complete)
- [x] **`import name`** loads a sibling `name.helix`; its top-level definitions are
      reached as `name.member`. A loader resolves the import graph (dependency
      order, deduplication by canonical path, cycle detection), then rewrites every
      module into one namespaced flat AST that the existing pipeline runs unchanged.
      Single-file programs are unaffected (unmodified errors); multi-file error
      messages strip the internal namespacing.
- [x] **Subdirectory / path imports** `import lib.stats` → `lib/stats.helix`
      (relative to the importer, nested arbitrarily deep).
- [x] **Aliases** `import lib.stats as st` (`as` is contextual and remains usable as
      an ordinary identifier). Verified on both engines; cross-module calls, globals,
      local shadowing, cycle and missing-module errors.
- [x] **Standard-library search path** — a non-local `import a.b` resolves against
      `HELIX_PATH` and the install-relative location beside the binary, after the
      importing file's own directory (local imports win). General machinery for shared
      user libraries (and a future Helix-source stdlib once there is a package manager).
- [x] **Selective import** — `import lib.mod.{f, g}` brings the chosen names into scope
      unqualified (resolving to the source module), with a local definition of the same
      name shadowing the import. No new keyword: the brace tail mirrors the dotted path.
- [x] **Cross-module error attribution** — each module is given a global line range,
      so an error's line unambiguously identifies its file; the renderer maps it back
      to the owning module's source and local line, showing the right file, line, and
      caret (previously the caret could point at the entry file).

### CPython interop (v1 complete) — see [ADR 0008](adr/0008-cpython-interop.md), [guide](python-interop.md)
- [x] **Feature-gated bridge** (`cargo build --features python`, off by default so
      the core binary remains self-contained). Embeds CPython via PyO3.
- [x] **`import python.math as m` / `python.import("numpy")`** — both surface forms
      (the statement form lowers to the expression form). Attribute access and method
      calls forward to Python.
- [x] **Opaque-by-default conversion** (following the PyCall→PythonCall precedent):
      immutable scalars convert to native Helix values; lists/dicts/objects remain
      opaque `PyObject` handles (identity and mutability preserved); `to_array(x)` is
      the explicit materialization. Python exceptions become Helix diagnostics
      (`python error: <Type>: <msg>`); v1 aborts (no `try`/`catch` yet).
- [x] Works identically on the VM and the tree-walker; default and feature test
      suites both pass with zero warnings. Example: [examples/python/interop.helix](../examples/python/interop.helix).
- [~] **Zero-copy scientific bridge (the differentiator).**
      - [x] **DataFrame ↔ Python polars** — a Helix DataFrame crosses to and from
        Python's `polars` by sharing the Arrow buffers (via `pyo3-polars`);
        `to_dataframe(x)` brings a Python frame back as a first-class Helix
        DataFrame; `missing` maps to the Arrow validity bitmap with no translation.
        Verified round-trip on both engines.
      - [x] **`Tensor` ↔ Python NumPy** — a Helix Tensor crosses to and from NumPy
        `f64` arrays (via `rust-numpy`); `to_tensor(x)` brings a NumPy array back as
        a first-class Helix Tensor. Copies at the boundary (NumPy is mutable, Helix
        tensors are immutable, so each side receives its own buffer). Verified
        round-trip on both engines.
      - [ ] Truly buffer-sharing tensors via **DLPack** (GPU/large arrays) where
        mutability allows; `f32`/int dtypes.
      - [ ] pandas/pyarrow frames via the Arrow C stream interface; chunked Series
        via the stream interface (avoiding a rechunk copy).
- [ ] **Bundled relocatable CPython** (python-build-standalone / `pyembed`) so the
      feature build ships self-contained, addressing Mojo's primary pitfall (runtime
      failure to locate libpython).
- [ ] **Python → Helix** (Helix as an installable CPython extension) for calling
      Helix from Python on hot paths.

### Distribution and install — see [ADR 0009](adr/0009-distribution-and-install.md)
- [x] **A `helix` CLI** — `helix run` / `eval` / `repl` / `version` / `help`
      (plus the `helix <script.helix>` shorthand), replacing `cargo run`.
- [x] **`cargo install --path .`** plus **`install.sh`** (the eventual `curl | sh`
      one-liner: downloads a prebuilt binary, falls back to a source build) plus
      **`.github/workflows/release.yml`** (cross-builds the self-contained core for
      Linux/macOS/Windows on a tag). Prepared; prebuilt downloads activate once the
      repo is on GitHub.
- [ ] Package-manager presence (Homebrew, Scoop/winget, `cargo binstall`); a
      Windows `install.ps1`. The differentiator: **managed Python for interop**
      (uv-style) so interop operates reliably and reproducibly.

### Packaging, tooling, trust
- [ ] **Package manager and lockfile** — the distribution half of modules (path/git
      dependencies buildable locally; a registry requires hosting). The Python
      environment should be pinned here as well (related to the bundled-CPython work).
- [ ] **Jupyter kernel** — to support scientists in their existing environment.
- [~] **Error handling.** `try EXPR` -> `{ok, value, error}` is implemented (runs on
      the tree-walker). Remaining: a `Result` + `?` form
      ([ADR 0004](adr/0004-functions-errors-mutability.md)), VM support, and
      catching it across the Python-interop boundary (recoverable Python errors).
- [ ] **Semantics freeze and compatibility policy**, and a reproducible CI benchmark.

## Phase 6 — GPU
- [ ] Offload tensor/dataframe kernels to GPU (candle backend).
- [ ] Fusing tensor compiler (XLA/JAX-style): compile typed tensor/array
      expression graphs to fused CPU/GPU kernels.

## Correctness and robustness (ongoing)
- [x] **Leak-free by construction** — no interior mutability, therefore no `Rc` cycles
      and an acyclic value graph. The interpreter core is fully safe; the only `unsafe`
      is confined to the JIT's native-call boundary (`src/jit.rs`, see
      [memory-safety.md](memory-safety.md)). Backed by a deterministic
      `Rc::strong_count` test and a flat-RSS empirical check. See
      [memory-safety.md](memory-safety.md).
- [x] **Deep recursion** — the interpreter runs on a 2 GiB-stack thread; a
      `MAX_CALL_DEPTH` guard converts runaway recursion into a clean error rather
      than a crash.
- [ ] Iterative/trampolined evaluation to remove the native-stack recursion limit
      entirely (only if needed).

## Cross-cutting principles to uphold at every phase
- Prefer dots over pipes; minimize operator symbols.
- One obvious way to perform each task.
- Immutable by default.
- Errors must be instructive.
- Zero-copy where possible; lazy where it is beneficial.

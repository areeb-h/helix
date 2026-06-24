# Helix Roadmap

The north star: *create the language scientists wish they had instead of the
language they learned to tolerate.*

**Positioning:** not a general-purpose / Python replacement, but *a scientific
computing language for the post-Pandas era*, with **computational biology as the
flagship domain** (the R-for-statistics play). Tabular/stats/array domains (data
science, finance, climate, astronomy) come nearly free off the Polars/array core;
bio is the differentiated wedge. See [positioning in memory] and below.

## Flagship — Computational biology
Delegate parsing to the Rust bio ecosystem (`needletail`, `noodles`, `rust-bio`)
the way DataFrames lean on Polars; Helix is the consistent, fast, memory-safe
surface.
- [x] **`read_fasta(path)`** → array of `{id, seq, length}` records via
      `needletail` (FASTA/FASTQ, gzip-aware). `seq` is a `Dna` (ambiguous bases
      like `N` preserved). Demo: [examples/genomics.helix](../examples/genomics.helix).
- [x] **Sequence ops**: `gc_content`, `complement`, `reverse_complement`,
      `kmers(k)`, `find(motif)` (→ index or `missing`), slicing; plus `Array.top(n)`
      (frequency histogram) so `seq.kmers(9).top(20)` works.
- [x] **`read_vcf(path)` → DataFrame**: variant tables flow straight into the existing
      `where`/`group`/`count` verbs — the unified-model demo
      (`read_vcf(...).where(gene == "BRCA1").group(consequence).count(pos)`). The eight
      fixed columns plus every INFO field (`gene`, `consequence`, …) become columns.
      Demo: [examples/variants.helix](../examples/variants.helix). v1 is a hand-rolled
      parser for plain VCF; gzip/BGZF/BCF + full INFO typing via `noodles` is the next
      step. (No-arg grouped `count()` = rows-per-group is a small follow-up.)
- [ ] `read_fastq` (quality scores → `missing`-aware), `read_gff`/`read_bed`.
- [ ] BAM/CRAM via `noodles` (memory-mapped, streaming — the local-first edge).
- [ ] RNA (`fold`, `translate`), protein sequences; an ADR for the bio type model.
- [~] Python interop for adoption (call into Biopython / existing pipelines) —
      **v1 shipped** (`import python.pysam`, etc.); see
      [Phase 7](#phase-7--adoption--ecosystem). Zero-copy BAM/array sharing pending.

## Phase 1 — Core interpreter ✅ (current)

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

### Syntax polish (best of Python + TypeScript)
- [x] **String interpolation** `"{expr}"` (always-on, `{{`/`}}` escapes;
      embedded exprs type-checked).
- [x] **`??` null-coalescing** for `missing` (lowers to Polars `fill_null` in
      DataFrame predicates).
- [x] **Records** `{name: "Ada", age: 41}` + `.field` access (`.method()` keeps
      parens). Structurally typed — field typos caught at compile time. Nested,
      arrays-of-records, function-returning-record. Trailing commas allowed.
- [x] **Slicing** `xs[1:3]`, `xs[:n]`, `xs[::2]`, `xs[::-1]` (full Python
      semantics; negative indices/step; arrays, strings, DNA). Type-checked
      (preserves the collection type; non-integer bound = compile error).
- [x] Tensor **first-axis** indexing + slicing (`t[i]` row/scalar, `t[1:3]`
      sub-tensor, `t[i][j]` scalar, `t[::-1]`).
- [ ] Tensor **multi-axis** subscript `t[i, j]`, `t[1:3, :]`; `xs[i] = v` assignment.
- [ ] String-keyed / dynamic dicts `{"col": v}` + `r["key"]` access.
- [x] **Tuples + destructuring** `(a, b)`, `a, b = pair`, `mut a, b = …`, tuple
      indexing; `zip`/`enumerate` now yield tuples. Destructure arity type-checked.
- [x] **Lambda-param destructuring** `pairs.map((a, b) => a + b)` (over tuples
      from `zip`/`enumerate`, or any tuple/array element). Type-checked.
- [x] **Optional chaining unneeded** — `.` is already missing-safe (propagates
      through field + method access), so `user.name ?? "anon"` needs no `?.`.
- [ ] String-keyed / dynamic dicts `{"col": v}` + `r["key"]` access.

### Local bindings & blocks
- [x] **`let a = x, b = y in body`** — local bindings as expressions (sequential,
      scoped). The principled choice over indented blocks: indentation collides
      with multi-line dot-chains (see [ADR-0004](adr/0004-functions-errors-mutability.md)).
- [ ] Multi-statement function bodies beyond `let` (only if a real need emerges).

### Likely next within Phase 1
- [x] Control flow — **decided: method/comprehension style.** `if cond then a
      else b` is an expression; iteration is `map`/`filter`/`where`/`reduce`
      with `it` (and `acc`) bound per element. No statement keywords, no braces,
      and `where` is the same verb DataFrames will reuse.
- [x] Comprehension methods: `map`, `filter`, `where`, `reduce`.
- [x] Named element binders via `=>` (`grid.map(row => row.map(v => v + 1))`,
      `xs.reduce(0, (acc, x) => acc + x)`); `it` stays the default. See
      [ADR 0005](adr/0005-syntax-conventions.md).
- [x] Surface conventions locked: `then` kept, `count` over `len`, parens always.
- [x] User-defined functions (`fn name(a, b) = expr`, recursion, first-class
      `=>` values). See [ADR 0004](adr/0004-functions-errors-mutability.md).
- [x] `missing` value (scalar part of [ADR 0001](adr/0001-missing-data.md)):
      Option-style absence, Julia propagation + three-valued logic,
      `.is_missing()`, `.drop_missing()`, propagating aggregations.
- [x] Elementwise broadcasting for arithmetic (`xs - xs.mean()`, `xs + ys`).
- [x] More array methods: `take`, `drop`, `zip`, `enumerate`, `any`, `all`.
- [ ] Errors-as-values: `Result` + `?` ([ADR 0004](adr/0004-functions-errors-mutability.md)).
- [ ] A real test suite ✅ (44 unit tests in `interp.rs`); add golden-output
      tests for `examples/`.

## Phase 2/5 — Type system & tooling
- [x] **Static type checker** (`src/types.rs`) — bidirectional, localized
      inference (not global HM), **permissive** (errors only on provable
      mistakes; `Unknown` top type for dynamic spots; zero false positives).
      Runs after parse, before interp. See [ADR-0002](adr/0002-type-system.md).
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
- [x] SQL-intuitive verbs: `patients.where(age > 40).select(name).sort(age)`.
- [x] `group(keys).mean(col)` grouped aggregations (also sum/min/max/count/std).
- [x] Column refs inside `where`/`select`: predicate AST → Polars expression
      (`dataframe::to_polars`), so `where` is one verb across Array + DataFrame.
- [x] `read_csv` + **`read_parquet`** (memory-mapped scan), `head`/`count`
      (`len()` pushdown) / `columns` (schema only, no scan).
- [x] `write_parquet` (eager). **Benchmarked** — see [benchmarks.md](benchmarks.md):
      50M-row query ~0.2s (Parquet) / ~2.3s (CSV) warm; Parquet `count()` is O(1)
      from metadata; interpreter overhead negligible.
- [x] **`DataFrame.cache()`** — materialize a lazy frame once into memory and
      re-wrap as lazy, so later queries reuse it instead of re-scanning the source.
      Explicit + eager ⇒ no stale state, no interior mutability; safe by immutability.
      See [caching-and-memory.md](caching-and-memory.md). (Automatic reuse-detection
      could come later, but explicit is the unsurprising default.)
- [x] **Streaming sink** for `write_parquet` (Polars `sink` API): 50M-row write
      1.52 GB peak (was 4.76 GB eager, 3.1× less). Not bounded-constant yet — CSV
      scan side still buffers.
- [x] **Cold-cache benchmark** (`posix_fadvise` eviction, `scripts/coldbench.sh`):
      cold CSV ~8× slower (disk-bound); cold Parquet stays fast.
- [x] **Helix vs Python-Polars** comparison (`scripts/compare.sh`): identical
      query engine; Helix adds no query overhead, wins end-to-end (no import tax).
- [ ] Streaming engine toggle for true out-of-core reads on huge files.
- [ ] `head`-preview on print so printing a huge frame doesn't materialize it all.
- [ ] **Cross-statement caching** — reusing a DataFrame binding re-scans the file.
- [ ] More IO: Arrow IPC, JSON, FASTA; `write_csv`.
- [ ] DataFrame `missing` as the Arrow validity bitmap (unify with ADR 0001).
- [ ] Derived columns (`df.with(bmi = weight / height)`), joins.
- [ ] Formal benchmarks: variance/CI, vs pandas/DuckDB; 100M+ rows.

## Phase 3.5 — Math & numerics core (shipped)
- [x] Math standard library (broadcasts + propagates `missing`): sqrt/exp/ln/
      log/trig/hyperbolic/floor/ceil/round/abs/sign/hypot/atan2/min/max.
- [x] `**` power operator (right-assoc, Int-preserving), constants pi/e/inf.
- [x] Interpreter perf: `rustc-hash` env, edition-2024, deps optimized in dev.
- [ ] Complex numbers as a first-class type (`2 + 3i`), if demand warrants.
- [ ] Parallel array combinators (rayon `map`/`filter`/`reduce`) for big arrays.

## Phase 3.7 — Data access & APIs (shipped)
- [x] **JSON** — `parse_json(str)` (object→record, array→array, scalars, `null`→
      `missing`) and `to_json(value)`. Pure compute, always available. See
      [ADR 0010](adr/0010-networking-privacy-security.md).
- [x] **HTTP client** — `http_get(url)` → `{status, body}` for fetching REST APIs;
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
- [ ] Other dtypes (`f32`, int tensors); tensor ⊕ Array interop.
- [ ] BLAS-backed linalg for large matrices (optional feature) if perf needs it.
- [ ] Autodiff + GPU via candle/burn (Phase 6) behind the same surface API.
- [ ] `model.train(x, y)` / `model.predict(...)` ML surface.

## Phase 5 — Execution engine (performance)

The tree-walker is correct but ~100× slower than the delegated data path for
scalar/control-flow code (single-threaded, AST re-traversal, `String`-hashed env).
The staged plan, each step independently valuable:

- [x] **Stage 1 — bytecode compiler + stack VM** (`src/bytecode.rs`, `src/vm.rs`).
      Compiles the scalar/function/recursion core to a flat instruction stream
      with **slot-resolved variables** (no per-access hashing) and a **heap call
      stack** (recursion bounded by memory, not the native stack — the proper fix
      to the depth limit). Reuses the interpreter's value + arithmetic/boolean
      helpers, so it is observationally identical; anything it can't compile yet
      (arrays, methods/comprehensions, records, tensors, DataFrames, lambdas)
      falls back to the tree-walker per-program. **~3× faster on `fib(30)`
      (debug); 100k-deep recursion runs on an ordinary stack.** Parity is
      regression-gated: VM-vs-tree-walker unit tests + an all-examples diff.
- [~] **Stage 1b — widen the VM to do *everything* (the one-engine goal).** The
      end state: the bytecode VM is the *sole* executor; the tree-walker is
      removed; the JIT + memoization are optimization *tiers* on the one bytecode,
      not parallel engines — which eliminates cross-engine divergence by
      construction. Done so far: array literals, indexing, string interpolation
      (each reuses the tree-walker's own implementation, so parity is guaranteed).
      Next, in order:
      - tuples, records, field access, slicing, destructuring (mechanical, reuse
        existing impls);
      - **value-methods** via a runtime-dispatched `Method` op (reuse `call_method`);
      - **comprehensions as bytecode *loops*** — `xs.map(it + 1)` compiles to a
        tight loop with `it` in a local slot and the body inlined: no closures, no
        per-element calls, *faster* than the tree-walker. This is the key insight
        that makes "the VM does everything" tractable;
      - DataFrame/GroupBy verbs (carry the unevaluated predicate AST, reuse
        `eval_df_method` with a slot-aware variable resolver);
      - first-class lambdas (stored/passed as values) via closures — the last,
        rarest piece.
      Each step removes tree-walker fallback for more programs and is parity-tested.
- [ ] Stage 2 — **rayon-parallel** array combinators for big in-memory arrays.
- [x] **Stage 3 — Cranelift JIT, first iteration** (`src/jit.rs`). Compiles the
      integer-recursion core (`+`/`-`/`*`, `if` w/ comparison, `let`, calls to
      other eligible fns, ≤4 params) to **native machine code**; the VM calls it
      when all args are `Int`, falls back to bytecode otherwise. Eligibility is a
      fixpoint (a fn calling an ineligible fn is ineligible). **fib(35): 0.04s —
      beats Node/V8 (0.08s) and Python (0.69s), ~2× off Go, ~4× off C; 38× over
      our own VM.** JIT≡VM verified (`jit_matches_vm` test). Monomorphization-with-
      a-guard (specialize on `i64`). One contained `unsafe` (the native call).
- [x] Stage 3b — **`Float`/`f64` JIT specialization** (dual-spec: each fn compiled
      for both `i64` and `f64`; the VM dispatches by argument type). `Div` supported
      in the float path. Float recursion `fibf(35.0)` now runs native: **0.05s vs
      1.58s on the VM (32×)**, same tier as integer code. Parity-tested.
- [ ] Stage 3c — widen further: `Mod`/`Pow`, `and`/`or` in conditions,
      forward-referenced mutual recursion (two-pass bytecode fn registration), then
      array/loop kernels (the bridge to Track C).

## Phase 7 — Adoption & ecosystem

The "viability bar" — the work that turns an impressive compiler into a language
people can actually adopt. See [docs/adoption.md](adoption.md) for the honest
gap analysis that motivates this phase.

### Modules (shipped)
- [x] **`import name`** loads a sibling `name.helix`; its top-level definitions are
      reached as `name.member`. A loader resolves the import graph (dependency
      order, dedup by canonical path, cycle detection), then rewrites every module
      into one namespaced flat AST the existing pipeline runs unchanged. Single-file
      programs are untouched (pristine errors); multi-file error messages strip the
      internal namespacing.
- [x] **Subdirectory / path imports** `import lib.stats` → `lib/stats.helix`
      (relative to the importer, nested arbitrarily deep).
- [x] **Aliases** `import lib.stats as st` (`as` is contextual — still usable as an
      ordinary identifier). Verified on both engines; cross-module calls, globals,
      local shadowing, cycle + missing-module errors.
- [ ] Selective import (`from m import f`); a stdlib search path.
- [ ] Cross-module runtime-error caret attribution (message + line:col are correct;
      the caret may point at the entry file).

### CPython interop (v1 shipped) — see [ADR 0008](adr/0008-cpython-interop.md), [guide](python-interop.md)
- [x] **Feature-gated bridge** (`cargo build --features python`, off by default so
      the core binary stays self-contained). Embeds CPython via PyO3.
- [x] **`import python.math as m` / `python.import("numpy")`** — both surface forms
      (the statement form lowers to the expression form). Attribute access + method
      calls forward to Python.
- [x] **Opaque-by-default conversion** (the PyCall→PythonCall lesson): immutable
      scalars convert to native Helix values; lists/dicts/objects stay opaque
      `PyObject` handles (identity + mutability preserved); `to_array(x)` is the
      explicit materialization. Python exceptions become Helix diagnostics
      (`python error: <Type>: <msg>`); v1 aborts (no `try`/`catch` yet).
- [x] Works identically on the VM and the tree-walker; default + feature test
      suites both green, zero warnings. Example: [examples/python/interop.helix](../examples/python/interop.helix).
- [~] **Zero-copy scientific bridge (the differentiator).**
      - [x] **DataFrame ↔ Python polars** — a Helix DataFrame crosses to/from
        Python's `polars` by sharing the Arrow buffers (via `pyo3-polars`);
        `to_dataframe(x)` brings a Python frame back as a first-class Helix
        DataFrame; `missing` ↔ Arrow validity bitmap with no translation. Verified
        round-trip on both engines.
      - [x] **`Tensor` ↔ Python NumPy** — a Helix Tensor crosses to/from NumPy
        `f64` arrays (via `rust-numpy`); `to_tensor(x)` brings a NumPy array back as
        a first-class Helix Tensor. Copies at the boundary (NumPy is mutable, Helix
        tensors are immutable — each side gets its own buffer). Verified round-trip
        on both engines.
      - [ ] Truly buffer-sharing tensors via **DLPack** (GPU/large arrays) where
        mutability allows; `f32`/int dtypes.
      - [ ] pandas/pyarrow frames via the Arrow C stream interface; chunked Series
        via the stream interface (avoid a rechunk copy).
- [ ] **Bundled relocatable CPython** (python-build-standalone / `pyembed`) so the
      feature build ships self-contained — closes Mojo's #1 footgun (runtime
      "can't find libpython").
- [ ] **Python → Helix** (Helix as an installable CPython extension) for calling
      Helix from Python on hot paths.

### Distribution & install — see [ADR 0009](adr/0009-distribution-and-install.md)
- [x] **A real `helix` CLI** — `helix run` / `eval` / `repl` / `version` / `help`
      (plus the `helix <script.helix>` shorthand). No more `cargo run`.
- [x] **`cargo install --path .`** + **`install.sh`** (the eventual `curl | sh`
      one-liner: downloads a prebuilt binary, falls back to a source build) +
      **`.github/workflows/release.yml`** (cross-builds the self-contained core for
      linux/macOS/Windows on a tag). Ready; prebuilt downloads activate once the
      repo is on GitHub.
- [ ] Package-manager presence (Homebrew, Scoop/winget, `cargo binstall`); a
      Windows `install.ps1`. The differentiator: **managed Python for interop**
      (uv-style) so interop "just works" and stays reproducible.

### Packaging, tooling, trust
- [ ] **Package manager + lockfile** — the distribution half of modules (path/git
      deps buildable locally; a registry needs hosting). The Python environment
      should be pinned here too (ties into the bundled-CPython work).
- [ ] **Jupyter kernel** — meet scientists where they work.
- [ ] **Errors-as-values** (`Result` + `?`, [ADR 0004](adr/0004-functions-errors-mutability.md))
      — also unblocks *recoverable* Python errors.
- [ ] **Semantics freeze + compatibility policy**, and a reproducible CI benchmark.

## Phase 6 — GPU
- [ ] Offload tensor/dataframe kernels to GPU (candle backend).
- [ ] Fusing tensor compiler (XLA/JAX-style): compile typed tensor/array
      expression graphs to fused CPU/GPU kernels.

## Correctness & robustness (ongoing)
- [x] **Leak-free by construction** — no `unsafe`, no interior mutability ⇒ no
      `Rc` cycles ⇒ acyclic value graph. Backed by a deterministic
      `Rc::strong_count` test + flat-RSS empirical check. See
      [memory-safety.md](memory-safety.md).
- [x] **Deep recursion** — interpreter runs on a 2 GiB-stack thread; a
      `MAX_CALL_DEPTH` guard turns runaway recursion into a clean error, not a
      crash.
- [ ] Iterative/trampolined eval to remove the native-stack recursion limit
      entirely (only if needed).

## Cross-cutting principles to defend at every phase
- Prefer dots over pipes; no symbol soup.
- One obvious way to do each thing.
- Immutable by default.
- Errors must teach.
- Zero-copy where possible; lazy where it pays.

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
- [x] **`read_fasta(path)`** → array of `{id, seq, length}` records via
      `needletail` (FASTA/FASTQ, gzip-aware). `seq` is a `Dna` (ambiguous bases
      like `N` preserved). Demo: [examples/bio/genomics.helix](../examples/bio/genomics.helix).
- [x] **Sequence ops**: `gc_content`, `complement`, `reverse_complement`,
      `kmers(k)`, `find(motif)` (→ index or `missing`), slicing; plus `Array.top(n)`
      (frequency histogram) so `seq.kmers(9).top(20)` works. Native 2-bit-packed
      `kmer_counts(k)` (forward) and `canonical_kmer_counts(k)` (strand-agnostic, a
      k-mer and its reverse complement counted together — the Jellyfish/KMC convention).
- [x] **`read_vcf(path)` / `read_bcf(path)` → DataFrame**: variant tables flow
      directly into the existing `where`/`group`/`count` verbs, demonstrating the unified model
      (`read_vcf(...).where(@gene == "BRCA1").group(@consequence).count(@pos)`). The eight
      fixed columns plus every INFO field (`gene`, `consequence`, …) become columns, each
      **header-typed** (an `Integer`/`Float`/`Flag` INFO field becomes a numeric/bool column,
      so `where(@af > 0.001)` is a numeric comparison). Parsing delegates to `noodles`; plain
      `.vcf`, gzipped/BGZF `.vcf.gz`, and binary **BCF** all share one record model and
      column-building core. Demo: [examples/bio/variants.helix](../examples/bio/variants.helix). (No-arg
      grouped `count()` for rows-per-group is a small follow-up.)
- [x] **`read_fastq`** — FASTQ reads as records `{id, seq, qual, length}` (via
      `needletail`); `seq` is a DNA value and `qual` the Phred string. `qual.phred()`
      decodes the Phred+33 string to per-base integer quality scores, which compose
      with the array verbs — a read's mean quality is `qual.phred().mean()` and a
      quality filter is one `where`. Demo:
      [examples/bio/sequencing.helix](../examples/bio/sequencing.helix).
- [x] **`read_gff` / `read_bed`** — feature/interval tables as DataFrames (GFF3 via
      `noodles-gff` with one column per attribute tag; BED hand-rolled for BED3/6/12).
- [x] **`read_sam` / `read_bam`** — sequence alignments as a DataFrame: the eleven
      mandatory SAM fields (`name`, `flag`, `ref`, `pos`, `mapq`, `cigar`, `rnext`,
      `pnext`, `tlen`, `seq`, `qual`) become columns (`ref`/`rnext` resolved to names
      from the header, CIGAR rendered to its SAM string). BAM is the binary, BGZF-framed
      form; both share one record model and column-building core via `noodles-sam`/
      `noodles-bam`. Demo: [examples/bio/alignments.helix](../examples/bio/alignments.helix).
- [x] **Indexed region queries** — `read_vcf(path, "chr17:43k-44k")` and
      `read_bam(path, "chr1:1k-2k")` seek via the file's index (`.tbi` for VCF, `.bai`
      for BAM) and read only the variants/reads in the region, never scanning the whole
      file (the local-first capability). The result is identical to a full read filtered
      to the region. Demos: [variants.helix](../examples/bio/variants.helix),
      [alignments.helix](../examples/bio/alignments.helix).
- [x] **Pairwise alignment** — `seq.align(target[, mode])` (global / local /
      semiglobal) via a hand-rolled Gotoh affine-gap aligner (ADR 0015), returning a
      record `{score, cigar, query, target, start, end}`. Demo:
      [examples/bio/alignment.helix](../examples/bio/alignment.helix).
- [ ] Region queries for BCF (its `.csi` index); CRAM via `noodles-cram`
      (reference-based compression); an RNA/protein sequence type model; custom
      alignment scoring (substitution matrices) — likely via a scoring-record
      argument, since `align` is a method and named arguments are user-function-only.
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
- [x] **Record update / spread** `{ ...base, field: value }` — copy a record with
      fields overridden/added, producing a new immutable record (later keys win).
      Low-viscosity "same but …" updates; also fixed a real bug class (field-by-field
      rebuilds silently dropping fields). See
      [syntax-and-dx.md](syntax-and-dx.md) Proposal 7.
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
- [x] **`do { … }` blocks** — a sequence of `name = expr` bindings and a final result
      expression, desugared at parse time to the `let … in` chain (zero run-time cost).
      The idiomatic multi-step body; also used for multi-step lambda bodies. See
      [syntax-and-dx.md](syntax-and-dx.md) Proposal 6.
- [x] **Callable function values** — a function held in a variable / record / array
      field is invoked with call syntax; parenthesise an expression callee to
      distinguish it from a method call: `(rec.handler)(x)`. Enables router / dispatch-
      table patterns. See [ADR 0005](adr/0005-syntax-conventions.md).

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
- [x] **Named arguments and default parameters** for user functions:
      `fn greet(name, greeting = "Hi") = …`, called `greet("Ada", greeting: "Hey")`.
      Resolved to positional form at parse time (zero run-time cost). Defaults are
      literal constants; builtins/methods stay positional (a follow-up). Demo:
      [examples/language/named-arguments.helix](../examples/language/named-arguments.helix).
      Shipped follow-ups: named args + defaults now also resolve **through module
      qualification** (`dep.f(x, open: -10)` — [ADR 0019](adr/0019-module-system.md))
      and **inside interpolation holes** (`"{gap(3, open: -10)}"`).
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
- [x] Bivariate: `correlation(xs, ys)` (Pearson r; symmetric, undefined on a
      constant series).
- [x] DataFrame-column statistics: `df.column(name)` materializes a column as an
      array (Polars nulls → `missing`), so the array statistics and verbs apply to
      loaded data — e.g. `df.column("age").median()`, or `drop_missing()` first.
- [x] Special-functions layer: `erf`, log-gamma, and the regularized incomplete beta
      (Abramowitz & Stegun / Numerical Recipes), accurate to better than 1e-7.
- [x] Inferential: `t_test(a, b)` — Welch's two-sample t-test → `{statistic, df,
      p_value}` — and the normal distribution functions `normal_cdf`/`normal_pdf`/`erf`
      (broadcasting math). Verified against R's reference values.
- [x] `linear_regression(x, y)` — ordinary least-squares fit → `{slope, intercept,
      r_squared, slope_std_error, slope_p_value}` (slope inference on `n - 2` df).
      Predictions need no special method: broadcast `fit.slope * x + fit.intercept`.
- [x] `multiple_regression(predictors, y)` — OLS on several predictors via the normal
      equations → `{coefficients, std_errors, p_values, r_squared, adj_r_squared}`
      (parameter-indexed arrays, intercept first). Rejects collinear predictors.
- [ ] More distributions: Student's-t / binomial / chi-squared pdf/cdf/quantile, and
      one-sample / paired t-tests, on the same special-functions layer.
- [ ] Whole-frame aggregation shorthands (`df.median(col)`, `df.correlation(c1, c2)`)
      over the `column` accessor, if the explicit form proves verbose in practice.

## Phase 3.7 — Data access & APIs (shipped)
- [x] **JSON** — `str.parse_json()` (object→record, array→array, scalars, `null`→
      `missing`) and `value.to_json()`. Pure compute, always available. See
      [ADR 0010](adr/0010-networking-privacy-security.md).
- [x] **HTTP client (complete)** — `http_get(url)` → `{status, body}`;
      `http_post(url, body[, headers])`; `http_request(url, {method, headers, body})`
      the general form exposing **response headers** too; and `http_stream(url, …)`, a
      **pull-based streaming client** (consume a response chunk-by-chunk — large
      downloads, token streams — instead of buffering). Default-on (`http` feature;
      `--no-default-features` for a network-free binary). Demo:
      [examples/api/fetch.helix](../examples/api/fetch.helix).
- [x] **Native HTTP server (from-scratch, `std::net`, no async runtime, no new dep)** —
      `listen`/`accept`/`respond` with custom headers/redirects/cookies/CORS; non-blocking
      `poll` for cooperative multi-client; **Server-Sent Events** (`sse`/`send`);
      `SO_REUSEPORT` share-nothing sharding across cores; and a **cooperative event-loop
      keep-alive** mode (`accept_poll`/`poll_request`/`is_open`/`wait`) measured at **83k
      req/s on one core**. Governed by [ADR 0022](adr/0022-http-version-roadmap.md);
      untrusted-input surface bounded (request-head caps, SSE backlog budget) per
      [audit.md](audit.md). Demo:
      [examples/api/event_server.helix](../examples/api/event_server.helix).
- [x] **Stream ergonomics** — `write`/`elog` sinks (no-newline stdout write; stderr log),
      stream `.close()`, and a per-chunk timeout on streaming reads, so streaming servers
      and clients are ergonomic in-model.
- [x] **String-keyed record access `r["key"]`** — dynamic field access for JSON keys
      that aren't valid identifiers (e.g. `d["first-name"]`); an absent key is
      `missing` (the safe/optional accessor; `.field` stays the typo-catching one). Plus
      `r.get(k)`/`r.has(k)`/`r.keys()` for unknown-shape (parsed-JSON) records.
- [ ] Reading CSV/Parquet/JSON straight from a URL; client auth helpers.
- [ ] **HTTP/2 & HTTP/3** and linear multi-core scaling — a deliberate future
      major-version step on an async stack (Tokio + hyper + Quinn, TLS via rustls),
      **never a hand-rolled QUIC/TLS/H2 core**. See
      [ADR 0022](adr/0022-http-version-roadmap.md) Stages 2–3.
- [ ] Heavy web-backend / gRPC / websockets / Kafka stay out of the core — via Python
      interop. (The minimal `std::net` server above is the in-core exception: serving a
      dashboard / SSE stream / small local API is a real scientific need and stays
      dependency-free.)

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
- [x] **Stage 1c — `.hbc` serializable core-bytecode container + emitter
      (`helix emit-hbc`).** Lowers a compiled bytecode `Program` (the dependency-free
      scalar core — Int/Float/Bool `+ - *` and comparisons, frame locals, if/while,
      direct and tail calls; `/` is deliberately excluded — Helix `/` float-promotes,
      hvm's DIV does not, see ADR 0023's amendment) to the byte-exact `.hbc` (Helix
      Bytecode Container) format, reconciling
      Helix's instruction-index jump targets and per-chunk constant pools with hvm's
      byte-offset targets and one program-global scalar pool. Anything outside the core
      is rejected with a source-attributed error. This is **ctype's ring-0 execution
      substrate (V2.5)**: `helix emit-hbc` produces `.hbc` that ctype's embedded no_std,
      zero-allocation VM runs in kernel ring 0 — verified end-to-end in QEMU (a
      Helix-compiled `fib(25)=75025` matching the hand-assembled demo). The byte format
      is specified authoritatively in [ADR 0023](adr/0023-hbc-emitter-artifact-format.md).
      **Host calls (2026-07-10):** the emitter lowers `print`/`emit`/`elog` to hvm's
      `CALL_HOST 0`, `sleep` to `CALL_HOST 1`, and `read_int` to `CALL_HOST 2`, so a `.hbc`
      program can print, pace itself, and **read console input** through host-mediated
      capabilities — ctype gates them on `CAP_PRINT`/`CAP_SLEEP`/`CAP_GETKEY`. `read_int()`
      is also a new first-class Helix builtin (0-arg → `Int`; reads a line from stdin,
      `missing` on EOF). Interactive Helix *system programs* now compile and run in ring 0:
      `greet(n)=print(fib(n))`, a `fibseq` streaming via `emit`, a `paced` `sleep`-countdown,
      and `add2()=print(read_int()+read_int())` reading two typed numbers. More
      builtins/capabilities follow as the host ABI grows (ADR 0023's Host ABI section).
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
- [x] **Stage 3i — map→reduce fusion by substitution.** The last spelling inversion:
      `(0..n).map(f).reduce(init,g)` materialized an intermediate array the equivalent
      direct reduce never builds. MEASURED (n=5M, min-of-4, JIT): `f64_mr`
      **0.21s → 0.00s**, `f64arr_mr` **0.23s → 0.03s**, `saxpy_mr` **0.23s → 0.02s** —
      each now equal to its `_dr` twin, so the table has no inversions left. The VM
      path is unchanged (1.39s vs 1.47s before; the first reading of 6.12s was a
      single cold run — measure min-of-N).

      Implemented as the algebraic identity `map(f).reduce(init,g) ≡ reduce(init,
      (acc,i) => g(acc, f(i)))`, NOT by teaching `FusionStage` to carry f64 elements.
      That alternative would have built a SECOND implementation of "f64 body with
      array captures and bounds" beside the reduce path's — which already has f64
      accumulators over the i64 counter, `ArrayF64` captures with i128-proven bounds,
      affine indices, value scalars, multi-accumulator splitting, and the poison-flag
      division rule. Substitution reuses the proven one; the duplicate would drift.

      **SAFETY comes from the emission shape, not the identity.** The fused body is a
      `TryJitReduce` GUARD whose fall-through is the ORIGINAL unfused expression, so
      the fused form runs ONLY as native code and only once the VM has proven `Int`
      bounds within the cap, a `Float` init, every array index in range, and a clear
      poison flag. A kernel meeting those cannot raise — which retires the one real
      hazard of map-reduce fusion: unfused, `map` evaluates EVERY `f(i)` before any
      `g`, so if both can raise the two spellings report DIFFERENT errors (f's
      out-of-bounds at a later index vs g's division at an earlier one). Every
      raising case takes the untouched original path.

      Three guards, each sabotage-proven against a specific case:
      * the capture check (`f` must not mention `pa`) — without it,
        `s = 5.0; map(i => s * 1.0).reduce(0.0, (s,x) => s+x)` binds `s` to the
        accumulator and returns 0.0 instead of 15.0;
      * the SYNTHETIC accumulator slot name — naming it `pa` shadows an outer
        variable for the recompiled fall-through, breaking
        `range(0,u)…reduce(0.0,(u,x) => …)`. **Visible only on the VM path**, since
        the JIT path takes the fused route and never runs the fall-through — which is
        why every battery here compares all three engines;
      * the `no_fuse` re-entry flag — without it, compiling the fall-through
        re-detects the same expression and compilation hangs (exit 124).

      `subst_ident` (`src/jit.rs`) is deliberately partial: it handles only the pure
      arithmetic node set the f64 reduce can lower and returns `None` for anything
      else, so every binding form (`let`, lambda, `match`) is excluded by
      construction and there is no shadowing case inside the substituted region to
      get wrong. Declines (and stays correct via the original) on: an i64 init (the
      i64 map→reduce already fuses through `FusedKernel` — measured equal to its
      direct twin, and untouched), a binding form, chained maps, a filter in the
      chain, and a non-idempotent bound (which the guard's operands and the
      fall-through would otherwise evaluate twice).

      Pinned by `map_reduce_fusion_is_exact_and_declines_where_it_must` — whose
      engagement check is the *property*, not a counter threshold: the map spelling
      must cost the SAME number of native calls as the equivalent direct reduce, so
      it self-calibrates if kernel accounting changes — plus
      `tests/corpus/j6_map_reduce_fusion.helix`, a 24-case cross-engine battery, and
      1500 fuzzed programs across 5 seeds that randomize both bodies, the init type,
      array representations, and the capture/shadow/raise shapes so the DECLINE paths
      are exercised as hard as the fused one. 0 divergences.

- [x] **Stage 3j — numeric recursion JITs without type annotations.** Found while
      investigating whether to replace Cranelift, and it turned out to be the largest
      single perf defect in the engine. The mixed per-parameter specialization — the
      sound successor to the removed blanket-`f64` function spec (see the note above
      `let kind = NumKind::Int` in `build`: a float-arg function can still return an
      `Int`, so blanket `f64` codegen diverged on result type) — was reachable ONLY
      through explicit `: Int` / `: Float` annotations. One line,
      `_ => return None`, meant that the natural shape of a numeric loop (float state
      plus an integer counter) never reached native code at all.

      MEASURED (n=5M, min-of-3, gate profile), identical bodies:

      | signature | before | after |
      | --- | --- | --- |
      | `fn spin(zr, zi, i, n)` (mixed) | 0.53s | **0.01s** (53×) |
      | `fn spin(zr, zi, i, lim)` (all `Float`) | 0.63s | **0.01s** (63×) |
      | `fn spin(zr: Float, …, n: Int)` (annotated) | 0.01s | 0.01s |
      | `fn spin(a, b, i, n)` (all `Int`) | 0.00s | 0.00s |

      `JIT ≈ NOJIT` on the unannotated forms confirmed they never compiled — this was
      a cliff, not a slow path.

      `infer_param_kinds` proposes a kind per unannotated parameter by float taint:
      a `Float` literal, `sqrt`/`to_float`, or a division forces `f64`; `%`, `//`,
      bitwise, shifts and array indices force `i64`; a self-call ties argument *j* to
      parameter *j* (the strongest signal in a tail-recursive function); and a
      comparison ties its two sides, which is the only way the limit in `i >= lim`
      gets its type. Contradictory evidence declines the function rather than picking
      a side. It iterates to a fixpoint because taint propagates.

      **It only has to be PLAUSIBLE, not sound**, and that is the design's point:
      `mixed_tail_ret_kind` re-types the whole body under the proposal and declines if
      anything fails to check, and the VM re-tests every ARGUMENT's runtime type
      against the compiled `float_mask` before dispatching (`vm.rs`, `Op::CallFn`). So
      a wrong proposal costs a specialization that is never called — never a wrong
      answer. Annotations, where present, are still honoured exactly.

      Verified: `unannotated_numeric_recursion_reaches_native_code` (four
      shapes — mixed, all-float, the mandelbrot inner loop, and a partly-annotated
      signature — each checked against the walker AND asserted to engage native code,
      since agreement alone would also hold if nothing compiled), plus the decline
      path; sabotage-proven (restoring the old behaviour fails the test with "the
      annotation cliff is back"); 1300 fuzzed unannotated mixed-recursion programs
      across 5 seeds, 0 divergences. The suite's kernels are all annotated and
      unchanged.

- [x] **Stage 3k — the numeric-builtin coverage audit, and the three it closed.** After
      `to_float` turned out to be missing from every float gate (132–227×), all 22 numeric
      builtins were audited the same way — a hot loop with the JIT on and off, where
      `JIT ≈ NOJIT` means the builtin blocks compilation. **17 of 22 blocked.** Full
      standing result, with the reason for each exclusion, in
      [jit-builtin-coverage.md](jit-builtin-coverage.md).

      Closed here: **`to_float`** (Stage 3i), **`to_int`** and **`sign`**. Measured at
      n=30M, before = the VM time (these did not compile at all):

      | shape | before | after | |
      | --- | --- | --- | --- |
      | `map(to_int(it) * 2)` | 3.08s | 0.01s | **308×** |
      | `map(to_float(to_int(x * 1.5)))` | 4.41s | 0.02s | **220×** |
      | `reduce(s + sign(i - k))` | 1.91s | 0.01s | **191×** |
      | `map(sign(it - k))` | 3.18s | 0.02s | **159×** |

      The dividing line is **"can it fail?"**, not "what does it return". `to_int` and
      `sign` never raise — `to_int` SATURATES (NaN → 0, ±inf → the i64 extremes, exactly
      `fcvt_to_sint_sat` and Rust's `as i64`), and `sign`'s two comparisons both go false
      on NaN so the selects fall through to 0, matching the interpreter (which compares
      rather than using `signum`, so it does not propagate NaN). That is what let them be
      lowered with no new machinery.

      STILL EXCLUDED, deliberately: `floor`/`ceil`/`round`/`trunc` return an `Int` and
      **raise** out of i64 range, and `clamp` raises when `lo > hi`. A kernel cannot raise
      mid-loop, so they need the **poison out-param** pattern the dividing f64 reduce
      already uses, extended to the map/filter/fused kernels. Recorded with it: Helix's
      `round` is **half-away-from-zero**, NOT IEEE round-to-nearest-even, so lowering it to
      Cranelift's `nearest` would be silently wrong on every tie — a bug no small-input
      test would catch. The transcendentals stay out permanently (they must match the host
      libm bit-for-bit).

      Also measured and recorded as a separate, larger gap: an **`Int`-rooted body with
      `Float` intermediates** (`map(to_int(to_float(it) * 1.5))`) has no kernel shape at
      all — 4.05s JIT vs 4.01s VM — because the i64 kernel cannot hold a float intermediate
      and the mixed kernel needs a float root. No builtin work changes that; it needs a
      Float-source → Int-output map specialization.

      Verified by `to_int_and_sign_compile_and_match_the_interpreter_at_every_edge`
      (17 value cases across saturation, both infinities, NaN, both zeroes, Int identity,
      reduce bodies and a tail-recursive function, plus an engagement assertion) and by
      asserting the EXCLUDED builtins still raise identically on all three engines.

- [x] **Stage 3l — a range `map` no longer materializes its source: peak memory −43%.**
      Helix was the heaviest implementation in the kernel suite — `k1_dot.helix`
      recorded ~1,195 MB peak against C's 783 MB for the same 800 MB of arrays. The
      whole overhead was one transient: `densify_range_top` built a full-size `Ints`
      buffer purely so the map kernel had something to *read*, and it stayed live
      alongside the output, so a single `(0..n).map(f)` peaked at **twice** its result.

      MEASURED (n=20M, 160 MB of payload per array):

      | shape | before | after |
      | --- | --- | --- |
      | `(0..n).map(f)`, array kept | 328 MB | **186 MB** |
      | k1 shape (two arrays) | 485 MB | **345 MB** |
      | `(0..n).map(f).reduce(…)` (already fused) | 20 MB | 20 MB |

      Overhead above payload fell from ~165 MB to ~25 MB. It is also **2–3× faster**,
      because a full buffer is no longer written and then read back.

      A range's element is `start + step*j`, so there is nothing to store: the kernel
      now receives values generated into a 16K-element scratch (128 KB, cache-resident)
      reused per chunk, via `run_map_range_chunked`. The kernel itself is unchanged —
      only what feeds it — so no codegen was touched. The formula is `range_at`'s
      verbatim, in `i128`, so the multiply cannot overflow before the truncation the
      interpreter also performs.

      The sharp edge is CHUNK BOUNDARIES: the element index must be `base + k`, not
      `k`, and a bug there is invisible below 16384 elements. Pinned by
      `range_map_generates_values_without_materializing`, which reads
      `a[16383]`/`a[16384]`/`a[32767]`/`a[32768]` individually rather than summing (a
      sum can hide a swapped pair), and straddles `PAR_MATH_THRESHOLD` where generation
      moves into rayon workers. Plus degenerate ranges, negative steps, an `i64::MAX`
      start, indexed maps (whose bounds still discharge against the range endpoints),
      and 880 fuzzed programs across the boundary sizes — 0 divergences.

      `filter` over a range now takes the same path: **250 MB → 98 MB (−61%)** on a 20M
      range keeping half, and slightly faster. Its generation is serial by construction —
      a filter COMPACTS, so chunk *i*'s output offset depends on how many elements chunks
      `0..i` kept, which is the same dependency that already makes `run_filter_kernel`
      serial. Chunk offsets are the sharp edge, so the test includes a predicate keeping
      ~one element per chunk, where a wrong offset shows up at once instead of being
      absorbed by its neighbours.

      REMAINING memory work at the time: a map over a REAL array necessarily keeps both
      buffers — only a *range* source can be generated away. Closed by Stage 3o below.

- [x] **Stage 3m — a mixed `Int`→`Float` map may CAPTURE, which unblocked k8's build.**
      An `Int`-source map whose body produces `Float` compiled *only if the body
      captured nothing*, so swapping one literal for a variable moved the whole map onto
      the VM. At 4M elements `((7 * j) % 100) * 0.5` ran native in 0.01s while
      `((c * j) % 100) * 0.5` took 0.37s — the same arithmetic. The `i64` analysis had
      always taken captures; `mixed_map_eligible` was capture-free by construction, and
      `mixed_map_captures_indexed` (which does take them) required a non-empty
      `index_bounds`, so an *unindexed captured* body matched no analysis at all.

      MEASURED (20M elements, min-of-5 on both engines):

      | body | JIT | VM | |
      | --- | --- | --- | --- |
      | `((c * j) % 100) * 0.5` captured | 0.02s | 1.72s | **86×** |
      | `((7 * j) % 100) * 0.5` literal | 0.02s | 1.69s | 84× |
      | `(c * j) * 0.5` | 0.02s | 1.42s | **71×** |
      | `i * dt * 0.001` | 0.02s | 1.45s | **72×** |

      The number that matters is 86 vs 84: the captured spelling now matches the
      capture-free one, so the *inversion* is gone rather than merely shrunk. On k8 the
      nested build fell **0.17s → 0.02s (8.5×)** and the kernel **0.39s → 0.17s**,
      output bit-identical on all three engines.

      A free scalar rides as a plain `i64` `CaptureKind::Scalar` and is typed `Int` by
      `gen_value_typed` — which is what keeps an integer subexpression containing it
      *wrapping* exactly like the interpreter's. The old comment said a capture's runtime
      type "is unknown at compile time … which we couldn't guarantee". It cannot be
      guaranteed statically, but it can be *proved at dispatch*: both sites (`try_map_range`
      and `Op::TryJitMap`) now marshal through `int_scalar_caps`, which requires every
      capture to be a `Value::Int` and declines to the bytecode loop otherwise — the
      identical runtime proof the plain i64 map path has always used. A `Float` there
      would promote earlier in the kernel than in the interpreter, so declining is the
      correctness rule, not a missed optimization.

      That guard is the load-bearing one and it is sabotage-proven: accepting a `Float`
      by truncation makes `c = 2.5` return `[0.0, 1.0, 2.0, 3.0]` on the JIT against
      `[0.0, 1.25, 2.5, 3.75]` on the other two engines. The test therefore uses `2.5`
      and not just `2.0`, whose truncation is invisible. The build-side re-check also
      requires the re-derived capture list to equal the stored one, so codegen's
      `caps[j]` and the VM's marshal order cannot drift — the same discipline the indexed
      arms already had.

      Pinned by `captured_mixed_int_to_float_map_agrees_and_declines_a_float_capture`:
      the wrap cases (`c = i64::MAX`), both sides of 2^53 where mistyping would diverge,
      `Float`/non-numeric captures that must decline, empty and reversed ranges, a data
      array source, multiple captures, and k8's own nested shape — plus an engagement
      assertion, since agreement alone would be satisfied by a JIT that declined
      everything.

      Two hypotheses were tested and REJECTED before writing any code: reassociating to
      promote first (`(j * 0.5) * c`) did not help, so this was the missing analysis and
      not the `mix_combine` value-scalar rule; and nesting was not the cause — the same
      body at top level with a captured scalar was equally VM-bound, so k8 only *looked*
      like a nesting problem.

- [x] **Stage 3n — `tensor()` stops boxing every element, and k8 overtakes NumPy.**
      With Stage 3m's build cost gone, conversion was k8's largest single term: **0.12s**
      to turn a nested 1024×1024 array into a tensor, ~120 ns per element where the copy
      itself is an 8 MB memcpy. The cause was `ArrayData::to_values()`, which materializes
      a `Vec<Value>` — for a packed `Floats` buffer that means BOXING every element. It
      was called twice per row: once in `shape_of`, purely to re-derive `vec![]` from each
      element and throw it away, and once in `flatten_into` to copy. A 1024×1024 build
      paid ~2M allocations, into a `Vec::new()` that doubled its way to 8 MB.

      Three changes, no new machinery: `shape_of` short-circuits a packed
      `Ints`/`Floats`/`Range` buffer to `[len]` without walking it; `flatten_into` copies
      one straight through (`extend_from_slice` for `Floats`, a converting `extend` for
      `Ints`, `range_at`'s `i128` formula for `Range`); and `from_value` sizes the buffer
      from the known shape instead of growing it.

      MEASURED (k8 phases at n=1024, cumulative):

      | phase | before 3m | after 3m | after 3n |
      | --- | --- | --- | --- |
      | build both | 0.17s | 0.02s | 0.03s |
      | + `tensor()` both | 0.32s | 0.14s | **0.03s** |
      | + `matmul` (= k8) | 0.37s | 0.17s | **0.05s** |

      **Whole kernel min-of-8: 0.04s against NumPy's 0.07s — Helix is 1.75× faster**, at
      67 MB to NumPy's 58 MB and ~120% CPU to NumPy's 471%. Output bit-identical on all
      three engines (`606023.500000`). This is a WHOLE-PROGRAM win: the isolated GEMM
      still loses to OpenBLAS ~1.8×, and nothing here changed that.

      The short-circuit is only sound because a packed buffer holds numbers by
      construction, so the element walk it skips could never have rejected anything —
      `missing` and non-numeric elements only ever live in an `ArrayData::Values`, which
      still takes the original path (where `to_values()` borrows and costs nothing).
      Ragged detection is the sharp edge, since only non-first rows are compared against
      the first: `tensor_construction_takes_the_packed_path_without_weakening_its_checks`
      covers packed+packed, packed+nested AND nested+packed, plus the `Ints`/`Range`
      conversions, an empty buffer's `[0]` shape, and element ORDER (a row/column mix-up
      would otherwise pass a shape-only check). Sabotage-proven: extending the
      short-circuit to `Values` — which would skip real ragged checks — fails the test.

- [x] **Stage 3o — a map over a DEAD buffer reuses it: chained-map peak −45%.**
      Stage 3l removed the transient for a *range* source, leaving the case it could not
      reach: a map over a real array allocated a fresh output while the input stayed live.
      For `xs.map(f).map(g)` that is pure waste — the intermediate is dead the moment `g`
      consumes it — and it also pays to zero a fresh `Vec` about to be overwritten in full.

      MEASURED (n=20M, 160 MB per buffer, peak RSS):

      | shape | before | after |
      | --- | --- | --- |
      | `(0..n).map(f).map(g)`, intermediate dead | 340 MB | **186 MB** |
      | three-stage chain, all temps | 344 MB | **186 MB** |
      | `f64` two-stage chain | 340 MB | **186 MB** |
      | source still NAMED and live | 338 MB | 340 MB (unchanged — correct) |

      That last row is the result, not a miss: a reachable source must keep its own buffer.
      The whole mechanism is `Rc::get_mut`, which succeeds only when the handle on the stack
      is the ONLY one — so reuse happens exactly when the mutation is unobservable, and the
      values are identical either way. This is an allocation decision, not a semantic one.
      It follows the interpreter's existing precedent (`map_buf_inplace`, used by `abs` and
      the float math functions behind the same `Rc::get_mut` gate).

      ALIASING is the sharp edge on the kernel side: `run_map_inplace` hands the same
      pointer to the kernel as both source and destination. Sound because `dst[i]` depends
      only on `src[i]` and the read-only `caps` — each iteration reads its own index before
      storing to it, and never looks at an index another iteration writes. Both pointers are
      derived from ONE `&mut` borrow (not a `&`/`&mut` pair), and the parallel form keeps the
      property because `par_chunks_mut` hands out disjoint sub-slices that alias only
      themselves at matching indices — which is also why output stays byte-identical to the
      sequential run. A body reading a *captured* array is excluded by an explicit
      `index_bounds.is_empty()` check at the call site; such bodies are already range-only
      (their bounds are dischargeable only against range endpoints), so it cannot trigger
      today, but the guard keeps the safety argument local instead of in another function.

      The failure mode is the opposite of a wrong number — a live source silently rewritten —
      so `map_reuses_a_dead_buffer_but_never_a_reachable_one` keeps a SECOND way to observe
      the original in every case and reads it after the map: under its own name, an alias, a
      record field, a closure capture, and a source mapped twice (where the second map must
      see the original). Plus chunk boundaries read individually across
      `PAR_MATH_THRESHOLD`, and an engagement assertion. Sabotage-proven: replacing
      `Rc::get_mut` with an unconditional `&mut` makes `src` print `[10, 20, 30]` instead of
      `[1, 2, 3]` on the JIT while the other two engines still print the original.

- [x] **Stage 3p — a mixed `Int`→`Float` map body may CALL an `i64` user function: 75–100×.**
      Factoring a loop body into a named function dropped the whole map to the bytecode
      loop. The `i64` map kernel had always been able to call user functions; the mixed one
      could not, because `define_array_kernel` hands the non-mixed path
      `gen_value(…, fn_ids, module, …)` while the mixed path called `gen_value_typed(…)`,
      which took neither — so it had no call support, and `infer_mixed_kind`'s `Call` arm
      admitted only builtins to match.

      MEASURED (20M elements, min-of-3 both engines):

      | body | before | after | inline twin |
      | --- | --- | --- | --- |
      | `f(i) * 0.5` | 1.50s | **0.02s** | 0.02s |
      | `(f(i) % 100) * 0.5` | 2.00s | **0.02s** | 0.02s |

      Both now equal their inlined spelling exactly, which is the result — the inversion is
      gone, not merely smaller.

      An `i64`-eligible callee needs no new information to type: `int_eligible` means
      "i64-closed for all-`Int` arguments", so such a call takes `Int` args and returns
      `Int`, and the enclosing expression promotes it at the first `Float` precisely where
      the interpreter does. That is why this case is separable from the `Float`-parameter
      one still open above, which needs signatures the bytecode compiler does not have.

      SHADOWING is the sharp edge: the user-call arm is tried BEFORE the inline-builtin arm
      in both the analysis and the codegen, so `fn abs(x) = x + 1000` dispatches to the
      user's function and never to `iabs` — the same precedence `gen_value`'s `fn_ids`
      lookup already establishes. `a_mixed_map_body_may_call_an_i64_user_function` shadows
      all five scalar builtins (`abs`/`min`/`max`/`to_int`/`sign`), each returning something
      the genuine builtin never would, so a wrong dispatch cannot coincide with the right
      answer. It also covers `i64::MAX` wrapping inside the callee, nested calls, a
      tail-recursive callee, callees the i64 spec declines (`/`, a float literal, non-tail
      recursion), `Float` arguments, an empty and a data-array source, a nested build, and
      an engagement assertion.

      Honest note on one guard: requiring every argument to type `Int` is defence in depth
      rather than the only line. Removing it does not produce a wrong answer — the `f64`
      value reaches an `i64` call signature, Cranelift refuses the function, and the kernel
      declines (verified). It is kept because the alternative is constructing ill-typed IR
      and relying on the builder to reject it, and a builder that panics rather than erroring
      would breach ADR-0024's never-abort guarantee.

- [x] **Stage 3q — a `filter` predicate may CAPTURE: 55–119×.** Found by sweeping the
      constructs the map family's fixes had not touched. `xs.filter(it > k)` fell to the
      bytecode loop while the identical `xs.filter(it > 5000000)` ran natively — the same
      swap-a-literal-for-a-variable cliff, in the one place still carrying it. The filter
      kernel had no `caps` pointer at all (`define_array_kernel` gave one only to map), and
      `filter_kernel_eligible` used the capture-*rejecting* `cond_eligible`.

      MEASURED (10M elements; a declining JIT runs the bytecode loop, so the VM column is the
      before-number):

      | predicate | before | after |
      | --- | --- | --- |
      | `it > k` | 0.55s | **0.01s** (55×) |
      | `it * c > 9000000` | 0.68s | **0.01s** (68×) |
      | `it > lo and it < hi` | 1.19s | **0.01s** (119×) |
      | `it % 7 == 0 and it > k` | 0.60s | **0.02s** (30×) |

      The capture-collecting condition analysis (`cond_eligible_cap`) already existed for the
      fused path; `filter_kernel_eligible` simply was not using it. The filter kernel now takes
      the caps pointer as its 4th parameter, exactly as map does — the only remaining signature
      difference is that filter also RETURNS the kept count.

      NOT fixed, and deliberately: `filter(it % k == 0)` with a VARIABLE divisor stays on the
      VM. That exclusion is about `%`, not captures — a non-literal divisor could be `0`, which
      must raise, and a negative one has sign subtleties. The first sweep conflated the two,
      which is why the original measurement showed no improvement at all; the corrected probe
      isolates a capture that is not a divisor.

      A capturing predicate declines in a FUSED pipeline (which has no caps slice) and falls to
      this standalone kernel instead — both fusion call sites now require an empty capture list
      explicitly.

      The failure mode of a compacting loop is a wrong output OFFSET, so
      `a_filter_predicate_may_capture_and_declines_a_non_int_capture` reads chunk-boundary
      elements individually across `PAR_MATH_THRESHOLD` and includes predicates keeping roughly
      one element per chunk. The non-`Int`-capture declines are load-bearing too: they are what
      exercises popping the captures off the stack and falling through, where a stack-discipline
      mistake would surface.

- [x] **Stage 3r — an `f64` reduce may CAPTURE a scalar and CALL a user function: 48–134×.**
      Two more inversions from the same sweep, sharing one root cause: the f64 reduce
      analyses refused both, while their i64 twins had always accepted them.

      MEASURED (10M elements; the VM column is the before-number):

      | body | before | after |
      | --- | --- | --- |
      | `s + to_float(i) * c` — captured coefficient | 0.78s | **0.01s** (75–134×) |
      | `s + to_float(f(i))` — a call, no captures | 0.74s | **0.02s** (48×) |
      | `s + to_float(f(i)) * c` — both | 0.80s | **0.02s** (54×) |

      TWO gates had to learn it, and they are selected separately — a body WITH captures takes
      the indexed analysis (`infer_f64_indexed`), a capture-FREE one takes
      `infer_reduce_f64_kind`. Fixing only the first left call-only bodies still on the VM,
      which the probe caught; both now carry the same user-call arm, and the codegen they
      share (`gen_f64_typed`) grew the matching emit.

      The captured case was one guard: `reduce_jit_f64_range_captures` required
      `caps.iter().any(|c| c.kind == ArrayF64)` — "at least one array capture" — so a body
      whose only capture was a SCALAR matched neither that path nor the capture-free one. It
      is now `!caps.is_empty()`, which still lets an empty list fall through to the
      capture-free path exactly as before, so this admits only shapes that previously had no
      kernel. The build re-gate carried the identical guard and was relaxed with it; those two
      must always move together or the build declines a kernel the compiler emitted.

      Promotion is the correctness crux and is unchanged: a value scalar rides as `f64` but
      may be `Int` at runtime, so `mix_combine` admits it only where a genuine float promotes
      it. The test pins both sides of 2^53, where `i64` and `f64` genuinely differ, and
      re-checks the two shapes this must NOT disturb — the Stage 3h dot product and the
      dividing body whose kernel carries a poison out-param.

      One refactor came with it, not a suppression: `infer_f64_indexed` reached 8 arguments
      and tripped clippy's limit. Its three parallel OUTPUTS (`caps`, `synth`, `bounds`) are
      always built, passed and consumed together, so they are now one `IndexedOut` — 8
      arguments down to 6, and clippy back to its 2-warning baseline.

- [x] **Stage 3u — the Int-ROOTED mixed map: 105–423×.** An `i64`-out body through Float
      intermediates (`to_int(to_float(i) * 1.5)`) had NO kernel shape at all — recorded in
      the handoff as 4.05s JIT against 4.01s VM, i.e. silently interpreted. The new
      specialization ("mapmi") types the body per node exactly as the f64-rooted mixed
      kernel does, but with root `Int` — and because it reads `i64` and writes `i64`, its
      ABI is the plain i64 kernel's, so it rides the same FFI wrappers, the same dispatch
      marshalling (`Pick::I64`), and the same dead-buffer in-place reuse for free.
      `define_array_kernels`' `mixed: bool` became `mixed_root: Option<NumKind>`; the build
      gate requires `map_kernel_captures` to have REJECTED the body, so an i64-closed body
      is never double-compiled. Measured: `to_int(to_float(i)*1.5)` 167×, with a capture
      423×, `sign(to_float(i)-5e6)` 105×, with a user call 131×. 21-case battery: to_int
      saturation at ±huge and NaN, 2^53 spacing, shadowed `to_int` dispatching to the user
      fn, chains in both directions, declines.

- [x] **Stage 3v — the RAISING rounders compile, behind a poison out-param: 32–58×.**
      `floor`/`ceil`/`round`/`trunc` raise when their result leaves i64 range, and a kernel
      cannot raise mid-loop — the mechanism that kept them (and 17 of 22 numeric builtins,
      per the audit) blocking the JIT. The retrofit: `ArrayKernel.raises` (set by
      `map_body_raises` at compile, re-derived at build as a drift guard — it decides the
      SIGNATURE), a 5th poison out-cell param on raising mixed kernels, an accumulator the
      rounder arm ORs into, and serial poison FFI wrappers whose `None` falls through to the
      bytecode loop for the exact interpreter error. The range wrapper stops at the first
      poisoned chunk.

      THE TWO SABOTAGE-PROVEN CRUXES:
      * `round` is HALF-AWAY-FROM-ZERO. Cranelift's `nearest` is round-to-nearest-EVEN —
        sabotaging to it turns `[1, 2, 3, 4]` into `[0, 2, 2, 4]` on the tie battery. The
        textbook `trunc(x + copysign(0.5, x))` is also wrong: for x = 0.49999999999999994
        (the largest f64 below 0.5) the add rounds up to 1.0. The exact lowering is
        `t = trunc(x); |x − t| ≥ 0.5 ? t + copysign(1, x) : t` — the subtraction is exact
        below 2^52 and the fraction is exactly 0 above it. The range check is the
        interpreter's `round_to_i64` verbatim: accept iff rounded ∈ [−2^63, 2^63), half-open,
        NaN/±inf rejected by the comparisons themselves; the conversion is
        `fcvt_to_sint_sat` because a plain `fcvt_to_sint` TRAPS.
      * A raising kernel must NEVER take the in-place buffer reuse. Sabotaging the `!raises`
        guard crashes outright — the 4-param in-place runner calls a 5-param kernel — and
        even with matched ABI, a poison after mutating the source would corrupt the
        fall-back's input. The guard lives at the dispatch site next to the `Rc::get_mut`.

      Measured (10M elements): `round(to_float(i)*0.5)` 32×, `floor` 46×, `trunc`+capture
      58×, Float-rooted `to_float(round(…))*2.0` 45×, with a user call 45×. Raise cases
      verified for EXACT error text on all three engines from a range source, a chained dead
      intermediate, NaN, inf, and one element past the boundary — with exactly-representable
      `i64::MIN` accepted. NOT yet admitted: `clamp` (runtime-typed mixing like `min`/`max`,
      plus a second raise condition — needs its own design), `/` inside mixed bodies (the
      pre-existing division exclusion, which is why `ceil(x / 4.0)` still declines — use
      `* 0.25`), and rounders in INDEXED mixed bodies or reduce bodies (the analyses there
      have no rounder arm yet; same mechanism would apply).

- [ ] Stage 3c — widen further: `Mod`/`Pow`, `and`/`or` in conditions,
      forward-referenced mutual recursion (two-pass bytecode function registration),
      then array/loop kernels (the bridge to Track C).
- [x] **Stage 3d — map-side `arr[i]`.** A `reduce` body could read a captured array;
      a `map` body could not, so ONE missing arm sent the whole map to the per-element
      VM loop. It was the largest measured JIT gap.

      MEASURED (2026-07-17, `target/gate`, n=20M, min of 2, same binary):

      | body | JIT before | JIT after | `HELIX_NOJIT=1` |
      | --- | --- | --- | --- |
      | `(0..n).map(i => i*2+1)` | 0.03s | 0.03s | 1.01s |
      | `(0..n).map(i => a[i]+1)` | 1.31s | **0.06s** | 2.33s |
      | `(0..n).reduce(0, (s,i) => s+a[i])` | 0.03s | 0.03s | 2.16s |

      Before, the indexed map cost ~1.28s under BOTH engines (1.31 − 0.03 to build
      `a`; 2.33 − 1.01) — it never JITed, while the identical read on the reduce side
      was native. Now it runs native, landing at the unindexed map's cost: **~22× on
      the shape**. It was never the Cranelift ceiling.

      Nor was it a codegen gap. `gen_value`'s `Expr::Index` arm already emitted the
      load and is shared by both kinds; the map kernel already hoisted capture loads
      off a `caps` pointer as `I64` slots, and an array base IS such a slot. What was
      missing: eligibility (`map_kernel_captures_indexed`, which reuses the reduce's
      `value_eligible_cap_indexed` by passing the map's binder as `pb`), and
      `ArrayKernel` carrying `Vec<Capture>` + `Vec<IndexBound>` instead of names-only
      `Vec<String>`.

      **SAFETY — the part that was not mechanical.** A reduce's `pb` is the loop
      COUNTER, so `IndexBound::Counter` discharges the whole access set with a
      two-endpoint check. A map's binder is an ELEMENT VALUE: in `xs.map(x => a[x])`
      the index is arbitrary data, unprovable without an O(n) scan, and `x` may be
      NEGATIVE — which the interpreter Python-WRAPS rather than rejecting, so even an
      O(n) `min >= 0` scan would reject legal programs. Map-side `Index` is therefore
      admitted ONLY when the receiver is a lazy `ArrayData::Range`, whose elements are
      exactly `start + step*j` over `j ∈ [0,len)` — monotone in `j`, so the two
      endpoints bound the whole access set (computed in `i128`, so the check itself
      cannot overflow). `map_index_caps` (`src/vm.rs`) discharges it, and the range
      shape is read BEFORE `densify_range_top` — densifying erases the range-ness the
      proof depends on, after which a range is indistinguishable from any other `Ints`
      buffer. A gather stays on the VM loop BY DESIGN (measured 2.26s: excluded, not
      accidentally slow).

      Pinned by `map_index_bounds_agree_across_engines_at_every_boundary` (an
      EXHAUSTIVE `len × start × end` sweep rather than a fuzzer — endpoint bugs live at
      `end == len+1` and `start == -1`, which random generation reaches only by luck),
      `indexed_map_engages_the_native_kernel_only_over_a_range_source` (three engines
      agreeing prove nothing if the kernel never ran), and
      `tests/corpus/j1_map_index_bounds.helix`. Each of the four checks in
      `map_index_caps` was verified load-bearing by sabotage: delete any one and the
      sweep goes red.

      NEXT LEVERS, measured after this landed (n=5M, min-of-4, same binary — a single
      cold run reads ~6× slow from JIT compile + cold cache, so min-of-N is not
      optional here):

      | shape | JIT speedup | why |
      | --- | --- | --- |
      | i64 `a[i]` gather + reduce | **3.7×** | this change |
      | f64 `a[i]` | ~~1.5×~~ → **done, below** | was: f64 indexed map declined |
      | affine i64 `a[2*i]` | ~1.6× | map analysis emits no `Affine` bound (reduce-only) |
      | matmul `_maptemp` (f64 **and** affine) | ~1.0× | needs affine + fused captures |

- [x] **Stage 3e — f64 map-side `a[i]`: the MIXED kernel reads f64 arrays.** The
      vector-add / AXPY / gather-transform shape `(0..n).map(i => a[i] + b[i])` over
      `Floats` arrays now runs native. MEASURED (n=5M, min-of-4, same binary):
      vector-add **5.9×** (was 1.8×), AXPY **6.0×**, scale-by-constant **4.2×**, the
      isolated map **32×**. (The composed `map→reduce` still materializes the
      intermediate the reduce-side spelling avoids — its 51× belongs to fusion, a
      separate lever.)

      Design: ONE stored `ArrayKernel` now carries TWO specializations of the same
      indexed body — the i64 build (caps marshaled from `Ints`, I64 loads) and the
      mixed build (caps from `Floats`, F64 loads, f64 result) — because the compile
      time analysis cannot know a captured array's element type. A dual-typed body
      like `a[i] + 1` is admitted by BOTH analyses (`map_kernel_captures_indexed`
      types the load Int; the new `mixed_map_captures_indexed` types it Float), both
      record identical captures/bounds, and the VM routes by the runtime
      representation. The bounds discharge is unchanged — the index arithmetic is
      `i64` in both, so `map_index_caps` gained only a `float_arrays` flag.

      **THE NEW HAZARD — type confusion — and why it cannot happen.** The i64
      version's failure mode was an out-of-bounds read; this version adds a worse
      one: an `Ints` buffer reaching an F64 load reinterprets the bits as denormal
      junk (~5e-323 where 20.0 belongs) — no crash, no error, silently wrong
      science. The marshal's match-on-representation IS the guard: it declines
      before any pointer is formed, and the dispatch falls back to the other
      specialization or the checked loop. Sabotage-verified: forcing the marshal to
      accept `Ints` under float mode makes
      `f64_map_index_agrees_and_routes_by_representation` fail with exactly that
      denormal signature, pinned against literal expected values (routing probes:
      float body over Ints falls back; dual-typed body over Ints stays int, over
      Floats goes f64; mixed representations decline; a Float SCALAR cap declines —
      scalar caps are typed `i64`). Also pinned: a boundary sweep over float arrays
      with an engagement assertion, `tests/corpus/j2_map_index_f64.helix`, and a
      1600-program random fuzz that randomizes the array REPRESENTATIONS per capture
      (so the routing itself is fuzzed), 0 divergences.

      The f64-SOURCE map (`xs.map(x => a[x])` where `xs` is a `Floats` array) stays
      excluded PERMANENTLY, not as a gap: its binder is an element value — the
      gather shape whose bounds are undischargeable (Stage 3d's safety argument).

- [x] **Stage 3f — affine map indices.** `a[2*i]`, `a[i + off]`, and the matmul
      row/column reads `a[i*n + k]` / `b[k*n + j]` now run native in the mixed
      kernel — the missing piece for the naive-comprehension matmul. MEASURED: the
      maptemp inner-loop miniature (n=300) **1.0× → 4.9×**; the real
      `k9_matmul_naive_maptemp` at n=512 **~25s → 6.0s** (was 55× over the naive
      reduce spelling, now ~10×). The naive `reduce` spelling is still the faithful
      port at 1.2× C — maptemp is natural-Helix ergonomics, and it is no longer a
      cliff.

      Reuses the reduce's affine machinery verbatim: `infer_mixed_kind_indexed`'s
      `Index` arm calls `index_scalars_eligible` + `affine_split` (the map binder as
      the counter, the empty string as the absent `pa`), `base`/`coef` land as
      Scalar caps — bare idents reuse the body's own, compound terms like `i*n` get
      a synthetic `$aff{k}` slot the compile site evaluates once (counter-free
      `+ - *`, so side-effect-free — the same argument the reduce site makes).

      **SAFETY — a stronger overflow story than the reduce's.** The index composes
      with the source's step: `idx = base + coef*(start + step*j)`, affine∘affine,
      monotone in `j`, so the two ENDPOINT indices bound the set. The reduce's
      affine bounds a counter ≤ 2^63 (products fit in i128); here the composed
      magnitude `coef*(start + step*j)` can exceed even i128 — so `map_index_caps`
      computes it in **CHECKED i128** (`checked_mul`/`checked_add`), and an overflow
      DECLINES exactly as an out-of-range endpoint does (a value that large is
      outside `[0,len)` anyway). The kernel then evaluates the original index in
      wrapping i64 over the materialized element; mod-2^64 is a ring homomorphism,
      so the wrapped index equals the true i128 value precisely when that value is
      in `[0,len) ⊂ [0,2^63)` — exactly the checked set.

      Pinned by `affine_map_index_agrees_at_every_boundary` (an exhaustive
      stride × offset × range × length sweep + engagement + the matmul reads +
      stepped-range composition + the overflow-declines-with-the-exact-error probe)
      and `tests/corpus/j3_map_index_affine.helix`. Both endpoint checks
      sabotage-proven load-bearing. (The sweep's first version spelled negatives as
      `-1` literals — a PARSE error, so every `lo<0` corner was a vacuous
      `(Err,Err)`; caught because sabotaging the lower endpoint FAILED to turn it
      red. Negatives now spell as `(0 - k)` arithmetic. The mirror of the same
      print-wrap/parse-error trap that recurs across this codebase.)

      Remaining: **FusedKernel captures+bounds** — `(0..n).map(k => …).reduce(…)`
      still materializes the inner array per (i,j); fusing it away is the last step
      to bring maptemp to the naive spelling's cost.
- [x] **Stage 3g — float scalar captures in the mixed kernel (SAXPY).** Found by an
      honest AXPY-vs-C measurement, now closed: `(0..n).map(i => a * x[i] + y[i])`
      with a runtime float coefficient `a` runs native (was 0.56s → **0.03s**, the
      same as with a float literal; the isolated `a*x[i]+y[i]` map is **~10× faster**
      than the VM). SAXPY/AXPY with a runtime coefficient is *the* canonical BLAS-1
      op — the flagship numeric gap.

      THE DESIGN (it was not a one-liner). `a * x[i]` is admitted by BOTH the i64
      analysis (`a` an `i64` Scalar, `x` `Ints`) and the mixed analysis (`a` float,
      `x` `Floats`), so both must emit the same capture KIND. A new
      representation-agnostic `CaptureKind::ScalarValue` — marshaled `i64` in the i64
      spec, `f64` in the mixed spec, exactly as `ArrayI64` array caps route by
      representation — is emitted by `relabel_value_scalars` in the two MAP wrappers
      (a non-index Scalar becomes ScalarValue); the reduce path does NOT relabel, so
      `value_eligible_cap_indexed` and every reduce/nested-reduce site is
      byte-unchanged (all 34 reduce tests pass). An INDEX scalar (`a[k]`, an affine
      `base`/`coef`) stays `Scalar`: an index is an integer, correct in both specs.

      BIT-IDENTITY — the real subtlety. The interpreter evaluates in `i64` until the
      first float, then promotes. A `ScalarValue` rides as `f64` in the kernel but is
      possibly-`Int` at runtime, so it is bit-identical ONLY where the interpreter
      ALSO promotes it. `infer_mixed_kind_indexed` now carries a three-valued `MixT`
      { Int, GFloat (a genuine array/literal float), SFloat (a value scalar) }: a
      value scalar is admitted only once a `GFloat` promotes it. `a * x[i]` is safe
      (`SFloat * GFloat`); `a * i`, `a + b`, `abs(a)` are REJECTED (the interpreter
      does `i64`, the kernel `f64` — diverging past 2^53). Proven load-bearing by
      sabotage: forcing `combine()` to accept `(SFloat, Int)` makes
      `(2^53+1) * 3 + x[i]` compute `…976.0` on the JIT vs the interpreter's correct
      `…980.0` — a real 4-ULP divergence the `MixT` decline prevents.

      Pinned by `saxpy_float_scalar_caps_route_and_decline_correctly` (engaging
      shapes to literal values + the divergence-decline cases at 2^53+1 + routing
      probes) and `tests/corpus/j4_map_index_saxpy.helix`; 1750 fuzzed programs
      randomizing scalar/array representations, 0 divergences.

      HONEST CEILING: a functional-immutable AXPY still builds a NEW vector where C's
      BLAS mutates `y` in place, so the whole program is allocation-bound (Helix wins
      on memory-bound *reduces*, k1 dot beats C, not on produce-a-new-vector — the k7
      lesson). A k10 BLAS-1 benchmark should be added and say so; the kernel engaging
      is necessary, not sufficient.

- [x] **Stage 3h — value scalars in the f64 indexed REDUCE.** Found by measuring the
      map-vs-reduce spellings side by side: `map(i => c*a[i]+b[i]).reduce(…)` came out
      **faster** than the direct `reduce(0.0, (s,i) => s + c*a[i]+b[i])` — impossible,
      since the reduce allocates nothing. `infer_f64_indexed`'s `Ident` arm returned
      `None` for any free var ("only indexed array caps allowed", a documented v1b
      limit), so a coefficient of EITHER type sent the whole reduce to the VM.
      MEASURED (n=5M, min-of-3): float coefficient **0.75s → 0.03s**, int coefficient
      **0.76s → 0.02s** (~30×). This is the *allocation-free* spelling — the faithful
      port that beats C on k1 dot — so it matters more than the map side.

      Simpler than the map case: this kernel is monomorphically `f64` (a `Float` init
      picked it), so there is no representation routing — a value scalar always rides
      `f64`. What carries over is the bit-identity rule, and it is now literally the
      same code: `mix_combine` was hoisted out of the mixed-map analysis and is shared
      by both, so one proven rule guards both sites. `infer_f64_indexed` returns
      [`MixT`]; the accumulator and array loads are `GFloat`, the counter and index
      scalars `Int`, a free value scalar `SFloat` — admitted only where a genuine float
      promotes it. INDEX scalars (a point index, an affine `base`/`coef`, and names
      inside a synthetic `$aff` term) stay `Scalar`/`i64` via `relabel_value_scalars`.

      Proven load-bearing AT THIS SITE (a shared rule still needs proving wherever it
      guards): sabotaging `mix_combine` makes `s + (2^53+1)*3 + a[i]` compute
      `…928.0` on the JIT vs the interpreter's correct `…944.0`; restoring makes them
      agree. Pinned by `f64_reduce_value_scalars_promote_or_decline` (engaging shapes
      to literal values with an engagement assertion + the 2^53+1 decline cases +
      index-scalar cases incl. compound affine) and
      `tests/corpus/j5_reduce_value_scalars.helix`; 1750 fuzzed programs across 5
      seeds, 0 divergences; the reduce oracles
      (`differential_f64_dot_product_reduce_jit`,
      `differential_indexed_reduce_oob_fallback`,
      `parallel_indexed_nested_reduce_matches_tree_walker`,
      `multiacc_reduce_matches_across_k_boundary`) all still pass.
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
- [x] **A `helix` CLI** — `helix run` / `eval` / `repl` / `build` / `emit-hbc` /
      `version` / `help` (plus the `helix <script.helix>` shorthand), replacing `cargo run`.
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
- [x] **Tail-call optimization (VM)** — a `CallFn` in tail position compiles to
      `Op::TailCallFn` (via a `tco_peephole` pass) that **reuses the current frame**
      instead of pushing, so tail recursion runs in **constant stack space**. This is
      what lets an intentional forever-loop (a server accept loop, an event loop, an
      accumulating fold) hold flat memory instead of growing the frame `Vec` until OOM
      — the event-loop server ([ADR 0022](adr/0022-http-version-roadmap.md)) relies on
      it (flat 16 MB). Parity-tested (`deep_tail_recursion`,
      `tail_calls_match_tree_walker_on_vm`). See
      [execution-engine.md](execution-engine.md) and [audit.md](audit.md).
- [x] **Adversarial audit, round 2** (2026-07-10) — a second hunt (strings/format,
      comprehensions/closures, DataFrame, genomics, module system) found and fixed
      five more issues, each verified under both engines with regression tests:
      - **`module::locate` line-0 underflow** — a position-free error (e.g. a
        format-spec failure carrying line 0) hit `line - start_line` and **panicked
        the host** under overflow checks (garbage location in release). Now saturates
        (ADR 0024's "no host panics" property; the actual format error now renders).
      - **`reduce`/`scan` with a duplicate binder `(a, a)`** deleted a same-named
        *outer* binding in the tree-walker (double `remove` on the shared entry); the
        outer name is now preserved (VM was already correct).
      - **A closure capturing a binder that shadows a same-named global** read the
        *global* in the tree-walker instead of the lexical binder; capture is now by
        current binding (immutable globals snapshot, mutable globals stay live —
        matching the VM's local→upvalue→global resolution).
      - **Radix format specs (`{x:x}`/`b`/`o`) on negatives** printed Rust's 64-bit
        two's-complement (`ffffffffffffff01`) instead of Python-style sign-magnitude
        (`-ff`); a **string precision** (`{s:.3s}`) was ignored. Both now match the
        documented Python mini-language.
      Deferred (need a design decision or are by-design — see backlog below):
      zero-param comprehension lambdas (`[].map(() => 5)`), the `where`-vs-`filter`
      error wording, and DataFrame `@a / 0` → `inf` / `@a % 0` → `missing`.
- [x] **Total runtime: user input never aborts the host** (2026-07-10, [ADR 0024](adr/0024-total-runtime-no-host-panics.md)) —
      an adversarial audit found and fixed four reachable aborts/wrong answers, each
      with a regression test and verified across engines (differential oracle green):
      - `i64::MIN // -1` / `i64::MIN % -1` **panicked the host in every build mode**
        (`div_euclid`/`rem_euclid` are always-checked overflow). Now wrap
        (`wrapping_*_euclid`) in **both** the tree-walker (`interp/ops.rs`) and the VM
        (`vm.rs`), matching the `arith` path's wrapping policy. The JIT was already
        safe (it compiles `//`/`%` only by a positive constant divisor).
      - `.sort()`/`.argsort()` with a `NaN` element **aborted** (the comparator
        returned `Equal` for NaN — intransitive, and Rust's sort panics on a
        non-total order). `numeric_cmp` now uses `f64::total_cmp` (NaN sorts to a
        consistent extreme, as numpy does).
      - `round(x, d)` with `d ≥ 2^31` silently returned `NaN` (`as i32` wrap
        underflowed the scale to 0). Digit count now clamped to f64's exponent span;
        a scaled overflow is a no-op.
      - `argmax`/`argmin` raised a type error on `missing` and silently skipped
        `NaN`; they now propagate `missing` like every other aggregation (ADR 0001).
- [x] **Test suite runs on a stock environment** (2026-07-10) — the tree-walker
      recurses on the *native* stack, and three recursion-heavy tests overflowed
      cargo's default ~2 MiB test-thread stack, SIGABRT-ing the whole binary and
      masking every later test. `run_tw` (vm/tests.rs) and `no_reference_leaks`
      (interp/tests.rs) now run on a 2 GiB thread, matching production's
      `run_on_big_stack`; `cargo test --bin helix` passes 342/342 with no env-var
      workaround.
- [x] **Recursion parity across engines** (2026-07-16/17, #81) — one shared
      `MAX_CALL_DEPTH = 20_000` (the VM's old 1M heap-frame budget made
      `s(50000)` print on the VM and error on the walker; off-by-one from the
      `<main>` frame corrected so both trip at the same activation), and the
      walker gained **tail-call optimization** (a trampoline in
      `call_function` + `eval_tail`) for exactly the shapes the VM's
      `CallFn`→`TailCallFn` peephole optimizes — keyed on the callee's
      *declared* name, so immutable-global aliases dispatch dynamically on
      both engines. Deep tail recursion is constant-depth everywhere;
      non-tail recursion errors identically at 20k; an infinite tail
      recursion spins (`while true` semantics) everywhere.
- [x] **Walker lexical scoping — the flat-env dynamic-scope divergence**
      (2026-07-17, found by #81's adversarial review) — the walker's single
      flat env let a CALLEE resolve free names to its CALLER's params/lets
      (`x = 10; fn callee() = x; fn caller(x) = callee() + 0` → walker 42,
      VM 10 — dynamic scoping, wrong values on legal programs). Locals now
      live in a per-frame map swapped wholesale at each call boundary
      (`mem::take`, cheaper than the old per-name save/restore); globals in
      their own map; resolution is locals-then-globals, the VM's
      local→upvalue→`LoadGlobal` order. Also fixed on the way: `let`
      initializer errors leaking installed bindings past `try`, and
      rebinding a `fn`-declared name (now an error on both engines — the
      VM binds `CallFn` targets at compile time and could never honor it).
- [ ] Iterative/trampolined evaluation of *non-tail* recursion to remove the
      native-stack depth limit entirely (only if needed — the shared 20k budget
      is the language contract now).

### Future work — correctness backlog
- [x] **Extend the differential fuzzer's literal pool** (2026-07-10) — `gen_expr`'s
      binary-op arm now draws both operands from an adversarial i64 edge pool
      (`i64::MIN`, `i64::MAX`, `-1`, `±(2^53+1)`, small consts) 1/4 of the time, so
      `MIN // -1`, `MIN % -1`, and 2^53-boundary comparisons are routine fuzzer
      traffic (they had ~1e-9 grammar probability before). Differential oracle green.
- [x] **Zero-parameter comprehension lambdas — DECIDED: reject** (2026-07-17).
      `xs.map(() => 5)` ignores every element, which is a bug rather than a
      constant-map, so both engines now reject it **before iterating** with the
      identical message (`comp_needs_binder`, shared by the walker's map /
      filter / where / any / all; `reduce` already had its own exact-two check).
      The walker previously had no check at all and only noticed when the
      *destructure* failed — so `xs.map(() => 5)` **succeeded on an empty `xs`**
      (returning `[]`, the lambda never invoked) and failed with a different
      message once `xs` had data: a value-vs-error divergence from the VM, and a
      bug that ships green and detonates on real input. The decision follows from
      that asymmetry — a rejection must not depend on whether any element exists.
      Pinned tri-engine (`zero_param_comprehension_lambda_rejects_on_both_engines`,
      plus `tests/corpus/z1_zeroparam`). The differential fuzzers never generated
      `() =>` inside a comprehension, which is why this survived so long.
- [ ] **Thread the invoked method name into `where`/`filter` runtime errors** — a
      non-bool `where` predicate reports "`filter` expects a yes/no test" in the VM
      (`Op::CompFilterPush` hard-codes `filter`; the compiler routes `where` through
      `CompKind::Filter`). Both engines correctly *error*; only the name in the
      message diverges. Carry the surface method name into the op/error.
- [ ] **Decide DataFrame divide/modulo-by-zero semantics** — inside a column
      expression, `@a / 0` yields `inf` and `@a % 0` yields `missing` (Polars/IEEE
      columnar semantics), whereas scalar Helix *raises* "division by zero". Both
      engines agree (it's the backend), so it's not an oracle bug — but it's a
      scalar-vs-columnar inconsistency to either document as intended (SQL-like) or
      intercept in the verb lowering.
- [x] **BAM quality-byte overflow fixed** (2026-07-10) — `render_quality`
      (`src/sam.rs`) did `(s + 33) as char` on a raw `u8`; a malformed BAM with a
      quality byte `>= 223` overflowed `u8` (panic in debug, wrong char in release).
      The score is now clamped to the SAM-valid `0..=93` before the `+ 33` (the doc
      comment already assumed that range). Text SAM couldn't reach it; only `read_bam`.
- [x] **Panic-free audit, round 3** (2026-07-17) — the surfaces this list named:
      Dict operations, the genomics parsers (VCF/GFF/BED/FASTA/FASTQ/SAM) on
      malformed files, unicode byte-vs-char slicing, module-system cycles, plus
      malformed JSON/CSV and numeric conversion edges. **~60 adversarial probes,
      zero panics** — every one produced a value or a clean Helix error. Import
      cycles report `import cycle detected`; binary garbage, truncated records,
      non-numeric fields, negative coordinates, and `i64::MAX` slice bounds all
      error cleanly. (Probes were validated against positive controls first — a
      "clean" result from a program that never ran is worthless.)
      **But the audit found a WRONG-ANSWER bug instead of a crash**, which is
      worse: `read_fasta`/`read_fastq` uppercased without validating, minting
      `Dna` values that `dna()` itself rejects. A `>s1 / ATGCXXZZ!!` record gave
      `gc_content() = 0.2` (counted over the garbage) and `kmers(3) = ["ATG",
      "TGC"]` — 2 k-mers where a 10-base sequence must yield 8, the rest silently
      dropped — and the value could not round-trip through `dna()`. A scientist
      reading a corrupt FASTA got a believable GC number and no warning. Both
      readers now enforce `dna()`'s exact rule at the boundary (uppercase; ACGTN
      + IUPAC `R Y S W K M B D H V`), erroring with the record id and position;
      lowercase soft-masking, `N`, and ambiguity codes still read normally.
      Pinned: `fasta_enforces_the_dna_invariant_at_the_boundary`.
- [x] **A gate for new `unwrap`/`expect` in interpreter paths** so the never-abort
      property is enforced by CI, not re-audited by hand. Shipped as a per-file
      **ratchet** (`no_new_panicking_calls_on_user_reachable_paths`), not the
      `#![deny]` this entry originally asked for: the ~90 existing calls are
      proven-by-construction, not sloppiness — 38 of `vm.rs`'s are
      `stack.pop().unwrap()`, sound because the compiler emits balanced code, and
      the rest are guarded (`get_mut(name).unwrap()` after a contains-check) or
      genuine invariants (`expect("same length as source tensor")`). A blanket
      deny would have meant ~90 `#[allow]`s: churn that buys no safety and trains
      reviewers to wave the attribute through. What matters is that the count
      cannot silently *grow*. The budget fails in both directions — a new call
      must be justified by raising the number in the same commit, and removing one
      forces the budget down so it cannot creep back. Verified by sabotage (add a
      call → fails `+1`; remove one → fails `lower it to 0`), because a gate you
      have not watched reject something is not known to be a gate.
- [ ] **Document NaN ordering** in the language docs (`sort` places NaN after
      `+inf`; reductions propagate `missing`) so the behavior is a contract, not an
      implementation detail.
- [ ] **`try`-on-VM error-recovery soak** — `TryBegin`/`TryOk`/`TryErr` unwinding
      under the fuzzer, composed with JIT bailouts mid-`try`.

- [x] **Stage 3y — a map body may CALL a `Float`-parameter function: 2.09s → 0.06s.** The
      last shape where naming something cost two orders of magnitude. Same body, 20M
      elements: inline 0.02s, behind `fn g(x: Float) = …` **2.09s** — annotated or inferred.

      This needed more than Stages 3p/3r's call work. There is deliberately no standalone
      `f64` specialization of a user function (see the note above `let kind = NumKind::Int`
      in `build`): a float-argument function can still return an `Int`, so `f64`-monomorphic
      codegen would diverge on RESULT TYPE. The only thing callable is the MIXED
      specialization, whose ABI is all-`i64` bit slots plus a trailing poison pointer.

      Three pieces:
      * `MixedSig` lost its `FuncId` (ids moved to a parallel `mixed_ids` map), which makes
        signature inference module-free — so `mixed_fn_sigs(program)` is now a pure AST
        function, the twin of `int_eligible_fns`, and the bytecode compiler holds the table
        it needs to type a call. Pinned by
        `the_pure_mixed_sig_table_matches_what_the_jit_specializes`, which checks it names
        exactly what the JIT specializes — including the unannotated tail-recursive case
        (Stage 3j) and excluding the all-`Int` one (the i64 spec's territory).
      * `infer_mixed_kind` gained a mixed-call arm: argument kinds must EQUAL the callee's
        parameter kinds (no promoting at the boundary — the same strict rule
        `infer_typed_env` uses for a mixed sibling).
      * `gen_value_typed` marshals `Float` args to bits, hands the callee a stack CELL as
        its poison out-param, folds that cell into the kernel's accumulator, and bitcasts a
        `Float` result back.

      THE CRUX, AND A BUG CAUGHT ONLY BY THE RAISE CASES: a callee's bail must poison the
      whole map. `map_body_raises` scanned only for rounders and division *in the body*, so
      a body that merely CALLS a mixed function was built poison-free, the VM took the
      non-poison wrapper, and a raising callee was silently swallowed — `[0.0, 0.0, …]`
      where the other two engines raised "division by zero". A call to any mixed
      specialization now counts as raising, since its ABI carries a poison pointer precisely
      because it can bail. All four raise cases (a `/0` at every element, at one element, a
      NaN comparison, a rounder out of range) failed that way before the fix.

      Also fixed: the poison cell is read through its ADDRESS rather than via
      `stack_load`/`stack_store`. The callee writes through the pointer, which slot
      promotion cannot see, so slot-relative accesses can be folded away as "loads what was
      stored" — zero.

      HONEST COST: the callee path is **0.06s against the inline twin's 0.02s**, because any
      mixed callee can bail, so the kernel is "raising" and the poison FFI wrappers are
      SERIAL. Parallelizing them needs per-chunk poison cells (sound — a map has no
      cross-element dependency and poison is a monotonic flag); recorded below as the next
      lever, and it would lift the rounder kernels from Stage 3v too.

- [x] **Stage 3z — the poison FFI wrappers run parallel.** Every RAISING kernel (the Stage
      3v rounders, dividing bodies, the Stage 3y mixed callees) had been giving up the
      chunked-parallel path, because `run_map_poison` / `run_map_range_poison` were serial.
      Now each chunk carries its own poison cell and they are reduced with `|`.

      MEASURED at 20M elements:

      | shape | before | after | non-raising twin |
      | --- | --- | --- | --- |
      | `fn g(x: Float)` callee | 0.06s | **0.03s** | 0.02s |
      | `round(to_float(i) * 0.5)` | 0.03s | **0.02s** | — |
      | `floor(to_float(i) * 1.5)` | 0.03s | **0.02s** | — |
      | `to_float(i) / d` | 0.03s | **0.02s** | — |

      Sound for the same reason the non-poison map is: chunk *k* reads and writes only its
      own index range, so output is byte-identical to the sequential run, and poison is a
      MONOTONIC flag, so OR-reducing it is order-independent. The one behaviour change is
      that a poisoned run no longer stops at the first bad chunk — harmless, since the whole
      output is discarded either way. The serial path (below `PAR_MATH_THRESHOLD`) keeps its
      early exit.

      A PROBE LESSON, recorded so the test's cases are not "simplified" back into the
      obvious form: a raise targeted with `floor(if i == K then 1e19 else …)` proves NOTHING
      about the reduce. `if` is not in the mixed analysis, so a conditional body declines to
      the bytecode loop and raises correctly however broken the parallel reduce is — three
      such probes passed happily against a reduce hard-wired to return 0. The cases in
      `the_parallel_poison_reduce_never_loses_a_chunks_bail` are all straight-line arithmetic
      that raises only on a chosen index range (`floor(x * 1e14)` leaves i64 range at
      x ≥ 92234, so the plain counter raises late and the reversed one early; a division
      raises at exactly one index), and every one of them fails under that sabotage.

- [x] **Stage 4a — `frequencies()`/`top()` over integers were O(n × distinct): 39.2s → 0.11s.**
      Found by the standing method — compare a program against its own equivalent spelling.
      The SAME 5M histogram over 10k distinct keys ran **0.06s spelled with string keys and
      41.7s spelled with integer keys**, a 600× inversion, because `value_histogram` had a
      hash path for text and a LINEAR SCAN for everything else (2.5e10 `values_equal` calls).
      `unique` had had an all-Int hash path since `range(50_000).unique()` was found to be
      ~1.25 billion comparisons; `frequencies`/`top` never got one.

      A CORRECTNESS FIX rode along, and it is user-visible. The text key was the string's
      bytes alone, so `dna("AT")` and `"AT"` shared a bucket — but `values_equal` has a
      `(Str, Str)` arm and a `(Dna, Dna)` arm and no cross arm, so the pair is FALSE, which
      is what `contains`/`index_of` always reported:

      | before | after |
      | --- | --- |
      | `[dna("AT"), "AT"].index_of("AT")` → `1` | unchanged |
      | `[dna("AT"), "AT"].unique().length()` → `1` | → `2` |

      ADR 0001 names those four as one family answering on `values_equal` identity; now they
      do. Homogeneous text — every k-mer spectrum — is untouched.

      **Why the Int key stops at `Int | missing`.** `values_equal` collapses `1 == 1.0` across
      types, and above 2^53 that collapse is not even TRANSITIVE: `9007199254740993` and
      `…92` both equal `9007199254740992.0` but not each other. No hash key can reproduce
      that, so any array holding a `Float` or `Rational` keeps the scan. `missing` joins the
      key because it is one identity that is never an integer — provably exact. **Float-only
      arrays remain a known gap** (`-0.0` and NaN need care, and NaN is not even reflexive);
      they are the same scan they always were.

      Pinned by `set_like_operations_hash_exactly_the_identities_they_report` (26 cases, plus
      a loop asserting `unique` and `frequencies` report the same identity count for every
      kind mix — they choose keys independently, so that is what catches one being fixed
      without the other). All four semantic guards fail under sabotage; the control —
      dropping `missing` from `unique`'s key, a speed change with no semantic content —
      correctly still passes.

- [x] **Stage 4b — interpolated strings skip the per-string `Vec` and the nested formatter:
      4–8%.** `Op::Interp` was calling `split_off`, minting a `Vec<Value>` per string built,
      and every hole went through `write!(buf, "{}", value)` — two nested `fmt::Arguments`
      dispatches, one to reach `Display for Value` and one for the scalar inside it. Holes are
      now read in place off the value stack (safe: `Handler` records the depth at `try` entry
      and the catch truncates back to it), and `Int`/`Str`/`Dna`/`Bool`/`missing` are appended
      directly.

      MEASURED — interleaved, median child CPU over 21 runs, 5M elements:

      | shape | formatter | direct | change |
      | --- | --- | --- | --- |
      | short ints `"w{n<1e4}"` | 0.630s | 0.578s | **−8.3%** |
      | long ints `"{19 digits}"` | 0.565s | 0.540s | **−4.4%** |
      | k7 wordcount | 0.741s | 0.693s | **−6.5%** |

      The digit loop emits TWO digits per division against a compile-time-built pair table.
      A one-digit loop was tried and REJECTED on the numbers: it beats the pair table on short
      integers (−10.1%) but LOSES to the formatter on 19-digit ones (+5.6%), because std
      formats two digits at a time. The pair table is the only variant never worse than what
      it replaces. Removing `split_off` measured as no change on its own — the allocator
      serves that little Vec from a thread cache — and stays because it is strictly less work.

- [ ] **k7 wordcount is ~80% string construction, and the next lever is `Rc<str>`.**
      Stage-by-stage at 5M (child CPU, median of 21 interleaved runs): `scan` 0.02s, `+ int
      map` ~0s, **`+ string map` 0.61s**, `+ frequencies` 0.75s. Of that 0.59s of string
      building, ~0.22s is the interpreted closure call per element (a body returning a
      *constant* string costs that much) and the rest is formatting and allocation.
      **`Value::Str` is `Rc<String>`, so every word costs an `Rc` box AND a heap buffer** —
      two allocations where one would do. That refactor, not more formatter tuning, is what
      is left: Stage 4b took the 4–8% that was there. k7 is now 3.0× slower than C (0.64s vs
      0.21s) and 2.4× FASTER than CPython.

- [ ] **A stale `target/release` silently poisoned every published ratio, and wall clock
      cannot resolve small effects on this machine.** Two process lessons worth as much as
      the code:

      * `bench/kernels/run.sh` picks up whatever `target/release/helix` exists. The
        2026-07-18 table was taken against a binary that then went four days stale while a
        dozen JIT stages landed — nothing warned, the anchor gate still passed (outputs right,
        just slow), and **k7 stayed published at 7.0× slower than C and "also loses to
        CPython" when it was really 3.0× and beating CPython**, the scan kernel (Stage 3t)
        having fixed it weeks earlier. Rebuild before regenerating.
      * Wall clock here swings ~15% run to run; two honest best-of-3 runs of the SAME binary
        on k7 came out **1.7× apart**, once because a 125-second allocation-heavy probe ran
        immediately before. That fabricated a "1.12s → 0.79s" improvement in `c82c191` which
        did not exist and is corrected in `9f50966`. Small effects need child CPU time,
        variants run INTERLEAVED, and a median — and even then between-session drift is ~7%,
        so only within-session ratios are quotable.

- [x] **Stage 4c — `position`/`take_while`/`drop_while` stopped materializing: 24.3s and
      2.1 GB became 0.02s and 15 MB.** Found the standing way — the same question asked
      four ways over a 90M range: `any` 0.07s/14 MB (already lazy), `position` 5.03s/2.12 GB,
      `take_while` 24.33s/2.12 GB. The last two desugared to `map(p).index_of(Bool(want))`,
      running the predicate over EVERY element and boxing one `Value` each to find an index
      that is almost always near the front.

      `position` is now a first-class comprehension verb in both engines
      (`CompKind::Position`, `Op::CompFindTest { want, idx_slot, short_target }`, and the
      walker arm over `eval_pattern_loop`). `take_while`/`drop_while` keep their desugar
      shape but take the index from `position(p, false)` — a two-argument form the arity
      check in `desugar_position` makes unwritable from source, so the extra parameter
      never reaches the surface (ADR 0003).

      Even with NOTHING skippable the intermediate array is gone: a full-scan `take_while`
      over 10M went 287 MB → 17.7 MB at the same speed.

      The arms reproduce `index_of`'s comparison EXACTLY: `values_equal` is false for every
      non-`Bool` against a `Bool`, so a `missing` or non-boolean result is SKIPPED —
      neither a match nor an error. `[5, 6, 7].position(it)` is `missing`, not a type
      error. Deliberately unlike `any`/`all`, which DO reject a non-boolean test; the
      walker, the VM and the type checker each had to learn the asymmetry.

      TWO INTENDED BEHAVIOUR CHANGES. A predicate that raises PAST the stopping point no
      longer aborts (`[4, 1, 0].take_while(100 // it > 50)` was "integer division by zero",
      is now `[]`) — `any`/`all` always behaved this way, so the four early-exit verbs
      finally agree. And a non-array receiver names the verb the user wrote:
      `5.position(it > 0)` said "type Int has no method `map`", leaking a desugar.
      Verified by differential: 47 shapes on both binaries across all three engines, every
      case identical except that message. Sabotaged nine ways, all caught.

- [x] **Stage 4d — a tail loop may READ A GLOBAL and still compile: 0.80s → 0.01s (80x).**
      Helix has no `for` and no `while`; a tail-self-recursive function IS the loop, and
      the JIT already lowered one to a native loop. It refused for any function that read a
      global, because `value_eligible`'s `Ident` arm is `locals.contains(name)` and
      `locals` is the parameter list. Position was irrelevant — condition and body both
      cost 80x. The same capture defect Stages 3m–3z fixed for the kernels, on the most
      fundamental construct in the language.

      Additive by design: `eligible_set` is untouched, so `int_eligible_fns` — what the
      bytecode compiler reads to decide whether a KERNEL may call a user function — still
      describes only functions whose ABI is `params.len()` arguments. The capture-taking
      loop compiles under its own entry point (`name$caploop`), outside `fn_ids`, reachable
      only from the VM's `CallFn`. Same containment the MIXED tail loops already use, and
      the reason there is no transitive capture set: the body's calls still resolve against
      `eligible`, i.e. only to capture-free functions.

      Captured globals ride as trailing `i64` params. `gen_tail`'s back-edge zips the
      self-call's arguments against the REAL parameter slice, so captures are never
      rebound — correct, since nothing else runs during a native call. The VM reads them AT
      DISPATCH, so a `mut` global reassigned between calls is current for each
      (`[5, 9]`, not `[5, 5]`); any non-`Int` global declines to the VM.

      `free_idents` is correct by construction over exactly the forms `value_eligible`
      accepts — its catch-all is `false`, so anything else is ineligible and the capture
      list is never consulted; within that set binders occur only in `Let` and `Match`
      arms. Eligibility is then re-asked through `value_eligible` ITSELF with captures as
      parameters, so the two cannot drift on what "i64-closed" means.

      TEST LESSON WORTH KEEPING: two sabotages (dropping `let` binders or match-arm binders
      from `free_idents`) produce the RIGHT ANSWER 80x slower — the phantom capture fails
      to resolve to a global and the loop falls back to the interpreter. Only an
      ENGAGEMENT assertion sees that. And an earlier draft's "Float global declines" case
      used `n = 4.5` with a body reading `if i >= 3`: the global is never read, so there is
      no capture, the function compiles by the ordinary path, and the case proved nothing.
      A decline case must READ the global it names.

- [ ] **What the loop findings mean for `for`/`while` SYNTAX.** Recorded because the
      question will come up again. The capability exists and is fast: 5e8 iterations in
      0.11s (0.22 ns/iter, ~1 cycle), and Collatz — nested, data-dependent exit, the
      archetypal `while` — runs 70x faster than the VM. `mut` already ships (ADR 0004);
      the limit is that assignment is a STATEMENT, so `map(it => total = total + it)` is a
      parse error. What blocks adding `while` is not semantics but **the absence of an
      O(1) append**: the only accumulate-into-an-array spelling is `acc.concat([x])`, which
      measured 1.78s at n=20k, 14.36s at n=40k and 85.69s at n=80k against 0.01s for the
      comprehension — ~8,500x at n=80k and DIVERGING. `while` would invite exactly that.
      Fix the append first, then the syntax is sugar over a fast path instead of a trap.
      (Also open: the sieve's in-place marking loop, the one genuinely inexpressible
      algorithm — `k6_sieve_trial` is 83s against 0.02s for the `primes()` builtin.)

- [x] **Stage 4e — `concat` over packed numeric arrays stopped boxing: 83.9s → 5.1s.**
      The general path did three passes per call — `to_values()` boxes the receiver,
      `to_vec()` clones it, `array_sniff` unboxes the result — moving 16 bytes per element
      twice to append to a buffer of 8-byte elements. Fast path sits beside the
      `enumerate` one in `call_method` (it needs the `Rc`, before `to_values()`): both
      sides `Ints`/`Range` → one `Vec<i64>` and a memcpy; same for `Floats`. Mixed
      Int/Float falls through, so `[1].concat([2.0])` stays `[1, 2.0]`.

      | n | before | after |
      | --- | --- | --- |
      | 20,000 | 1.83s | **0.05s** |
      | 40,000 | 14.18s | **0.13s** |
      | 80,000 | 83.94s | **5.10s** |

      **Still O(n²)** — the receiver is copied every call, and what remains is exactly
      memcpy bandwidth (the uneven curve is the L2 cliff: 640 KB a copy at n=80k against
      320 KB at 40k). An O(1) append needs the receiver's `Rc` to be unique, and it never
      is: the caller's binding holds one reference and the evaluated receiver on the stack
      holds another. See the liveness item below.

- [x] ~~**`dot` SILENTLY LOSES PRECISION ON INTEGER ARRAYS**~~ — **FIXED** (`736fe10`).
      `dot` now accumulates in `i128` with `checked_mul`/`checked_add` and returns an `Int`
      when it fits, so `xs.dot(xs)` is `333332833333500000` and `[1,2,3].dot([4,5,6])` is
      `32`, matching every equivalent spelling and every sibling reduction. The record of
      the defect is kept below because the METHOD that found it is the reusable part.

      A wrong answer, not a style choice. Found by the standing method (a program against
      its own equivalent spelling). At n = 1,000,000:

      | spelling | result |
      | --- | --- |
      | `xs.dot(xs)` | `333332833333127552.0` |
      | `xs.zip(xs).map((a, b) => a * b).sum()` | `333332833333500000` |
      | `xs.map(it * it).sum()` | `333332833333500000` |
      | exact (Python) | `333332833333500000` |

      **Off by 372,448**, and the result is a `Float` where every sibling reduction over
      `Int`s returns an `Int` — `sum`, `product` and `reduce` all do. `[1,2,3].dot([4,5,6])`
      is `32.0`, not `32`. The cause is that `dot` goes through `f64` unconditionally,
      so it starts losing integers above 2^53 while the two equivalent spellings stay
      exact (`sum` promotes to `i128`).

      The fix was an `Int` fast path summing in `i128` exactly as `sum` does. It is a
      USER-VISIBLE type change for integer inputs (`32.0` → `32`), which is why it was
      recorded here rather than slipped in: it got its own commit, a decision recorded
      against `docs/integer-semantics.md`, and a check of whether any corpus program or
      example depended on the `Float`.

- [x] **Peak-RSS sweep over 26 array operations — six defects, one shape (2026-08-08).**
      The instrument matters as much as the result: peak RSS reproduces to ~0.05% where
      wall clock was swinging 5× on this box under an unrelated dev server, so
      memory-shaped defects stayed measurable when timing ones did not. Baselines: a
      20M-element packed `Ints` array is 160 MB, and `xs[0]` reads 185 MB total.

      | op | before | after | |
      | --- | --- | --- | --- |
      | `contains(4)` | 491 MB | 186 MB | `6315b83` |
      | `index_of(4)` | 491 MB | 185 MB | `6315b83` |
      | `reverse().first()` | 797 MB | 340 MB | `867462c` |
      | `sort().first()` | 799 MB | 340 MB | `867462c` |
      | `cumsum().last()` | 645 MB | 340 MB | `d1b7da6` |
      | `[xs].flatten()` | 797 MB | 339 MB | `d1b7da6` |
      | `range(100000000).reverse().first()` | 3071 MB / 9.00s | 15 MB / 0.00s | `867462c` |
      | `range(100000000).sort().first()` | 3071 MB / 3.00s | 15 MB / 0.00s | `867462c` |

      Every one was the SAME SHAPE: one operation with two spellings, only one of them
      fixed. `contains`/`index_of` boxed 306 MB to answer a scalar while their closure
      neighbours `any`/`position` already streamed; `sort`/`reverse` returned through
      `Value::array` where `array_sniff` sat beside it and re-packs. Counting `clamp`
      (array vs scalar builtin), `dot` (vs `sum`/`cumsum`), duplicate record fields
      (literal vs update) and `take`/`drop` (packed vs lazy `Range`), that is nine
      instances, so it is now the primary SEARCH HEURISTIC rather than an observation:
      list the pairs of spellings for one operation and check the neglected one.

      What stayed flat stayed flat for a reason — `take`, `position`, `any` and indexing
      were already optimal, and `concat`/`drop` sit at 339 MB because a 20M-element packed
      RESULT genuinely costs 160 MB on top of the source under eager semantics.

      Two findings came from sabotage rather than from writing the code: a `checked_neg`
      guard I added for `step == i64::MIN` was DECORATION (`-2^63 == 2^63 (mod 2^64)`, so
      the wrapping version cannot produce a different answer) and was removed; and the
      test case for it was too weak to notice, because the range I first used had one
      element, where the step is never applied.

- [ ] **THE CALCULUS FRONTIER: a user function called with a COMPUTED float argument
      never compiles, in ANY spelling — and neither does a function passed as a
      PARAMETER.** Library feedback (2026-08-09, the `calculus` module's
      `range(0,n).reduce(0.0, (acc,i) => acc + f(a + (i+0.5)*h))` stayed interpreted at
      2.6s after the batch that JIT'd its neighbours), reproduced and extended here — five
      probes at n=4M, engagement read as the JIT-vs-VM child-CPU ratio:

      | shape | JIT | VM | verdict |
      | --- | --- | --- | --- |
      | reduce, known i64 fn, bare index `g(i)` | 0.00s | 0.54s | **native** |
      | reduce, known f64 fn, computed `g(a + (i+0.5)*h)` | 0.49s | 0.59s | interpreted |
      | MAP spelling of the same | 0.63s | 0.71s | interpreted |
      | reduce, PARAMETER fn, bare `f(to_float(i))` | 0.49s | 0.49s | interpreted |
      | MAP spelling with a parameter fn | 0.62s | 0.63s | interpreted |

      Two independent blockers, and the second probe pair matters because it closes the
      "just respell it" escape: the boundary is SYMMETRIC across map and reduce, so no
      rewrite rescues the numerical-integration idiom today.

      1. **Computed/f64 call arguments.** Only the bare-loop-index call form compiles
         (Stage 3r's `to_float(g(i))`). The Stage 3y machinery marshals f64 args across
         the mixed-call ABI in map bodies, so the marshalling exists; the gate that
         declines a computed argument has NOT yet been traced to a site — do that before
         proposing a fix (the three-reverts lesson).
      2. **Function-valued callees.** Every native call is by `FuncId`, resolved by NAME
         at compile time; a function arriving as a parameter is an opaque value with no
         indirect-call machinery behind it. Structural — needs per-callsite
         specialization, inlining, or native indirect calls. This is the real blocker for
         a numerics LIBRARY, whose `integrate(f, ...)` can never name its callee.

      The library session's own four-probe diagnosis of this boundary was correct as
      stated — reproduced here before recording, per the standing rule.

- [ ] **`argmax`/`argmin` are ~12x slower than `index_of(max())`** at n=1M (0.120s against
      0.010s) for the same answer — the idiomatic spelling losing to the manual one, which
      is the exact defect signature this project hunts. They desugar through
      `enumerate` + `reduce` over tuples, which no kernel compiles. Same root cause as the
      15x gap measured for `xs.enumerate().filter(...)` against a plain `filter`: a lazy
      `Enumerate` still hands the body a TUPLE, and tuples are not packed.

      **This gap WIDENED from 7.7x to 12x on 2026-08-08, and the cause was my own work**:
      packing `index_of` (`6315b83`) took the manual spelling from 0.02s to 0.010s and its
      memory from 56 MB to 40 MB, while `argmax` did not move. Recorded rather than quietly
      restated, because it is the honest shape of the result — optimizing one spelling of
      an operation widens its distance from the other unless both are done, which is the
      same lesson the sweep above kept teaching from the other direction.

- [ ] **LAST-USE LIVENESS — the prerequisite for `while`/`for` syntax and for an O(1)
      append.** Today the final read of a binding clones its `Rc`, so an accumulator is
      always shared at the moment it is extended and can never be mutated in place. If the
      compiler knew a read was a binding's LAST, it could MOVE instead — leaving the `Rc`
      unique, which is the same uniqueness check the map kernels already use to reuse a
      dead buffer (Stage 3o). That single change makes `acc.concat([x])` O(1) amortized,
      which is what turns an imperative loop from a diverging trap into ordinary code.
      Until then, adding `while` would hand users a familiar name for the 8,500x shape.

## Design: admitting a non-literal `%` / `//` / `>>` (the 17-110x item)

Worked out in full on 2026-08-04 so the next pass implements rather than re-derives.
Everything below is measured or read out of the source, not assumed.

**THE SEMANTICS TO REPRODUCE.** `%` and `//` are EUCLIDEAN, not truncating — verified on
all three engines:

    7 % -3  ->  1      7 // -3  ->  -2       7 = (-2)(-3) + 1
    -7 % 3  ->  2      -7 // 3  ->  -3      -7 = (-3)(3)  + 2

    7 % 0, 7 // 0          raise
    MIN % -1, MIN // -1    do NOT raise (they wrap)
    1 << 64, 1 >> 64, 1 >> -1   raise

**WHY THE CURRENT LOWERING CANNOT JUST DROP ITS GATE** (src/jit.rs:5789-5822). It is
correct only because the gate guarantees a POSITIVE constant:

    Mod:      rem = srem(a, b); select(rem < 0, rem + b, rem)          -- `+ b` needs b > 0
    FloorDiv: q = sdiv(a, b);   select(rem < 0, q - 1, q)              -- `- 1` needs b > 0
    Shl/Shr:  ishl_imm / sshr_imm with the constant                    -- needs a constant

The general forms are ordinary arithmetic, no new concepts:

    Mod:      rem = srem(a, b); select(rem < 0, rem + abs(b), rem)
    FloorDiv: q = sdiv(a, b); rem = srem(a, b);
              adj = select(b > 0, q - 1, q + 1); select(rem < 0, adj, q)
    Shl/Shr:  the register forms, guarded on the count

**WHAT MUST BAIL, AND WHY.** Cranelift's `srem`/`sdiv` TRAP — they do not merely produce a
wrong value — on two inputs, so both must be branched around before the instruction:

    b == 0                  -> bail; the VM re-runs and raises the interpreter's exact error
    a == i64::MIN && b == -1 -> bail; the correct answer WRAPS rather than raising, and the
                                VM produces it exactly. Astronomically rare, so the branch
                                costs nothing in practice.
    shift count outside 0..=63 -> bail

**THE PRECEDENT TO COPY** is Stage 3w, which admitted a non-literal FLOAT divisor behind an
immediate poison bail (src/jit.rs:3649-3658, the `BinOp::Div` arm of `gen_value_env`). Same
hazard class, and that path measures clean today (`to_float(k) / d` is 1.03x its literal
twin, i.e. no penalty at all). IMMEDIATE bail rather than accumulate-and-store, for the
reason recorded there: a tail loop can be infinite, so the error cannot wait.

**4h PROBE (2026-08-04): narrowed, and the probe itself was flawed.**

NOT FUSION. A deliberately non-fused shape regresses identically, so `map(...).sum()` was a
red herring and the standalone MIXED map kernel is what declines:

    n = 20000000
    xs = (0..n).map(to_float(it % 7))
    print(xs.length())            0.04s -> 1.58s

Interleaved, warmed, median of 11 child-CPU samples, both binaries present at once:
`lit % 7` 0.103s -> 1.570s; `var % m` 1.703s -> 1.811s.

**THE PROBE WAS WRONG.** It printed `eligible=Some(0) stored_caps=0` and I read that as the
capture leg agreeing. The leg is `c == k.captures` — equality of two `Vec<Capture>` — and I
printed their LENGTHS. The `raises` leg was genuinely established; the capture leg was not.
The probe program also printed the right answer at n=8, which proves nothing: at that size a
declined kernel and a compiled one are indistinguishable.

Fourth instance in this thread of resting a conclusion on something ADJACENT to the fact.
The rule extends to instruments: **print the predicate, not a summary of its inputs.**

**NEXT PROBE, precisely:** in the mixed-Float re-check (:920-937) print
`map_body_raises(..) == k.raises` and
`mixed_map_eligible(..).is_some_and(|c| c == k.captures)` as BOOLEANS, plus the final `ok`,
for `to_float(it % 7)` at a size where the difference shows. If both are true the decline is
downstream of this branch, and the next thing to instrument is the kernel-pointer lookup at
dispatch.

**THIRD 4h ATTEMPT — sound, but it DECLINED a body that used to compile.** Scoped per the
traced wiring: relaxed the MIXED map gate (`infer_mixed_kind`, :2846) whose generator has a
poison accumulator, left the PLAIN i64 gate alone, added the guarded lowering and the
`map_body_raises` arms.

**Sound:** opfuzz 880 programs x 3 engines, 0 aborts, 0 divergences. Both defects that
killed attempt two are gone.

**But a 17x REGRESSION**, which the fuzz cannot see and the gate would not catch:

    (0..20M).map(to_float(it % 7))     0.03s -> 1.70s

That is the LITERAL spelling, compiling before and declining after — confirmed by
`HELIX_NOJIT=1` matching the JIT run (1.60s vs 1.66s), the signature of a kernel never
built. Reverted.

**Why that is informative:** relaxing a gate cannot make fewer things eligible. `op_ok`
went from three arms to unconditional `true`. So something DOWNSTREAM is coupled to the
literal test — most likely one of the three build-time re-checks at :935-960, which
re-derive the analysis and demand it reproduce exactly what the compiler stored
(`map_body_raises(&k.body, ..) == k.raises`, plus the capture list). A disagreement drops
the kernel silently.

**NEXT STEP, one probe:** put an `eprintln!` on each re-check leg at :935-960 and run
`(0..8).map(to_float(it % 7))`. Whichever leg fails names the coupling. Do this BEFORE more
codegen — three attempts have been lost to inferring structure instead of observing it, and
each observation has taken exactly one probe.

**Banked:** Stage 4g (mixed FUNCTION path) shipped at 57x. The guarded euclidean lowering,
both poison helpers and the `map_body_raises` arms are written and fuzz-clean, waiting only
on this question.

**THE ANSWER (traced 2026-08-04), which supersedes BOTH notes below and restores the
original design.** `define_array_kernel` (src/jit.rs:4990) picks the body generator on one
condition:

    let r = if let Some(root) = mixed_root {
        gen_value_typed(...)   // carries a `poison` accumulator
    } else {
        gen_value(...)         // NO poison parameter
    };

A PLAIN i64 map kernel is therefore generated by `gen_value`, which cannot report a bail —
the whole explanation for the abort at `gen_value:5896`. Stage 4h relaxed
`value_eligible_cap`, which gates the PLAIN map, while patching `gen_value_typed`, which
serves the MIXED map. **One generator patched, a different one opened.**

| generator | poison? | serves | gates |
|---|---|---|---|
| `gen_value` | **none** | i64 functions, PLAIN i64 map/filter, fused | 1208, 1211, 3158, 3164 |
| `gen_value_typed` | accumulator, read by `run_map_poison` | MIXED map kernels | 2846, 3116 |
| `gen_value_env` | bails to a poison BLOCK | mixed functions | 3660-3669 — **Stage 4g, shipped** |

**IMMEDIATELY AVAILABLE:** relax 2846/3116 (mixed map). No new machinery —
`gen_value_typed` already has the accumulator, `run_map_poison` already reads it, and the
guarded euclidean lowering was written during 4h. Small stage.

**STILL BLOCKED:** everything on `gen_value` — the i64 function path, the plain i64
map/filter kernels, and fused — until it gains a poison target. **One refactor unblocks all
of them at once**, including the 110x tail-loop case. This is what the ORIGINAL design note
said, and it was right; the two "corrections" below were not.

**THE ACTUAL WIRING (traced 2026-08-04), which corrects the note below.** One consumer at
a time, gate -> codegen -> FFI:

| shape | gate | codegen | FFI | can bail? |
|---|---|---|---|---|
| plain map | `map_kernel_captures` (:2463) -> `value_eligible_cap` | `gen_value_typed` (has a `poison` accumulator) | `run_map_poison` -> `Option<Vec<D>>` | **yes** |
| filter | `filter_kernel_eligible` (:3224) -> `cond_eligible_cap` (:3195) -> `value_eligible_cap` | — | `run_filter_kernel` -> `Vec<i64>` | **no** |
| fused | `map_kernel_eligible` (:2449) -> `value_eligible` (the 1208/1211 gate) | `gen_value` | — | **no** |

`value_eligible_cap` has exactly TWO consumers and they arrive by different routes: the map
gate directly, the FILTER gate transitively via `cond_eligible_cap`. Relaxing it therefore
relaxes filters as a SIDE EFFECT — the real mechanism behind the swallowed `filter % 0`,
and nothing in the map gate's signature reveals it. The FUSED path is unaffected: it goes
through `value_eligible`, which 4h never touched.

**This makes the fix smaller than the note below claims.** A `can_raise: bool` threaded
into `value_eligible_cap`, `true` from `map_kernel_captures` and `false` from
`cond_eligible_cap`. No FFI change: filters keep their literal-only restriction correctly,
because they have nowhere to report. **The filter poison port is then a later optional
stage that buys filters the same win — not a prerequisite.**

**ONE QUESTION TO ANSWER BEFORE WRITING CODE** (guessing it wrong cost the last two
attempts): the reverted build aborted at `gen_value:5896` for `(0..4).map(it >> d).sum()`,
yet the fused path uses `value_eligible` and was never relaxed. Something on the plain-map
route reaches `gen_value` — most likely `gen_value_typed` delegating for a subexpression.
Find the call; do not infer it.

**WHY 4h IS BLOCKED, established empirically (2026-08-04).** The note below says the next
attempt must find out which generator each shape reaches rather than reason from the gate.
Done — and the blocker is deeper than a missing patch:

**`run_filter_kernel` returns `Vec<i64>`, not `Option<Vec<i64>>`** (src/jit/ffi.rs:667;
`run_filter_kernel_range` at :460 likewise). **A filter kernel has no poison out-param at
all**, so it cannot report a bail. That is the entire explanation for the swallowed error:
the generated code set a poison variable that nothing on the FFI side reads. The guard was
correct; it had nowhere to report to. Compare `run_map_poison` (:503), which returns
`Option<Vec<D>>` — map can carry a bail, filter cannot, and reduce/fused go through
`gen_value`, which takes no poison parameter either.

**And the gate is shared**: `map_kernel_captures` (src/jit.rs:2463) is the single entry
point and feeds `value_eligible_cap` for map AND filter alike, so relaxing it is unsound by
construction for one of its two consumers.

So 4h needs, in order:
  1. a poison out-param for `run_filter_kernel` / `run_filter_kernel_range` — a PORT of what
     the map kernels already have, not a new design;
  2. a `can_raise` flag through `map_kernel_captures` into `value_eligible_cap`, so the
     relaxation reaches only consumers that can report;
  3. then the gate relaxation and guarded lowering, already written and measured at
     **16.8x** before the revert.

**STAGE 4h ATTEMPTED AND REVERTED (2026-08-04).** Extending the same work to the i64
MAP/FILTER kernels was implemented and measured at **16.8x** (1.191s -> 0.071s on
`(0..20M).map(it % m)`, variable and literal spellings finally equal), then reverted:
`scripts/opfuzz.py` found two defects that the value tests and the gate both missed.

    i64-map  `>>`  operand 63
        jit  error: internal error (src/jit.rs:5896): entered unreachable
        vm   0
    filter   `%`  operand 0
        jit  (prints nothing, exit 0)
        vm   error: modulo by zero

The first is a REACHABLE `unreachable!()` — the class that made `xs.clamp(5, 1)`
core-dump. The second is worse: a poisoned filter kernel does not discard its result, so
the program prints an empty answer and exits 0 where the interpreter raises.

**THIS CORRECTS THE TWO EARLIER DESIGN NOTES.** `be93c73` said the i64 gates funnel into
one generator; the correction in `5727ebf` said `gen_value` and `gen_value_typed` were
that one generator shared by two paths. Both are wrong. **The i64 map path reaches BOTH
generators depending on the expression, and the FILTER kernel discards differently again.**
Any next attempt must begin by establishing, empirically, which generator each kernel
shape actually reaches — not by reasoning from the gate that guards it. The gate
relaxation, the euclidean lowering and the poison-accumulator guards were each correct;
what was missing was coverage of every generator the relaxed gate can now reach.

Stage 4g (the mixed path, `9ee76a7`) is unaffected and stays.

**STATUS after Stage 4g (`9ee76a7`).** ONE of the seven gates is done: the mixed FUNCTION
bodies at src/jit.rs:3660-3669, measured at 2.83s -> 0.05s (57x) on a mixed tail loop with a
variable modulus. **The commit message for 9ee76a7 says "the MIXED map/function bodies",
which overstates it** — the two mixed MAP gates (2846 and 3116) were NOT touched and still
require a positive literal. Six gates remain.

**A REFINEMENT TO THE STAGING, learned while doing 4g.** The remaining i64 gates (1208,
1211, 3158, 3164) all funnel into ONE code generator, `gen_value`, which is shared by the
i64 FUNCTION path and the i64 MAP-KERNEL path. Those two do not have the same machinery:
the kernel has `run_map_poison`, the function has no poison in its ABI at all. So they
cannot be staged independently the way the design first assumed — admitting a non-literal
operand in `gen_value` requires threading an `Option<Block>` poison target through it, and
the eligibility gate must then admit non-literals ONLY where that target exists. That is one
refactor serving both, not two separate stages.

Until it lands, `gen_value`'s `unreachable!()` for a non-literal shift stays protected by
those four gates. VERIFIED after 4g, since relaxing one gate is exactly how such a thing
becomes reachable: mixed tail loops and mixed non-tail functions returning `1 << k`,
`256 >> k`, `7 % d` and `7 // d` in tail position, plus i64 map kernels over `it % 3` and
`it << 1`, all run correctly on all three engines with no abort.

**THE SEVEN GATES**, all `matches!(**right, Expr::Int(n) if n > 0)` (shifts `(0..=63)`):

    src/jit.rs:1208, 1211   tail loop
    src/jit.rs:2846         mixed map
    src/jit.rs:3116         indexed mixed map
    src/jit.rs:3158, 3164   i64 map / filter captures
    src/jit.rs:3660-3669    mixed function bodies

**STAGE IT BY POISON MACHINERY, NOT BY OPERATOR** — this is the part that decides the order:

  * The MIXED map/function paths (2846, 3116, 3660-3669) ALREADY have a poison out-param and
    a poison block. Those are the cheap ones and should land first; the mixed-function path
    is also where the k2-class tail loops live.
  * The i64 MAP/FILTER kernels (3158, 3164) have `run_map_poison` from Stage 3v/3z, so the
    accumulator exists; the kernel just needs marking as raising.
  * The PURE-i64 FUNCTION path (1208, 1211) has NO poison in its ABI — a compiled i64
    function is `fn(i64, ...) -> i64` with nowhere to report a bail. Giving it one is a
    signature change for every compiled function and should be its own stage, sequenced
    last. **This is why the 110x tail-loop case is the LAST one to fix, not the first**,
    even though it is the largest number.

**DO NOT** "fix" this by constant-folding the right operand so `% (3 + 4)` and
`MOD = 1000000007` pass the existing gate. It would leave the real case — a divisor that
arrives from data, `m = read_int()`, measured at 1.150s and permanently on the VM — with no
fast spelling at all, while making the gate look repaired.

## Syntax gaps, found by probing the parser rather than inferring it (2026-08-04)

Written down because inferring the language from the STYLE of older tests produced a
confidently wrong claim: that Helix had no unary minus. It has one. What it lacks:

- [ ] **`-9223372036854775808` is a FLOAT, not `i64::MIN`.** The magnitude exceeds
      `i64::MAX`, so the lexer degrades the literal to `f64` (src/lexer.rs:386, deliberately)
      and the negation applies to a float. `-9223372036854775807 - 1` gives the correct Int.
      It prints as `-9223372036854775808.0`, so the type change is visible but easy to miss —
      the same silent-boundary shape as the `dot` defect. Fix: fold a unary minus into a bare
      integer literal at parse time. The subtlety is that the fold must happen AFTER the
      operand is parsed, or `-1.abs()` would become `(-1).abs()` instead of `-(1.abs())`.
- [ ] **No tuple field access.** `(1, 2).0` is "expected a name after `.`, found a number".
      `(1, 2)[0]` DOES work, so this is pure sugar, but records use `.name` and the asymmetry
      surprises.
- [ ] **No record destructuring.** `a, b = [1, 2]` and tuple destructuring both work;
      `{a: x} = {a: 7}` does not parse. Records are a core type, so this is an inconsistency
      rather than a missing nicety.
- [ ] **No inclusive range.** `(0..3)` is exclusive and `(0..=3)` does not parse, so an
      inclusive bound must be written `(0..n + 1)`.
- [ ] **No `~`** (bitwise NOT), though `&`, `|`, `^`, `<<`, `>>` are all present.
- [ ] **No `+` on strings.** `"a" + "b"` raises, and STRING_METHODS has no `concat`/`join`,
      so interpolation is the ONLY way to join two strings. Arguably correct under
      "one obvious way" — recorded so the decision is explicit rather than accidental.
- [ ] **A bare named predicate binds inconsistently.** `xs.map(f)`/`any(f)`/`all(f)` wrap `f`
      into `it => f(it)`; `xs.position(f)`, `take_while(f)`, `min_by(f)` do not, and return
      `missing` instead of erroring. `wrap_bound_fn_arg` (src/parser.rs) only reaches the
      general method branch, not the desugared verbs. A silent wrong answer.

CONFIRMED PRESENT, so nobody re-derives them: unary `-` and `not`, `**`, `//`, `%`, `and`/`or`,
`??`, tuple/array/string indexing INCLUDING negative indices, slices with open ends, record
field access, default arguments (`fn f(x, y = 2)`), parameter and return type annotations,
multi-line lambdas and `do` blocks, closures, functions as values, match guards and
or-patterns, trailing commas, `range(3, 0, -1)`, and format specs (`"{x:.2f}"`).
Absent by design (there is one obvious way instead): `!`, `&&`/`||`, `|>`, `//` comments,
`return`, and chained comparison, which has its own error message.

## Deep audit, 2026-08-04 — 44 candidates swept, 11 confirmed under adversarial refutation

Five parallel sweeps (performance inversions, three-engine divergence, memory, robustness,
semantic consistency), each finding then handed to a separate agent whose instructions were
to REFUTE it. One was refuted and is recorded as such. Every survivor below was reproduced
with interleaved child-CPU timing, medians of 15–21, and stdout printed beside each time.
**Ordered by value, which is not the order they were found in.**

- [x] **`xs.clamp(hi, lo)` ABORTED THE PROCESS** — SIGABRT, exit 134, uncatchable by `try`.
      `Ord::clamp`/`f64::clamp` panic when `min > max`. The scalar `clamp(x, lo, hi)` builtin
      always had the guard; the array method did not. **Fixed in `23d8d69`.**

- [ ] **`%`, `//` and `>>` decline the WHOLE enclosing kernel unless the right operand is a
      positive integer LITERAL — 17–110× CPU, ~50× wall.** The highest-value item open.

      | shape | variable divisor | literal divisor | ratio |
      | --- | --- | --- | --- |
      | tail loop `i % m` | 2.233s | 0.020s | **110×** |
      | reduce `a + k % m` | 1.276s | 0.019s | 67× |
      | map `k // m` | 1.238s | 0.054s | 23× |
      | map `k >> s` | 1.192s | 0.049s | 25× |
      | `MOD = 1000000007` then `% MOD` | 1.333s | 0.061s | 22× |

      Controls in the same interleaved run are CLEAN — `k * m` 0.96×, `k & m` 1.00×, and
      float `to_float(k) / d` 1.03× — so a variable right operand is not the problem in
      general. It is these three operators. `HELIX_NOJIT=1` makes both spellings equal, and
      the JIT-enabled variable spelling equals its own VM time: **the JIT contributes nothing
      there; the entire gap is the eligibility gate.** Wall clock is worse than CPU (~50×)
      because the declined body runs boxed AND single-threaded (RSS 286.8 vs 189.8 MB).

      **The gate is a TOKEN test, not a constant test**: `k % (3 + 4)` also declines, 23×.
      Seven sites share it — src/jit.rs:1208, 1211 (tail loop), 2846 (mixed map), 3116
      (indexed mixed map), 3158, 3164 (i64 map/filter captures), 3660–3669 (mixed fn bodies)
      — all testing `matches!(**right, Expr::Int(n) if n > 0)`.

      **The fix already exists for the sibling case.** Stage 3w admitted a non-literal FLOAT
      divisor behind an immediate poison bail on a runtime zero (src/jit.rs:3649–3658), and
      that path measures clean above. Same hazard class, half fixed. The extra work over the
      float case is sign handling: `%` is `rem_euclid`, and `i64::MIN % -1` / `i64::MIN // -1`
      need the same care the literal path already takes; `>>` must bail outside `0..=63`.

      ROADMAP.md:763–767 does say this exclusion is deliberate, but it documents the
      correctness rationale for the gate, not parity between the two spellings — and it
      predates the poison mechanism that dissolves that rationale.

      **A whole class of program has no fast spelling at all**: a divisor read from data can
      never be a literal. `m = read_int() ?? 7` then `k % m` is 1.150s, permanently on the VM
      — a histogram whose bucket count is a parameter, a hash whose table size is computed.

- [ ] **`any`, `all`, `position` have no native kernel — ~45× slower than
      `filter(...).count()` when the scan runs to completion.** `range(20M).any(it < 0)` is
      0.779s against 0.020s for the same question spelled with `filter`. The distributions do
      not overlap: the slowest `filter` sample is 28× faster than the fastest `any` sample.
      Short-circuiting is already correct (Stage 4c); what is missing is the kernel for the
      case that runs to the end — which is the NORMAL outcome of a validation check.
      `filter().count()` is confirmed to be a real per-element native loop (linear, ~0.75
      ns/element), not a folded constant.

- [ ] **`unique`/`frequencies`/`top` are O(n × distinct) on FLOAT arrays** — the same defect
      Stage 4a fixed for integers, on the element type that is the scientific default.
      `(0..60000).map(to_float(it)).unique()` takes 3.2s; **stringifying every float and
      hashing the text is 220× faster** at 0.04s. The Stage 4a comment predicted this ("a
      Float path would mishandle -0.0/NaN") — the design is: canonicalize `-0.0` to `+0.0`,
      and give every NaN its own bucket, since NaN is not reflexive and so forms no
      equivalence class.

- [ ] **A `map` from a Floats array to an Int result compiles NO kernel — 11–25×.** The
      standard data-cleaning step (`to_int`, `floor` to an index, `sign` of a delta).

- [ ] **`zip` materializes one `Rc`-boxed 2-tuple per element — 132 bytes and 15× CPU.**
      `a.zip(a).length()` on 5M packed ints costs 0.20s and 631 MB to produce a length
      (re-measured 2026-08-08; was 0.228s / 645 MB). `zip`/`zipmap` is the ONLY spelling
      Helix offers for an elementwise two-argument function that broadcast cannot express.

      Sharpened by the sweep below: **`enumerate` is lazy and `zip` is not**, though both
      exist only to pair elements into tuples — `a.enumerate().length()` on the same input
      is **15 MB** against `zip`'s 631 MB. `ArrayData::Enumerate { inner }` is already the
      precedent, so the fix is a symmetric `Zip { a, b }` variant rather than a fast path;
      that is a representation change touching `get`/`len`/`to_values` and all three
      engines, so it is sized as a feature, not a patch.

      **A cheap interim fast path for `zip(..).length()/.first()/.count()` DOES NOT WORK —
      I suggested it and it is wrong.** Measured: `.first()`, `.last()` and `.count()` each
      cost the SAME 631 MB as `.length()`, because `zip` has already materialized by the
      time the outer method dispatches. There is nothing to intercept at that layer; the
      interception has to happen where `zip` itself builds, which is the full variant.

      Two design notes from scoping it, both worth keeping: the variant must be
      `Zip { a, b, len }` with `len = a.len().min(b.len())` computed ONCE at construction
      (mirroring `Range { start, step, len }`) — deriving the length recursively through
      nested zips is exponential; and computing the truncation at construction is not just
      an optimization but SOUND, because the intercept clones both source `Rc`s, so
      `strong_count >= 2` from that instant and no in-place `Rc::get_mut` mutation of
      either buffer can succeed afterwards.

- [x] ~~**`take(k)`/`drop(k)` box the ENTIRE source**~~ — **FIXED** (`8c9319b`). Re-slices
      the packed buffer: `(0..20000000).map(it * 2).take(3).sum()` went 503 MB → 190 MB.
      A lazy `Range` already had this path; a range that has been through `map` is `Ints`,
      and that spelling kept boxing.

- [ ] **`min_by`/`max_by`: ~200 ns and ~92 bytes per element** — 1.02s and 532 MB where
      `min()` is 0.021s and 69 MB (re-measured 2026-08-08, unchanged). Flat overhead, so
      invisible below ~100k rows. Same shape as `contains`/`index_of` below but inverted:
      there the closure spelling was the fast one, here it is the slow one.

- [ ] **`sort_by(key)` is 13-17× `sort()`, not 4.2× — and the gap GROWS with n.**
      Re-measured 2026-08-08 (child CPU medians, interleaved, packed `Ints` receiver):

      | n | `sort()` | `sort_by(it)` | | `sort().reverse()` | `sort_by(-it)` |
      | --- | --- | --- | --- | --- | --- |
      | 1M | 0.010s / 48.7 MB | 0.140s / 82.8 MB | 14.0× | 0.010s | 0.190s → 19.0× |
      | 4M | 0.060s / 94.7 MB | 0.790s / 204.9 MB | 13.2× | 0.060s | 1.040s → 17.3× |
      | 8M | 0.120s / 156.6 MB | 1.990s / 376.8 MB | 16.6× | 0.130s | — |

      **The doc's 4.2×/5.5× was not wrong when written — the baseline moved under it.**
      `867462c` made `sort`/`reverse` packed, and `sort_by` could not benefit **because it
      never calls `sort`**: `desugar_sort_by` (src/parser.rs:76) rewrites it to
      `$s.map(key).argsort().map($si => $s[$si])`. Three passes, and the fast path is not
      on any of them.

      Two walls, both traced:
      1. **`argsort` has no packed path.** It exists only inside `array_method(items:
         &[Value])` (src/interp/methods.rs:1638), and `array_numeric_fast` — where
         `sort`/`reverse` now live — has no `argsort` arm, so the key column is boxed at
         16 B/element. It then sorts INDICES with a stable sort, one closure call and two
         random-access enum-tag derefs per comparison against a 16-byte stride, where
         `sort` is `sort_unstable()` over a contiguous `Vec<i64>`. ~60% of the cost and
         essentially all of the 2.2-2.4× memory. A packed arm mirroring the `sort` one is
         the fix, and its tie-break must be `.then(a.cmp(&b))` because today's `sort_by`
         is STABLE — unlike `sort`, a keyed sort has distinguishable elements with equal
         keys, so stability IS observable here and `sort_unstable` alone would not do.
      2. **The final gather is unjitable by construction.** `order.map($si => $s[$si])`
         indexes a captured array from a map body whose binder is an ELEMENT VALUE, not a
         loop counter, so the bounds obligation cannot be discharged (the rule is stated
         at src/bytecode/ops.rs:495). ~45% of the cost, and it needs a different mechanism
         than a fast path.

- [x] ~~**`sort_by(-it)` pays extra for the unary minus**~~ — **FIXED** (`e30f9fe`), and it
      turned out to be worth far more than `sort_by`. Unary `-` was not JIT-eligible at
      all: `xs.map(-it)` was 0.45s against 0.04s for `xs.map(0 - it)` at 8M, and
      `xs.filter(-it > -5)` 0.47s against 0.02s — 11-16×, the idiomatic spelling losing to
      the clumsy one. `gen_value` had lowered `Neg` all along; only `value_eligible_cap`
      had never been taught it.

- [ ] **Unary `-` is still missing from ~16 more analyses in jit.rs.** Of the 32 functions
      there that match on `Expr::Binary`, **20 had no `Expr::Unary` arm**;
      `value_eligible_cap` (`e30f9fe`), `infer_mixed_kind` + `gen_value_typed`, and
      `f64_body_eligible` (`55830b6`) are now fixed — every MAP path compiles `-x`. The
      rest are recorded because the sweep is the reusable part — the same "supported in a
      minority of paths" shape. `gen_cond` and `gen_cond_env` remain CODEGEN functions
      without the arm, so the reduce/tail-loop *condition* positions cannot simply be
      admitted; admitting a shape codegen cannot emit is how this area was reverted three
      times.

      **A sabotage-record correction (2026-08-09), because `55830b6`'s commit message
      understates its own test.** It reports two codegen mutations (`fneg` -> `0.0 - x`,
      and the Int branch emitting `fneg`) as compiling and producing identical output —
      "not counting them as caught". Both were MIS-ANCHORED: the mutation script matched a
      bare `match k { ... fneg ... }` block and replaced the FIRST occurrence, which is
      the byte-identical arm in `gen_value_env` (tail loops, ~line 5564), not the arm
      under test in `gen_value_typed` (~6184). Re-anchored on the enclosing call line
      (unique per function), all three mutations are CAUGHT — the `fsub` ones by the
      kernel-scale signed-zero cases (`(0..100000).map(-to_float(it)).take(3)` diverges
      vm-vs-jit at element 0), the Int-branch one by the build. Fourth malformed-mutation
      instance this session; the standing rule stands: **when a mutation survives, first
      prove the mutation is real — and when the same match text exists in two functions,
      an anchor must include a line unique to its function.**

- [ ] **`<` orders tuples but `sort`/`min`/`max` refuse them.** Verified 2026-08-09 on all
      three engines:

          (1, 2) < (2, 1)              ->  true
          [(2, 1), (1, 9)].sort()      ->  error: `sort` needs an array of all numbers,
                                              all strings, or all DNA
          [(2, 1), (1, 9)].min()       ->  error: `min` needs an array of numbers

      One notion of order, two spellings, only one of them implemented — the same shape as
      the nine before it. It also blocks the natural fix for a real ergonomic wart: a
      composite sort key today has to be hand-encoded as arithmetic
      (`0 - (c.w * 100000 + c.uses)`, still present in real code), where `sort_by(c => (c.w,
      c.uses))` is the obvious spelling and would work the moment `sort` accepted the
      ordering `<` already defines. Lexicographic tuple comparison is the semantics `<`
      already implements, so this is extending the reductions to it rather than inventing
      anything.

- [x] ~~**`min`/`max` break ties differently depending on the array's REPRESENTATION.**~~
      — **FIXED** (2026-08-09). The packed float path now breaks ties with `total_cmp`,
      exactly as the boxed path's `numeric_cmp` always has, so the same array gives the
      same answer whatever its representation, `min`/`max` are permutation-invariant, and
      `min() == sort().first()` / `max() == sort().last()` hold everywhere (absent
      NaN/missing, which still yield `missing`). `[0.0, -0.0].min()` is now `-0.0` on
      every spelling — the boxed/sort answer, which also matches Julia.

      What tipped this from "design call" to "defect": the choice was never really
      between Python's answer and Julia's — the boxed path and `sort` had ALREADY chosen
      `total_cmp`, so the only question was whether an array's internal representation
      may be observable. It may not; the packed path was the outlier. Distinct non-zero
      values order identically under `total_cmp` and IEEE `<`, so only signed-zero ties
      moved; measured no cost at 20M elements (0.09s -> 0.10s, within noise). A sweep of
      the neighbouring stats (median/quantile/spread/mean/sum) found NO other
      representation split. Pinned by
      `min_and_max_do_not_depend_on_the_arrays_representation`; sabotage: the IEEE revert
      and a min/max swap are caught, and a last-wins-on-bit-identical-ties control
      correctly survives.

      Still true and still open (pre-existing, unchanged by this): `argmin`/`argmax`
      desugar through `<` on values, so `xs[xs.argmin()]` can disagree with `xs.min()` on
      a signed-zero tie (`[0.0, -0.0]`: argmin 0, min -0.0 at index 1). Same family as
      the tuple-reduce entry above.

- [ ] **`min_by`/`max_by` with a DESTRUCTURING key return a rebuilt tuple, not the
      element.** Verified 2026-08-08, all three engines agree:

          [[1,2],[0,3]].min_by((a, b) => a)   ->  (0, 3)   a Tuple
          [[1,2],[0,3]].min_by(r => r[0])     ->  [0, 3]   an Array
          [[1,2],[0,3]].min_by((a, b) => a).map(it * 2)
              -> error: type Tuple has no method `map`

      Two spellings of one query returning different TYPES, and the doc comment states the
      bug: `desugar_order_by` (src/parser.rs:255-259) builds `Expr::Tuple(params)` and
      calls it "the tuple of binders, which rebuilds the original element" — true only
      when the element IS a tuple, and Helix destructures arrays too.

      **FIXED (`a5737ce`)** by the two-pass desugaring
      `let $obe = recv in $obe[$obe.map(key).argmin()]` — and the caveat recorded above
      ("changes the error text on empty/`missing` receivers") turned out to be FALSE once
      probed: min_by's errors were always argmin's errors wearing a different name (same
      reduce seed on empty, same "cannot be indexed" on missing, same NaN/missing-key
      comparison errors, same first-wins ties), so composing argmin preserved the whole
      matrix byte-for-byte — verified with full stderr across 38 shapes x 3 engines x 2
      binaries. The key map keeps the user's own lambda, so the four destructure
      diagnostics are also untouched. Bonus: when argmin gets its native fast path (the
      12x item above), min_by/max_by now inherit it for free.

- [ ] **`argsort` and `sort` disagree on `missing` — and on DNA.** Found by the argsort
      probe matrix (2026-08-09), all three engines agree on each, so these are
      pair-inconsistencies, not divergences — the eleventh and twelfth instances of the
      two-spellings shape:

          [1, missing, 2].sort()      -> error: cannot sort: the array has missing values
          [1, missing, 2].argsort()   -> missing            (propagates, rc=0)
          [dna("T"), dna("A")].sort()     -> [A, T]
          [dna("T"), dna("A")].argsort()  -> error: `argsort` needs an array of all
                                             numbers or all strings

      One ordering question, two spellings, two different missing-policies and two
      different type domains. Which side is right is a design call (sort's explicit
      missing error follows ADR-0001's "make dropping visible"; argsort's propagation
      follows the reduction convention), but they should not differ from each other.
      `sort_by` inherits argsort's answers, so today `xs.sort()` and `xs.sort_by(it)`
      also disagree on a missing element and on DNA.

- [x] ~~**Three-engine value divergence — `print((try missing.map()).ok)` is `false` on the
      JIT and VM, `true` on the tree-walker.**~~ — **FIXED** (`comp_shape_check`). It was
      broader than recorded: not only `.ok`, but `missing.map()` itself SUCCEEDED on the
      walker while erroring on the other two, and it affected `filter`/`where`/`any`/`all`/
      `reduce`/`scan`, not just `map`.

      Arity and the binder requirement are STRUCTURAL — the VM and JIT settle them when
      they compile the comprehension, so the receiver's runtime value cannot matter. The
      walker reached the same rules per-arm, after matching the receiver, so the `missing`
      arm returned first and silenced the mistake. It was inconsistent with ITSELF before
      it was inconsistent with anything else: `[1, 2].map()` was an error and
      `missing.map()` was not, for the same malformed call.

      The obvious duplication-free implementation is WRONG, and is worth recording because
      it is genuinely tempting: validating by running the comprehension against an empty
      array reuses every rule and restates nothing — but it evaluates the arguments. All
      three engines agree `missing.reduce(1 / 0, (a, b) => a)` is `missing` while
      `[].reduce(1 / 0, (a, b) => a)` divides by zero (the init is evaluated only on the
      array path), so that version would have swapped this divergence for a new one. The
      check is therefore purely structural, and the sabotage suite includes that mutation
      specifically so the test pins the reason rather than just the behaviour.

- [ ] **The walker and VM report different errors for `5.map()` — masked from users by the
      type checker.** Found while testing the fix above. With the checker in front (every
      CLI path) both say "type Int has no method `map`" and agree. Without it — which is
      what the unit harness does, since `run_vm`/`run_tw` call
      `compile_with_types(.., None)` — the VM reaches its compile-time arity check and
      says "`map` takes exactly one expression" while the walker reaches its receiver-type
      check and says "type Int has no method `map`".

      Unobservable to users today, and unobservable to `vmparity` too, since that runs
      end-to-end. That is exactly what makes it worth writing down: it is a real
      disagreement between two engines of a differential oracle, currently hidden by an
      earlier phase, and it will surface the moment anything runs an engine without the
      checker. Deciding which is right is the actual work — the type error reads better,
      but the arity error is the one that does not depend on the receiver.

- [ ] **A malformed comprehension reports a DIFFERENT error for a `missing` receiver than
      for an array one** — pre-existing, and present identically on all three engines
      (verified against the previous binary), so it is not a divergence, just an
      inconsistency. `missing.filter(1, 2)` says "`filter` takes exactly one expression"
      while `[1, 2].filter(1, 2)` and even `[].filter(1, 2)` say "`filter` expects a
      yes/no test". Same for `filter`/`where`/`any`/`all` with a zero-parameter function.
      `map` and `reduce` do not have this split.

      Traced 2026-08-08: the winning error comes from a pass that runs BEFORE engine
      selection (`types::check`, src/main.rs:831), which is exactly why all three engines
      agree on it. So the arity checks in the `filter`/`where`/`any`/`all` arms of
      `eval_comprehension` are largely unreachable on the array path — decoration, with the
      real check living elsewhere.

      **The obvious fix is REFUTED — do not attempt it.** Hoisting these into the checker
      converts `try`-CATCHABLE runtime errors into UNCATCHABLE static ones: `types::check`
      runs before any engine, so a checker error has no `try` frame to land in. Eight
      programs that exit 0 today, printing `false` from `(try [1, 2].map()).ok` and
      continuing, would exit 1 with EMPTY stdout. It also changes error text for at least
      six other receiver types (DataFrame, Tensor, String, Int, Record and friends) whose
      text is built in src/interp/dataframe_ops.rs:80 and the `unknown_method` path in
      src/types/signatures.rs. Any real fix has to preserve catchability first.

- [ ] **`df.join(<non-DataFrame>)` error text differs** between the walker and the other two,
      and `try` turns it into a String, so it escapes to a value. Low.

- [x] ~~Tuple-accumulator `reduce` is slower than two passes~~ — **REFUTED.** The repro's
      `xs = range(n)` made both spellings pay range materialization; measured properly they
      are equal. Recorded because a refuted candidate is worth as much as a confirmed one.

- [ ] **Two spelling inversions still open** (two others from this sweep are now Stage 3r).
      At 10M elements; a declining JIT runs the bytecode loop, so "VM" means the JIT time
      equals it.

      | shape | JIT | peak RSS | its twin |
      | --- | --- | --- | --- |
      | ~~`map(i => i * c + 1).reduce(0, …)` — **Int** init~~ | ~~0.34s~~ | ~~110 MB~~ | **closed by Stage 3s below** |
      | `map(i => to_float(i) * c).reduce(0.0, …)` — Float init | 0.01s | 20 MB | already fuses ✓ |
      | ~~`scan(0, (s,x) => s + x)`~~ | ~~0.54s~~ | — | **closed by Stage 3t below** |

- [x] **Stage 3t — `scan` gets a native kernel: 0.54s → 0.05s.** The prefix fold was the
      last comprehension with no native form at all. The kernel is SERIAL by definition —
      element *j* depends on element *j−1* — so unlike the map kernels there is no parallel
      form to keep byte-identical; the order IS the definition.

      New machinery, each piece the smallest that works: `Op::TryJitScan` with `TryJitFused`'s
      operand protocol (the VM consumes `[start, end, init]` + captures whether or not it
      takes the native path; the fall-through is this same scan recompiled with `no_fuse`
      set); `Program.scan_loops` reusing `ReduceLoop`; `define_scan_loop` — the reduce loop's
      i64 scalar shape plus one store per iteration; `run_scan_kernel_range`; and the VM arm.
      Eligibility reuses `reduce_loop_captures` (scalar captures and user calls admitted —
      captures were this whole arc's theme, so they are in from day one; `ArrayI64` captures
      decline, since their bounds discharge is unexercised here and no probe has shown a scan
      body indexing an array). An `Int`-literal init only; the VM re-checks everything at
      dispatch and applies the same 100M length cap as the reduce guard.

      TWO LESSONS PAID FOR:
      * `jit::build` has TWO empty-checks — an early "is there anything at all" bail and a
        post-define "did anything compile" bail — and adding a kernel type to only the second
        makes a scan-ONLY program silently never build the JIT. Found because the probe
        `(0..8).scan(…)` printed correct values at VM speed; three temporary `eprintln!`s
        (guard emitted? kernel defined? VM sees a jit?) localized it in one run:
        guard ✓, defined ✗, `jit=false`.
      * The ADR-0024 never-abort ratchet caught the VM arm's three operand pops (+3 on
        `vm.rs`'s budget). They cannot fail — `TryJitScan` is emitted at exactly one site,
        which pushes exactly those three operands immediately before it — so the proof is now
        a comment at the site and the budget moves in the same commit, per the ratchet's own
        instructions.

      Sabotage-proven: storing the PRE-update accumulator (the classic inclusive/exclusive
      scan off-by-one) turns `[0, 1, 3, 6, …]` into `[0, 0, 1, 3, …]` on the JIT alone,
      caught by the first case of the battery. Every value case checks the FULL output array
      element-wise, because the store index is the kernel's one new obligation over a reduce.
      25-case battery across all three engines (0 divergences): wrapping at `i64::MAX` and
      in `20!`, a branching body, captures and calls, Float/array/shadowed-`range` declines
      (which exercise the consume-always stack discipline), scope hygiene (`init` reading an
      outer variable named like the accumulator), degenerate ranges, element-wise reads at
      100k, and an engagement assertion. Corpus golden `j13_scan_kernel`.

- [x] **Stage 3s — a CAPTURED i64 `map.reduce` chain fuses by substitution: 0.34s/110 MB →
      0.00s/20 MB.** The capture-free i64 chain has always fused through `FusedKernel`, but
      that kernel's signature has no caps pointer, so a chain whose map stage captured a
      variable had NO fused form: it materialized its 80 MB intermediate and ran 0.34s where
      the literal spelling ran 0.00s.

      Of the two routes considered, the substitution one (route b) won, and it turned out to
      be almost entirely selection: `emit_map_reduce_fusion`'s emission was already generic,
      so the change is the init guard (`Float` literal → `Float | Int` literal) plus an i64
      eligibility arm calling `reduce_loop_captures` — the SAME collector
      `compile_reduce_range` uses for a captured scalar body, so the fused body is admitted
      on exactly the terms an unfused one would be, and `reduce_bodies_eligible` re-derives
      the identical list at build time. `ReduceLoop.float` comes from the branch. No codegen,
      no FFI, no VM change: the i64 captured reduce kernel and its dispatch (caps
      marshalling, Counter/Scalar bounds discharge) predate this and are reused as-is.

      THE ROUTING RULE: the i64 arm requires captures to be NON-EMPTY. This site is only
      reached when `collect_fusion_chain` declined (compile_fused returns first), and for an
      i64 chain that means a stage captured — but if the chain was declined for some other
      reason, a capture-free body must stay on the path it had rather than gain a second one
      here. Only shapes that previously had no fused form are admitted.

      THE LOAD-BEARING GUARD is Stage 3i's capture-safety check (`expr_mentions(fbody, pa)`),
      and it needed re-proving on this arm because the corruption can be MASKED: if `f`'s only
      free variable is the one named like the accumulator, the corrupted body has no captures
      left and the empty-caps check declines by luck. Sabotage with the single-capture probe
      showed exactly that — nothing broke. Adding a second, genuine capture defeats the mask:
      `s = 4; c = 3; (0..5).map(i => i * s + c).reduce(0, (s, x) => s + x)` returns **618 on
      the JIT against 55** on the other two engines with the guard disabled. That case is
      pinned in the test and the corpus golden precisely because the obvious probe proves
      nothing.

      Also pinned: captures in `g` only and in BOTH stages (the collector merges them in
      first-appearance order), the same capture in both stages (one slot, deduped), an array
      capture `a[i]` whose OOB-at-end must produce the fall-through's exact error, a
      negative-start range whose `a[-1]` the interpreter Python-wraps (native declines via
      the `s < 0` bounds pre-check and all engines agree), wrapping at `i64::MAX`, `init`
      evaluating in the OUTER scope even when it names the accumulator binder, Float-capture
      declines, degenerate ranges, an engagement assertion, and the two NEIGHBOURS this must
      not disturb — the capture-free `FusedKernel` chain and the Float-init substitution.
      Corpus golden `j12_captured_i64_fusion` runs the same shapes as a program on all three
      engines.

- [x] **Stage 3w — non-literal float divisors compile behind a `/0` poison bail: k2 goes
      from 5.3× slower to a ~TIE with C (0.42s → 0.08s).** Two sites, two bail styles:
      * A mixed FUNCTION body (`infer_typed_env` + `gen_value_env`): the bail is
        IMMEDIATE — `divisor == 0.0` branches to the existing `poison_blk` (store 1,
        return), exactly like the NaN-compare bail and for the same reason: a tail loop
        can be infinite, so the interpreter's `/0` error cannot wait for an
        accumulate-and-store. Zero ABI or VM changes — every mixed function already
        carries the trailing poison pointer, and the VM already discards-and-reruns.
        `/` also now takes the f64 branch for two `Int` operands (`10 / 2 == 5.0`),
        which the literal-only rule had made unreachable.
      * A mixed MAP body (`infer_mixed_kind` + `gen_value_typed`): the loop always
        terminates, so `divisor == 0.0` ORs into the Stage 3v poison accumulator, and
        `map_body_raises` counts every `/` so a dividing kernel always gets the poison
        signature. This is what lets `ceil(to_float(i) / d)` compile instead of forcing
        the `* 0.25` spelling.

      Sabotage-proven: removing the immediate bail makes `f(1.5, 0.0)` return `inf` on the
      JIT where both other engines raise "division by zero". The billion-iteration tail
      loop with `/0` on iteration one errors immediately (pinned — accumulate-and-store
      would spin for minutes). `-0.0` divisors count as zero, matching the interpreter's
      `b == 0.0` check. Poison propagates through mixed callees via the existing
      post-call re-check.

      THE ENGAGEMENT ASSERTION EARNED ITS KEEP AGAIN: every map value case passes on VM
      fallback alone, and the assertion is what exposed that a FLOAT-variable divisor
      (`d = 4.0`) still declined — the plain mixed analysis carries captures as Int-proven
      scalars (Stage 3m's contract), so only Int variables and literals engaged (measured:
      0.24s / 0.06s against 3.48s VM at 20M). Closed by Stage 3x below.

- [x] **Stage 3x — value-scalar captures in the plain mixed map: the last spelling that
      declined now compiles.** A FLOAT variable in a mixed body (`d = 4.0`) fell to the VM
      while the identical `d = 4` and the literal ran native — 3.48s against 0.04s at 20M.
      The cause was Stage 3m's contract: the plain mixed analysis types captures `Int` and
      the dispatch proves them `Int`, so a runtime `Float` had nowhere to go.

      The fix is a SECOND specialization of the same stored kernel ("mapmv"), not a new
      analysis: `mixed_map_value_scalar_eligible` reuses `infer_mixed_kind_indexed` (the
      `MixT` walker the indexed kernel already uses) restricted to unindexed bodies with a
      non-empty, all-scalar capture list, then relabels every capture `ScalarValue`. The
      kernel loads them as `f64` bits; `value_scalar_caps` marshals an `Int` by promoting
      and a `Float` by passing bits through. Dispatch order is unchanged — the Int-proven
      marshal is tried first, and this variant only catches what it declines — so no
      existing shape moves. Measured after: float-var, int-var and literal all **0.04s**,
      byte-identical results.

      The build gate compares captures by NAME AND ORDER only, not kind: the stored kinds
      are the plain analysis's `Scalar`s while this specialization's are `ScalarValue`, and
      the kind is a per-specialization loading decision rather than an identity.

      `infer_mixed_kind_indexed` also gained a `Div` arm. Unlike `+ - *` it is safe for ANY
      operand mix including an unpromoted value scalar, because `/` promotes both operands
      in BOTH engines (`10 / 2 == 5.0`) — the promotion the interpreter performs at that
      node is exactly the one the kernel performs.

      THE GUARD IS `mix_combine`, and the sabotage that proves it took two refinements —
      recorded because the obvious probes prove NOTHING. `c = 2^53+1; map(i => to_float(c * i))`
      does not discriminate: `c` is an `Int` at runtime, so the Int-proven marshal wins and
      the value-scalar path is never reached. Multiplier 2 does not discriminate either —
      `(2^53+1) * 2` rounds identically from both directions. The case that works has BOTH a
      `Float` capture (to force the path) and a large `Int` capture inside an integer
      product: `c = 9007199254740993; d = 2.5; map(i => to_float(c * i) + d)`. Forcing
      `(SFloat, Int)` to combine yields `27021597764222980.0` on the JIT against
      `27021597764222984.0` on the other two engines, at element 3.

- [x] ~~**k2 mandelbrot: CAUSE IDENTIFIED — a float division by a NON-LITERAL declines the
      whole enclosing function to the VM.**~~ Closed by Stage 3w above. `row`'s body computes
      `-2.1 + to_float(x) * 2.7 / to_float(g)`, and `infer_typed_env` admits `/` only with a
      nonzero Float-LITERAL divisor (native `fdiv` yields inf where the interpreter RAISES
      on `/0`). So `row` — and `grid`, which calls it — fell to the VM, and every pixel paid
      ~250 ns of VM dispatch into the (native) `step`, which amortized only at huge inner
      caps. CONFIRMED by one change: precomputing the reciprocals once and passing them as
      parameters (`sx = 2.7 / to_float(g)` at the call site, `to_float(x) * sx` in the body)
      is **0.39s → 0.07s** with a byte-identical anchor (40715058) — C parity.

      THE FIX is in the language, not the benchmark: admit `/` by a non-literal `Float`
      divisor in mixed function bodies, guarded by the /0 **immediate poison bail** the
      mixed ABI already has for NaN comparisons (`MixedFn`'s trailing poison pointer —
      check `divisor == 0`, bail, VM re-runs and raises exactly). The benchmark kernel
      stays as written: the idiomatic spelling is the thing being measured, and rewriting
      it to dodge the division would be flattery. Everything previously ruled out (call
      path ~2.5 ns, arity, nesting) stays ruled out — the fixed cost was real and this is
      what it was.

      Holding the pixel count fixed and raising the inner iteration cap (so per-pixel
      work grows while the number of calls does not):

      | pixels | cap | Helix | C | ratio |
      | --- | --- | --- | --- | --- |
      | 360,000 | 100 (k2 as shipped) | 0.10s | 0.01s | 10.0× |
      | 360,000 | 1,000 | 0.23s | 0.16s | 1.4× |
      | 360,000 | 10,000 | 1.55s | 1.62s | **1.0×** |
      | 40,000 | 100,000 | 1.66s | 1.77s | **0.9× (Helix ahead)** |

      So codegen for the inner loop is not the problem — at cap≥10,000 Helix matches or
      beats gcc, consistent with the earlier finding that Cranelift beats gcc on tight
      scalar `f64` loops. Subtracting the measured ~1.8 ns/iteration leaves ≈250 ns per
      pixel. RULED OUT by measurement, each at 4M calls: the scalar call path itself
      (~2.5 ns/call, recursion included), callee arity (2, 3 and 5 mixed args all
      ~2.5 ns), and the three-layer `grid`→`row`→`step` nesting (flattening to two layers
      is *worse*, 0.15s vs 0.11s). Every spelling reports as natively compiled (JIT 18–26×
      over the VM), so it is not a silent fall-back either.

      Note when re-measuring: gcc at `-march=native` contracts to FMA, so Helix and C
      iteration counts diverge slightly at high caps (86125823 vs 86125368 at cap=1000).
      That is the known mandelbrot FMA drift, not a Helix defect — but it means high-cap
      runs are not anchor-clean and must not be published as a like-for-like comparison.

## Cross-cutting principles to uphold at every phase
- Prefer dots over pipes; minimize operator symbols.
- One obvious way to perform each task.
- Immutable by default.
- Errors must be instructive.
- Zero-copy where possible; lazy where it is beneficial.

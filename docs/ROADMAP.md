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
- [ ] **A `#![deny]` gate for new `unwrap`/`expect` in interpreter paths** (or a
      clippy `disallowed_methods` config scoped to `src/interp`/`src/vm`) so the
      never-abort property is enforced by CI, not re-audited by hand.
- [ ] **Document NaN ordering** in the language docs (`sort` places NaN after
      `+inf`; reductions propagate `missing`) so the behavior is a contract, not an
      implementation detail.
- [ ] **`try`-on-VM error-recovery soak** — `TryBegin`/`TryOk`/`TryErr` unwinding
      under the fuzzer, composed with JIT bailouts mid-`try`.

## Cross-cutting principles to uphold at every phase
- Prefer dots over pipes; minimize operator symbols.
- One obvious way to perform each task.
- Immutable by default.
- Errors must be instructive.
- Zero-copy where possible; lazy where it is beneficial.

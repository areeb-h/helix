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

- [ ] **A user function with a FLOAT parameter, called from a map body, still does not
      compile.** Partially closed by Stage 3p below, which handles the `i64` callee; the
      remaining case is a callee whose parameters are `Float`:

      | spelling (20M elements) | JIT | VM | |
      | --- | --- | --- | --- |
      | `map(i => to_float(i) * 2.0 + 1.0)` inline | 0.02s | 1.63s | native, 82× |
      | `fn g(x: Float) = x * 2.0 + 1.0` + `map(i => g(to_float(i)))` | **1.47s** | 2.15s | **VM** |

      This one is NOT the same fix. There is deliberately no standalone `f64`
      specialization of a user function (see the note above `let kind = NumKind::Int` in
      `build`): a float-argument function can still return an `Int` — a literal, or an
      `Int`-only subexpression — so an `f64`-monomorphic codegen would diverge from the
      interpreter on RESULT TYPE, not just value. The machinery that solves this already
      exists for *functions* calling functions: `MixedSig` carries `params` + `ret`, and
      `infer_typed_env`/`gen_value_env` type and emit such calls. The map kernel cannot
      reach it because `mixed_sigs` is built inside `jit::build`, while the decision to
      emit a kernel (and its capture list) is made earlier, in the bytecode compiler,
      which knows only function NAMES (`jit_fns`, `func_names`) and no signatures.
      Closing it means giving the bytecode compiler a mixed-signature table — i.e. moving
      or duplicating `mixed_fn_sig`'s inference — which is a compiler↔JIT contract change,
      not a local edit. Worth it (this is `map(i => escape(...))`, the k2/mandelbrot
      shape), but it should be designed rather than bolted on.

- [ ] **Two spelling inversions still open** (two others from this sweep are now Stage 3r).
      At 10M elements; a declining JIT runs the bytecode loop, so "VM" means the JIT time
      equals it.

      | shape | JIT | its twin | |
      | --- | --- | --- | --- |
      | `map(i => i * c + 1).reduce(…)` — captured, fused chain | 0.35s | 0.00s inline | 3–7× only |
      | `scan(0, (s,x) => s + x)` | 0.54s | 0.00s `reduce` twin | **VM — `scan` has no kernel at all** |

      The fused chain declines its map stage when the body captures; the standalone map has
      handled captures since Stage 3m, so the pipeline is now the odd one out. `scan` is a
      different matter: it is a prefix fold, so element *i* depends on element *i−1*. That is
      a genuine serial dependency (like `filter`'s output offset), so it can have a native
      kernel but not a parallel one.

- [ ] **k2 mandelbrot: the loop body is at PARITY with C; the gap is a ~250 ns
      per-pixel fixed cost that is still unidentified.** Recorded because the obvious
      suspects are now ruled OUT and re-checking them would be wasted work.

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

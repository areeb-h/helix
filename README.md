# Helix

**A scientific programming language that reads like a notebook and runs its numeric kernels at C speed.**

Helix is a from-scratch language and runtime, written in Rust, for data science, machine
learning, computational biology, and high-performance scientific computing. It pairs a clean,
high-level surface syntax with a **Cranelift JIT** that compiles hot numeric loops to native
machine code — so a dot product or an N-body inner loop, written as a one-line `reduce`, runs
*at or beyond single-threaded C*, while the rest of your program stays as readable as a notebook.

What makes that trustworthy: every JIT-compiled kernel is proven **bit-identical** to two
independent reference engines (a bytecode VM and a tree-walker) by a differential oracle over
tens of thousands of random programs. **Speed is never traded for a silently wrong answer.**

[Performance](#performance) · [Install](#install) · [A tour](#a-tour) · [What's inside](#whats-inside) · [Architecture](#architecture)

---

## Performance

The JIT compiles pure-numeric `map` / `filter` / `reduce` kernels over ranges and packed
arrays — including **array-indexed reductions** (`a[j]`) and **nested reductions** (a `map` of a
`reduce`) — to native code, and auto-parallelizes across cores above a size threshold.

**Against single-threaded C, Helix loses more often than it wins — and that is the honest
headline.** From [`bench/kernels/RESULTS.md`](bench/kernels/RESULTS.md), ten kernels where
every language must print byte-identical output before a timing counts, on a
page-size-equalized machine:

| # | kernel | Helix | C | vs C |
|---|---|---|---|---|
| k1 | dot, 50M i64 | **0.08 s** (379% CPU) | 0.11 s | faster on wall clock, for **2.8× the CPU** |
| k2 | mandelbrot 1200² | 0.08 s (97%) | 0.08 s | **tie**, single-threaded |
| k3 | basel 1e8 | 0.09 s (96%) | 0.06 s | 1.5× slower |
| k5 | montecarlo 1e8 | 0.68 s (99%) | 0.28 s | 2.4× slower |
| k6 | sieve π(10⁷) | 0.02 s (89%) | 0.01 s | ~tie (delegated; 3.5× over NumPy) |
| k7 | wordcount 5M | 0.68 s (99%) | 0.23 s | 3.0× slower — but **2.4× faster than CPython** |
| k8 | matmul build + GEMM | **0.05 s** (155%) | — | **1.6× faster than NumPy** |

**Helix loses to C on seven of nine comparable kernels**, ties on k2, and leads k1 only by
spending 2.8× the cores. Where it does win — k8 against NumPy, k6 against NumPy, k7 against
CPython — those are the comparisons that reflect who actually uses this.

> **Two older benchmark documents overstate this.** `docs/jit-benchmarks.md` published C
> baselines that were **wrong by ~4.4×** — the C reference was not getting transparent huge
> pages — and every "≈ C" or "beats 1-thread C" conclusion drawn from it is void. That
> document is kept for its engineering history (what each JIT lever did) and carries a
> banner saying so. `RESULTS.md` is the page-fair suite and the only benchmark source that
> should be quoted.

DataFrame throughput (a 50M-row filter→group→sort→head in ~0.2 s from Parquet) is measured
separately in [docs/benchmarks.md](docs/benchmarks.md).

## Install

Helix ships as a **single self-contained binary** — no runtime to install (no Python, no system
BLAS; the core links nothing external beyond the C runtime). Measured on the gate profile,
stripped: the default build is **19.3 MB** and the **appliance profile**
(`cargo build --release --no-default-features --features appliance`, ADR 0032) is **12.5 MB**.

The default build carries Helix's **own** DataFrame engine (ADR 0033). Polars is no longer in
it: it stays behind `--features dataframes` as the **oracle** every native result is compared
against, because an engine cannot be its own evidence. That oracle build is **77.5 MB**
stripped — which is the size of what the default stopped carrying.

```sh
# macOS / Linux — downloads the binary for your platform and verifies its SHA-256:
curl -LsSf https://raw.githubusercontent.com/areeb-h/helix/main/install.sh | sh

# Windows (PowerShell, no admin needed):
irm https://raw.githubusercontent.com/areeb-h/helix/main/install.ps1 | iex

# or from a checkout, with a Rust toolchain:
cargo install --path .
```

```
$ curl -LsSf https://raw.githubusercontent.com/areeb-h/helix/main/install.sh | sh
helix-install: downloading https://github.com/areeb-h/helix/releases/latest/download/helix-x86_64-unknown-linux-gnu.tar.gz
helix-install: checksum ok (helix-x86_64-unknown-linux-gnu.tar.gz)
helix-install: installed helix -> /home/areeb/.local/bin/helix
helix 0.9.0
helix-install: done. try:  helix eval "print(1 + 2)"   or   helix repl
```

```sh
helix run script             # run a script (`.helix` optional)
helix eval "print(1 + 2)"    # a one-liner
helix repl                   # interactive session
helix check script.helix     # type-check without running (takes many paths; `--lint` for advice)
helix test [path]            # run *_test.helix files and `##` doc examples (`--engines` cross-checks all 3)
helix fmt script.helix       # format — no options, and it cannot change your program
helix effects script.helix   # what each function reaches: authority, and whether it is reproducible
helix doc [Type]             # a type's methods (Array/String/Dna/Connection/…) or `builtins`
helix search <term>          # find a capability by what it does, not by its name
helix describe [what]        # the whole API as JSON — a name, a Type, or everything
helix build script.helix     # bundle program + runtime into one executable (no toolchain needed)
helix emit-hbc script.helix  # compile to a .hbc bytecode container (portable core-bytecode artifact)
helix new / add / sync / verify   # manifest, dependencies, lockfile
helix help                   # all commands
```

The installer picks the static musl build automatically on musl distros (Alpine) and
on glibc older than the gnu build's floor (2.35); `HELIX_MUSL=1` forces it (verified
`static-pie linked`).
`HELIX_INSTALL_DIR` changes where it lands. A checksum **mismatch aborts** rather than
warning — the installer will not install what it cannot verify.

> **Use `v0.1.1` or later.** `v0.1.0` is published but nothing can be installed from it: its
> pipeline uploaded four of six platforms and no `SHA256SUMS`, so the installers correctly
> refuse even the platforms that did upload. All three causes are fixed rather than worked
> around — see [CHANGELOG.md](CHANGELOG.md). `releases/latest` resolves to the current release
> (`v0.9.0`), so the commands above pick it up without you doing anything.

## A tour

```python
# Immutable by default; `mut` is explicit. String interpolation needs no `f` prefix.
mean_score = scores.where(it >= 60).map(it + 5).mean()
print("adjusted mean: {mean_score}")

# `if` is an expression; functions are single expressions (recursion supported).
fn variance(xs) = let m = xs.mean(), n = xs.count() in xs.map((it - m) ** 2).sum() / n

# Records destructure by field; an absent field is `missing`, so an optional spec reads in one line.
fn render(spec) = let {where, limit} = spec in {where: where ?? "true", limit: limit ?? 100}

# Native-speed numeric kernels — this reduce JIT-compiles to a native loop.
dot = (range(0, n)).reduce(0.0, (acc, j) => acc + a[j] * b[j])

# DataFrames. Columns use `@` — always a column, never a variable, so the two can't
# collide. A bare name works inside a query too, and a String predicate is a predicate.
read_csv("patients.csv")
    .where(@age > 40 and @resting_hr < 75 and name.starts_with("A"))
    .select(@name, @diagnosis)
    .sort(@age)
    .write_csv("cohort.csv")

# Databases return frames, so the same verbs continue over the result. Parameters are
# VALUES (`$1`), never text spliced into the statement, and the session is read-only
# from its first byte — a write comes back as the server's own SQLSTATE, not a guess.
db = postgres_open("postgres://user:pw@host/db")   # TLS on by default, verified
db.query("select name, age from people where age > $1", [40])
    .where(@age < 55)
    .select(@name)

# A write is a different session and a different grant (ADR 0047): `db-write`, never just `net`.
db = postgres_open("postgres://user:pw@host/db", "write")
db.execute("insert into people (name, age) values ($1, $2) returning id", ["Ada", 36]).rows
postgres_execute("postgres://user:pw@host/db", "delete from people where age < $1", [18]).affected

# First-class genomics.
seq = dna("ATGCGTAC")
seq.gc_content()
seq.reverse_complement()
reads = read_fastq("reads.fq")       # → a queryable DataFrame

# An HTTP server is a pure handler folded over a request stream — no framework, no global state.
fn serve(listener) = do {
    conn = listener.accept()
    conn.respond(route(conn.request()))
    serve(listener)
}
serve(listen(8080))
```

More in [`examples/`](examples/), the full [stdlib reference](docs/reference.md) (generated, gate-verified), and the [language & DX guide](docs/syntax-and-dx.md).

## What's inside

### The language
- **Expression-oriented** — `if/then/else`, `match`, `let … in`, and comprehensions are all
  expressions that yield values. No statements-vs-expressions friction, no truthiness coercion.
- **Records, tuples, destructuring** — `{name: "Ada", age: 41}` with `.field` access, and
  `let {name, age} = person in …` — or `{name, age} = person` as a statement — to read
  fields (an absent one is `missing`, ADR 0046);
  `(a, b)` tuples that unpack (`q, r = divmod(17, 5)`) and destructure in lambda params
  (`pairs.map((k, v) => …)`). Record spread/update: `{ ...base, status: 500 }`.
- **Pattern matching** — `match` with literal, range (half-open, matched by magnitude), or-,
  guard, and binding patterns.
- **Dicts and UFCS** — string-keyed dict literals that spread into records; any user function is
  callable as a method on its first argument, and **which one a call means is decided by the
  RECEIVER, at run time** (ADR 0045). So `where`, `select`, `count` and `join` can be your own
  verbs on your own type while an array keeps its comprehension and a frame keeps its column
  verb — in the same file. A type that owns a name always keeps it; that is what makes a fluent
  library writable without a second way to define behaviour.
- **`missing`-safe by design** — a single dedicated absent value (not `NaN`), propagated through
  `.` access, method calls, and arithmetic, with three-valued boolean logic; `x ?? default`
  supplies a fallback. No `?.` operator needed.
- **Static type checking, permissively** — a fast bidirectional pass runs *before* execution and
  flags provable mistakes (`5 + "x"`, wrong arity, unknown method, non-boolean `if`) with
  caret-anchored, "did you mean…?" errors — but **never rejects a program that would run**, so
  dynamic and DataFrame code passes through untouched. Annotations are optional and checked.
- **One obvious way** — dot-chains over pipes, methods always with `()`, one assignment operator,
  `@column` sigils, euclidean `%` and `//`. Multi-line method chains, no line-continuations.

### Numeric compute & the JIT
- **Three execution engines** behind one language: a tree-walker, a **bytecode VM**, and a
  **Cranelift JIT**. The VM runs everything; the JIT accelerates the numeric hot paths it
  recognizes and transparently falls back for everything else. (The JIT is the default-on
  `jit` cargo feature; building without it changes no program's bytecode or output.)
- **What compiles to native code:** scalar recursion; `map`/`filter`/`reduce`/`scan` over `i64`
  and `f64` ranges and packed arrays; **array-indexed reductions** (`a[j]` and `a[i]` — dot
  products, weighted sums, **all-pairs distance/Hamming matrices**); **tuple/record accumulators**
  (mean+variance in one pass); and **parallel nested reductions** fanned out across cores.
- **Auto-parallel & auto-memoized** — large maps/reductions split across cores (rayon,
  order-preserving); pure overlapping recursion (e.g. naive Fibonacci) is memoized `O(2ⁿ)→O(n)`.
- **Tensors** — dense n-dimensional `f64` arrays with NumPy-style broadcasting, axis-wise
  reductions, `matmul`/`dot`, and pure-Rust linear algebra (`det`, `inv`, `solve`, `norm` — no
  BLAS dependency).

### Data
- **DataFrames** on Helix's **own engine** (ADR 0033, the default since v0.9.0): `read_csv`/
  `read_parquet`, in-memory `dataframe({…})` or `dataframe(dict)` for a runtime schema, then
  `where`/`filter`/`select`/`with`/`rename`/`sort`/`group` + aggregations, four join kinds
  (`inner`/`left`/`right`/`outer`, with the type from a literal or a `{how: kind}` record),
  `unique`/`vstack`/`head`/`tail`/`slice`/`column`/`columns`/`count`/`cache`,
  `drop_missing`/`drop_nan`, `write_csv`/`write_tsv`/`write_json`/`write_parquet` and
  `to_html`/`to_markdown`/`to_table`. Eager,
  deterministic, and following the language's own scalar semantics (ADR 0034) rather than a
  library's.
- **Polars is the oracle, not the engine.** It stays behind `--features dataframes`, and
  `scripts/dfdiff.sh` runs every tracked program under both backends and compares them cell by
  cell — an engine cannot be its own evidence, so the thing that says the replacement *means the
  same* outlives the thing it replaced. Flipping the default exposed three real divergences no
  test had covered. Measured at 1.6M rows on materialised frames with every output consumed,
  min-of-7 (polars → native): `group` 20.7 → **5.2 ms**, `join` 84.6 → **29.9**, `unique(col)`
  9.0 → **3.2**, `with` 49.0 → **19.8**, `sort` 74.5 → **38.1**, `where` 26.0 → **13.6**. That is
  our use of Polars on our workloads, not a general claim about Polars.
- **Databases return frames**, so the whole verb surface continues over a query result.
  **SQLite** bundled (ADR 0038) and **PostgreSQL** spoken directly over the wire (ADR 0044,
  `--features postgres`) with **zero new dependencies** — the v3 protocol has been frozen since
  2003 and SCRAM-SHA-256 needs only primitives already in the tree. Parameters are values, never
  interpolated text; the session is read-only from the startup packet; and **TLS is on by default
  and the server cannot turn it off** — two modes where `libpq` has six, because its default
  (`prefer`, which Go's `pgx` shares) continues in plaintext whenever something on the path says
  so.
- **Math standard library** that broadcasts over arrays/tensors and propagates `missing`: the full
  transcendental/rounding set plus `hypot`, `atan2`, constants, and the `**` power operator.

### Scientific computing & genomics
- **First-class DNA** with an IUPAC-aware `Dna` type: `gc_content`, `complement`/`reverse_complement`,
  `kmers(k)` (canonical spectrum), `windows(k)`, Hamming distance, base counts — the hot paths
  are SIMD/byte-level native (memory-bandwidth-bound).
- **Bioinformatics readers** into queryable DataFrames: `read_fasta`/`read_fastq` sequences,
  `read_vcf`/`read_bcf` variants, `read_sam`/`read_bam` alignments, `read_gff`/`read_bed`
  (via the noodles crates).

### Web: servers, streaming, clients
- **HTTP server** as a pure `(request) → response` handler you fold over a request stream — no
  framework, no global app state, native DataFrame/record/DNA → JSON. Custom headers, redirects,
  cookies/CORS, and **Server-Sent-Events streaming** (`conn.sse()` / `conn.send()`).
- **Concurrency by *not* sharing** — a cooperative event loop within a core (`poll()`), and
  share-nothing **`SO_REUSEPORT` sharding** across cores (no locks, no `Arc`, nothing crosses a
  thread) — the ScyllaDB/Redpanda thread-per-core architecture Helix's immutable core is built for.
- **HTTP client, secure by default** (ADR 0031) — `http_get`, POST with methods/body, and
  pull-based streaming (`http_stream`). Header injection is refused in both directions; headers
  are a case-insensitive `Headers` type that keeps wire order and repeats; per-request
  `total_ms`/`connect_ms`/`read_ms`/`max_body` limits; redirects strip `Authorization`/`Cookie`
  on origin change, never downgrade https, refuse non-http(s) schemes, cap at 10 hops, and
  return the chain as data; cookies live in an explicit jar with Public-Suffix-List
  supercookie defence.
- **Real-time** — `emit` (flushed NDJSON sink) + `sleep` compose into paced live streams.

### Cryptography
Native, audited-crate-backed primitives, misuse-resistant by construction: `sha256`,
`hmac_sha256`, `base64`/`hex` encode/decode, **AES-256-GCM** (fresh nonce per call, authenticated),
and **Ed25519** (deterministic, strict verification). Enough to sign a JWT or seal a payload
end-to-end in pure Helix.

### Safety & correctness
- **The differential oracle** — the JIT, the bytecode VM, and the tree-walker are asserted
  **bit-identical** on tens of thousands of randomly generated programs. A JIT miscompilation
  cannot ship silently; an out-of-bounds native read falls back to the exact checked interpreter
  error. This is the project's cardinal rule.
- **Memory-safe** (inherited from the Rust host) **and authority-confined** — a **capability
  sandbox** (ADR 0021) enforced at one registry chokepoint and carried per-evaluation, so
  self-generated code gets a *narrower* grant. It is off unless asked for, and asking is a
  `[capabilities]` table in `helix.toml`: **present means enforced**, it is a ceiling rather than
  a request, a dependency cannot widen it, and `helix build` bakes it into the artifact where no
  environment variable can reopen it. `helix effects` reports what a program actually reaches, so
  the ceiling can be measured before it is declared. See
  [docs/memory-safety.md](docs/memory-safety.md).

### Packaging & interop
- **`helix build`** copies the interpreter and appends your program as an overlay → a standalone
  executable that runs with no toolchain and no Helix on `PATH`.
- **`.hbc` (Helix Bytecode Container)** — `helix emit-hbc` lowers a program's core subset to a
  portable core-bytecode artifact that runs on ctype's `no_std`, ring-0 `hvm` VM. It's the first
  artifact format Helix emits beyond the bundled executable.
- **CPython interop** (feature-gated) calls into NumPy, Polars, and friends — see
  [docs/python-interop.md](docs/python-interop.md).

## Architecture

Helix keeps **three engines on purpose**, as defense in depth:

- the **tree-walker** (`src/interp.rs`) — a direct AST executor, the independent correctness
  reference and the REPL backend;
- the **bytecode VM** (`src/vm.rs`) — the production host; runs the whole language;
- the **Cranelift JIT** (`src/jit.rs`) — compiles the numeric kernels the VM dispatches into,
  falling back to the VM for anything it doesn't handle.

Because the tree-walker is written independently of the VM/JIT, a shared VM/JIT bug still fails the
`JIT == VM == tree-walker` differential test — the safety net is *another implementation*, not more
tests of the same one. Details in [docs/execution-engine.md](docs/execution-engine.md).

## Design principles

| Principle | How it shows up |
|---|---|
| Read like a notebook, run like a compiler | high-level syntax; numeric kernels JIT to native |
| One obvious way | dot-chains (no `\|>`), methods always `()`, one assignment operator, `@column` |
| Immutable by default | `mut` is required to mutate; values share via `Rc` (zero-copy clones) |
| Correctness is not negotiable | three engines held bit-identical by a differential oracle |
| Honest about limits | measured benchmarks with caveats; permissive types never over-reject |
| Safe by default | memory-safe host + a denied-by-default capability model |

## Status & roadmap

A mature implementation, **not a prototype**: **848 tests** (481 library + 332 CLI + 3 native-df
+ 32 dual-engine differential), plus 140 programs run under both DataFrame backends and a
whole-tree type-check over 98 files — zero compiler warnings, with a differential oracle and a VM/tree-walker
parity gate on every change. Phase status (full plan in
[docs/ROADMAP.md](docs/ROADMAP.md)):

1. **Core language & interpreter** — done
2. **Type checker & module system** — done (package manager pending)
3. **DataFrame engine** (Helix's own, default since v0.9.0; polars retained as the oracle) — done
4. **Tensor engine & linear algebra** — done
5. **JIT compilation** — done for numeric kernels (coverage expanding; SIMD is the next lever)
6. **GPU support** — future

## Building

```sh
cargo build --release
./target/release/helix examples/language/tour.helix
```

Requires a recent Rust toolchain. For fast iteration during development, `scripts/gate.sh` runs
clippy + the full test suite + the VM/tree-walker parity diff + `dfdiff` across both DataFrame
backends + the whole-tree type-check, on an optimized-but-fast profile. **Do not use
`cargo test --release`** — that profile's fat LTO links noodles and Cranelift as one LLVM unit
(and polars too, on a `--features dataframes` build), which costs about twenty minutes per build
and changes no test's outcome.

## The formatter

`helix fmt` **cannot change your program, and that is checked rather than hoped.** It never
runs the parser: it reads the token stream, re-emits each token's source bytes verbatim, and
decides only the whitespace between them — so `lex(fmt(x))` equals `lex(x)` token-for-token,
asserted over every `.helix` file in this repository as a property test.

Three consequences follow from that one constraint, and each is a thing another language's
formatter gets wrong:

- **It never reflows, and never edits a byte inside a comment.** The author owns line breaks
  and prose; fmt owns indentation and inter-token spacing. This project already refuses
  `cargo fmt` for exactly the failure that avoids — 1280 diffs, mostly re-indented
  hand-wrapped comments. Aligned columns survive too: where a space is required you may use
  more, and fmt keeps what you wrote.
- **It formats a file that does not parse.** It only needs the file to lex. That is the
  moment a formatter is most useful and the moment prettier, rustfmt, black and gofmt all
  refuse — mid-edit.
- **It has no configuration.** No config file, no width flag, no `# fmt: off`. Every other
  tool's escape hatch exists to get away from reflowing, and there is nothing here to escape.

There are no stylistic lints anywhere in the toolchain, by construction: style is not a
choice, so it cannot be a warning. `helix check` covers meaning, `helix fmt` covers layout,
and the two do not overlap.

## Contributing

The website lives in [areeb-h/helix-site](https://github.com/areeb-h/helix-site), which tracks
this repository as a submodule and renders `docs/`, `examples/` and `bench/kernels/RESULTS.md`
verbatim — so a docs change here is a site change there, with no copy in between.

[CONTRIBUTING.md](CONTRIBUTING.md) covers the gate, the one non-negotiable rule (three engines,
byte-identical, on values *and* error text), and what a good change looks like. Security issues
go through [SECURITY.md](SECURITY.md) instead — privately, via GitHub's advisory form.
Release history, including what went wrong with `v0.1.0`, is in [CHANGELOG.md](CHANGELOG.md).

Licensed [MIT](LICENSE).

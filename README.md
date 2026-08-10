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

A 50-million-element **dot product** (best of 3, wall-seconds; every language prints the
identical result):

| dtype | **Helix (JIT)** | C `-O3` | Rust | Go | NumPy | CPython |
|---|---|---|---|---|---|---|
| `i64` | **~0.34 s** | 0.47 | 0.47 | 0.46 | 0.94 | 9.4 |
| `f64` | **0.36 s** | 0.47 | 0.47 | 0.48 | 1.5 | 9.4 |

A 900-million-pair **O(N²) all-pairs reduction** (`range(n).map(i => range(n).reduce(…))` — the
distance-matrix / N-body shape):

| | **Helix (JIT)** | C `-O3` (1 thread) | C (OpenMP) | Go | NumPy | interpreted |
|---|---|---|---|---|---|---|
| wall | **0.06 s** (~569% CPU) | 0.08 | 0.01 | 0.17 | 0.25 | ~12 s |

The single-threaded reduction is memory-bandwidth-bound *at parity with C*; Helix pulls ahead on
totals by **auto-parallelizing** array construction and outer loops across cores — a real win, but
an honest one: the C/Rust/Go baselines here are single-threaded, and Helix does not yet
auto-vectorize (SIMD), which is why threaded+vectorized C-OpenMP is still faster on the last kernel.
Array-indexed all-pairs — a genuine **distance matrix** over encoded data (`abs(codes[i]-codes[j])`,
for phylogenetics / clustering) — runs a native inner reduce *and* now parallelizes its outer loop:
**225M pairs in 0.03 s, edging single-threaded SIMD C (0.04 s)** and ~700× over the interpreter.
Honestly, that's a multi-core win, not a per-core one — Helix uses all cores where C here is one
thread; a threaded+vectorized C would still lead, since Helix's JIT doesn't auto-vectorize yet (the
one remaining lever, and a safe one for integer kernels).
Full methodology, per-language source, and the load-bearing caveats:
**[docs/jit-benchmarks.md](docs/jit-benchmarks.md)**. DataFrame throughput (a 50M-row
filter→group→sort→head in ~0.2 s from Parquet) is measured separately in
[docs/benchmarks.md](docs/benchmarks.md).

## Install

Helix ships as a **single self-contained binary** — no runtime to install (no Python, no system
BLAS; the core links nothing external beyond the C runtime). It's ~60 MB because it embeds the
Polars engine, yet starts instantly.

```sh
# from a checkout, with a Rust toolchain — the path that works TODAY:
cargo install --path .

# once the repo is public and a release is published, on macOS/Linux:
curl -LsSf https://raw.githubusercontent.com/areeb-h/helix/main/install.sh | sh
# …and on Windows (PowerShell, no admin needed):
irm https://raw.githubusercontent.com/areeb-h/helix/main/install.ps1 | iex
```

```sh
helix run script.helix       # run a script
helix eval "print(1 + 2)"    # a one-liner
helix repl                   # interactive session
helix build script.helix     # compile to a standalone executable (no toolchain needed)
helix emit-hbc script.helix  # compile to a .hbc bytecode container (portable core-bytecode artifact)
helix help                   # all commands
```

> **The one-line installs need the repository to be PUBLIC, not just released.** Verified
> 2026-08-10 against the live repo: while it is private, both
> `raw.githubusercontent.com/.../install.sh` and
> `github.com/.../releases/latest/download/...` return **404** to an unauthenticated
> client — so the commands above cannot work for anyone but the owner, no matter how many
> releases are tagged. Publishing also removes the other blocker: GitHub Actions is free
> with unlimited standard-runner minutes on public repositories, and the `v0.1.0` release
> run was cancelled by an Actions **billing** failure rather than by anything in the build.
>
> The release pipeline itself is verified end-to-end locally: the 64 MB binary packages to
> a 21 MB tarball, `SHA256SUMS` is produced in the exact format the installers parse, and
> the installer's verifier accepts the real artifact while rejecting both a one-byte
> corruption and a missing checksum entry. Nothing is waiting on code.

## A tour

```python
# Immutable by default; `mut` is explicit. String interpolation needs no `f` prefix.
mean_score = scores.where(it >= 60).map(it + 5).mean()
print("adjusted mean: {mean_score}")

# `if` is an expression; functions are single expressions (recursion supported).
fn variance(xs) = let m = xs.mean(), n = xs.count() in xs.map((it - m) ** 2).sum() / n

# Native-speed numeric kernels — this reduce JIT-compiles to a native loop.
dot = (range(0, n)).reduce(0.0, (acc, j) => acc + a[j] * b[j])

# DataFrames (Polars/Arrow, lazy). Columns use `@` — always a column, never a variable,
# so the two can't collide, and the whole chain lowers to one native Polars query.
read_csv("patients.csv")
    .where(@age > 40 and @resting_hr < 75)
    .select(@name, @diagnosis)
    .sort(@age)
    .write_csv("cohort.csv")

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

More in [`examples/`](examples/) and the [language & DX guide](docs/syntax-and-dx.md).

## What's inside

### The language
- **Expression-oriented** — `if/then/else`, `match`, `let … in`, and comprehensions are all
  expressions that yield values. No statements-vs-expressions friction, no truthiness coercion.
- **Records, tuples, destructuring** — `{name: "Ada", age: 41}` with `.field` access;
  `(a, b)` tuples that unpack (`q, r = divmod(17, 5)`) and destructure in lambda params
  (`pairs.map((k, v) => …)`). Record spread/update: `{ ...base, status: 500 }`.
- **Pattern matching** — `match` with literal, or-, guard, and binding patterns.
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
  recognizes and transparently falls back for everything else.
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
- **DataFrames** backed by **Polars/Arrow**, held lazily: `read_csv`/`read_parquet`, in-memory
  `dataframe({…})`, then `where`/`select`/`sort`/`group` + aggregations, `write_csv`/`write_parquet`
  and `to_html`/`to_markdown`. A chain builds a single query plan and materializes once, delegated
  to Polars' columnar, multi-threaded execution with predicate/projection pushdown.
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
- **HTTP client** — `http_get`, POST with methods/body, and pull-based streaming (`http_stream`).
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
- **Memory-safe** (inherited from the Rust host) **and authority-confined** — a
  **capability sandbox** (denied-by-default filesystem/network authority, enforced at one registry
  chokepoint, carried per-evaluation so self-generated code gets a *narrower* grant) is rolling out
  `audit → enforce`. See [docs/memory-safety.md](docs/memory-safety.md).

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

A mature implementation, **not a prototype**: ~420 tests, zero compiler warnings, a
differential oracle and VM/tree-walker parity gate on every change. Phase status (full plan in
[docs/ROADMAP.md](docs/ROADMAP.md)):

1. **Core language & interpreter** — done
2. **Type checker & module system** — done (package manager pending)
3. **DataFrame engine** (Polars/Arrow) — done
4. **Tensor engine & linear algebra** — done
5. **JIT compilation** — done for numeric kernels (coverage expanding; SIMD is the next lever)
6. **GPU support** — future

## Building

```sh
cargo build --release
./target/release/helix examples/language/tour.helix
```

Requires a recent Rust toolchain. For fast iteration during development, `scripts/gate.sh` runs
clippy + the full test suite + the VM/tree-walker parity diff on an optimized-but-fast profile.

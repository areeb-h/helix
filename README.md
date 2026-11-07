# Helix

**A scientific programming language that reads like a notebook and compiles its numeric kernels to native code.**

Helix is a from-scratch language and runtime, written in Rust, for data science, machine
learning, computational biology, and high-performance scientific computing. It pairs a clean,
high-level surface syntax with a **Cranelift JIT** that compiles hot numeric loops to native
machine code — so a dot product or an N-body inner loop, written as a one-line `reduce`, runs
within a small factor of single-threaded C ([measured below](#performance)), while the rest of
your program stays as readable as a notebook.

What makes that trustworthy: the JIT is **differentially tested for bit-identical results**
against two independent reference engines (a bytecode VM and a tree-walker) — tens of thousands
of generated programs per test run, plus every example and a pinned corpus on all three engines,
exit status included. That is strong evidence rather than a proof: it covers the shapes the
generators and the corpus exercise. The rule it enforces is that **any divergence is a bug**, and
speed is never knowingly traded for a different answer.

[Performance](#performance) · [Accuracy](#accuracy) · [Install](#install) · [A tour](#a-tour) · [What's inside](#whats-inside) · [Architecture](#architecture)

---

## Performance

The JIT compiles pure-numeric `map` / `filter` / `reduce` kernels over ranges and packed
arrays — including **array-indexed reductions** (`a[j]`) and **nested reductions** (a `map` of a
`reduce`) — to native code, and auto-parallelizes across cores above a size threshold.

**Against single-threaded C, Helix loses more often than it wins — and that is the honest
headline.** These rows are copied from [`bench/kernels/RESULTS.md`](bench/kernels/RESULTS.md),
which is the only source to quote: one run on **2026-08-13 at `3f91177`** (Ryzen 7 7700X, WSL2,
release build), with every language required to print byte-identical output before a timing
counts, on a page-size-equalized machine. It has not been re-measured since, and the file's own
rule is to rebuild and re-run before quoting newer numbers.

| # | kernel | Helix | C | vs C |
|---|---|---|---|---|
| k1 | dot, 50M i64 | **0.25 s** (417% CPU) | 0.30 s | 1.2× faster on wall clock, for **4× the CPU** |
| k2 | mandelbrot 1200² | 0.08 s (101%) | 0.08 s | **tie**, single-threaded |
| k3 | basel 1e8 | 0.09 s (105%) | 0.05 s | 1.8× slower |
| k4 | allpairs 15k | 0.01 s (305%) | 0.00 s | at timer grain — not a measurement |
| k5 | montecarlo 1e8 | 0.63 s (105%) | 0.26 s | 2.4× slower |
| k6 | sieve π(10⁷) | 0.02 s (100%) | 0.01 s | ~tie (delegated; 3× over NumPy) |
| k7 | wordcount 5M | 0.67 s (105%) | 0.20 s | 3.4× slower — but **2.1× faster than CPython** |
| k8 | matmul 1024³ (build + GEMM) | **0.03 s** (177%) | — | **2.3× faster than NumPy** |
| k9 | matmul 512³ (naive) | 0.49 s (105%) | 0.33 s | 1.5× slower |
| k9m | matmul 512³ (map-temp) | 0.51 s (105%) | 0.33 s | 1.5× slower |

**Of the eight kernels with a C time above timer grain, Helix is slower on five** (k3, k5, k7, k9,
k9m), ties on two (k2, k6), and leads k1 only by spending 4× the CPU. Where it does win — k8 and
k6 against NumPy, k7 against CPython — those are the comparisons that reflect who actually uses
this.

> **Two older benchmark documents overstate this.** `docs/jit-benchmarks.md` published C
> baselines that were **wrong by ~4.4×** — the C reference was not getting transparent huge
> pages — and every "≈ C" or "beats 1-thread C" conclusion drawn from it is void. That
> document is kept for its engineering history (what each JIT lever did) and carries a
> banner saying so. `RESULTS.md` is the page-fair suite and the only benchmark source that
> should be quoted.

DataFrame throughput (a 50M-row filter→group→sort→head in ~0.2 s from Parquet) is measured
separately in [docs/benchmarks.md](docs/benchmarks.md).

## Accuracy

Agreeing with itself on three engines does not make an answer right. For that, Helix is
scored against answers it did not compute: NIST's [Statistical Reference
Datasets](https://www.itl.nist.gov/div898/strd/), the files statistical software is audited
with, whose results NIST certified in 500-digit arithmetic.
[`examples/statistics/nist.helix`](examples/statistics/nist.helix) reads the files exactly as
NIST publishes them, fits and summarizes them, and counts the significant digits that agree
with each certified value (the LRE of McCullough, 1998). In brackets is the most **any**
double-precision program can get on the same inputs, computed in exact rational arithmetic by
[`ceiling.py`](examples/data/nist/ceiling.py), which shares no code with Helix:

| dataset | difficulty | coefficients | std errors | R² |
|---|---|---|---|---|
| Norris | lower | 14.1 (14.1) | 13.9 (13.9) | 15.0 (15.0) |
| Pontius | lower | 13.5 (13.5) | 13.8 (13.8) | 15.0 (15.0) |
| Longley | higher | 14.6 (14.6) | 14.5 (14.9) | 15.0 (15.0) |
| Filip | higher | 7.6 (7.6) | 7.4 (7.6) | 11.1 (11.8) |
| Wampler1 | higher | 15.0 (15.0) | 15.0 (15.0) | 15.0 (15.0) |
| Wampler5 | higher | 15.0 (15.0) | 13.7 (14.5) | 14.8 (15.0) |

Twenty datasets in all (eleven regressions, nine summary-statistics files) — every certified
statistic within one digit of the ceiling, on all three engines; the program fails, and CI
with it, if one falls short. Filip's 7.6 is the ceiling, not a shortfall: its degree-10
polynomial is so ill-conditioned that rounding `x ** 10` to a double already costs the rest.

Running it the first time is what made this table true. Regression solved the normal
equations, which square the problem's condition number: **0** correct digits on Filip (a NaN
standard error), 6 on Wampler, 7 on Longley, and a "collinear" refusal for a well-posed fit
whose `x` was in units of 10⁻⁷. It is now Householder QR with Björck's iterative refinement,
and a no-intercept R² is the uncentered one NIST, R and SAS report — see the
[changelog](CHANGELOG.md).

## Install

Helix ships as a **single self-contained binary** — no runtime to install (no Python, no system
BLAS; the core links nothing external beyond the C runtime). Measured on the gate profile,
stripped (2026-11-02): the default build is **21.1 MB** and the **appliance profile**
(`cargo build --release --no-default-features --features appliance`, ADR 0032) is **14.0 MB**.

The default build carries Helix's **own** DataFrame engine (ADR 0033). Polars is no longer in
it: it stays behind `--features dataframes` as the **oracle** every native result is compared
against, because an engine cannot be its own evidence. That oracle build is **82.2 MB**
stripped — which is the size of what the default stopped carrying. SQLite (`--features db`)
and PostgreSQL (`--features postgres`) are opt-in builds too; `has_feature(name)` lets a
program ask which it is running on.

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
helix 0.13.0
helix-install: done. try:  helix eval "print(1 + 2)"   or   helix repl
```

```sh
helix run script             # run a script (`.helix` optional)
helix eval "print(1 + 2)"    # a one-liner
helix repl                   # interactive session
helix check script.helix     # type-check without running (takes many paths; `--lint` for advice)
helix test [path]            # run *_test.helix files and `##` doc examples (`--engines` cross-checks all 3)
helix fmt script.helix       # format — no style options (`--check` reports), and it cannot change your program
helix effects script.helix   # what each function does and what it carries: authority (or unknown), reproducibility
helix jit-explain script     # which numeric kernels the JIT compiled, by line (`--json` too)
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
> (`v0.13.0`), so the commands above pick it up without you doing anything.

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

# A condition is a value (ADR 0052): outside a frame verb `@age > lo` is the record that
# describes it — `{kind: "bin", op: ">", left: {kind: "col", name: "age"}, right: {kind: "lit", value: lo}}`
# — so a library can render it (`age > $1`), a frame takes it back by name, and the
# load-time specializer folds a library's rendering of it to a constant.
adult = @age > 18 and not @name.starts_with("test")
cohort.where(adult)                                 # the frame reads the value
model.sql({where: adult, order: ["-age"]})          # an ORM renders it: `where age > $1 and not name like $2`

# Databases return frames, so the same verbs continue over the result. Parameters are
# VALUES (`$1`), never text spliced into the statement, and the session is read-only
# from its first byte — a write comes back as the server's own SQLSTATE, not a guess.
# (PostgreSQL and SQLite are opt-in builds — `--features postgres` / `--features db` —
# and not in the installed binary; `has_feature("postgres")` asks before calling.)
db = postgres_open("postgres://user:pw@host/db")   # TLS on by default, verified
db.query("select name, age from people where age > $1", [40])
    .where(@age < 55)
    .select(@name)

# A write is a different session and a different grant (ADR 0047): `db-write`, never just `net`.
rw = postgres_open("postgres://user:pw@host/db", "write")
rw.execute("insert into people (name, age) values ($1, $2) returning id", ["Ada", 36]).rows
postgres_execute("postgres://user:pw@host/db", "delete from people where age < $1", [18]).affected

# Several statements in ONE round trip (and one transaction): an Array in, an Array of frames out.
[person, posts] = db.query([{sql: "select * from people where id = $1", params: [7]},
                            {sql: "select * from posts where author_id = $1", params: [7]}])

# A transaction is a VALUE: dropped without `commit()` — an error unwinding past it — it rolls back.
tx = rw.begin()
tx.execute("update accounts set balance = balance - $1 where id = $2", [10, 1])
tx.execute("update accounts set balance = balance + $1 where id = $2", [10, 2])
tx.commit()

# A result larger than memory is read a page at a time — the plan `query` would run, one
# round trip a page, and an empty frame once it has been read to its end.
cur = db.cursor("select * from events where day = $1", ["2026-09-26"], 50000)
fn buys(cur, n) = let page = cur.next() in
  if page.count() == 0 then n else buys(cur, n + page.where(@kind == "buy").count())
buys(cur, 0)

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
  fields (an absent one is `missing`, ADR 0046); `let [a, b] = pair in …`, `[head, ...tail]
  = xs` and `q, r = (17 // 5, 17 % 5)` to unpack a tuple or an array by position (ADR 0053);
  `(a, b)` tuples that destructure in lambda params (`pairs.map((k, v) => …)`) and take
  defaults (`(x, n = 10) => …`). Record spread/update: `{ ...base, status: 500 }`.
- **Pattern matching** — `match` with literal, range (half-open, matched by magnitude), or-,
  guard, and binding patterns.
- **Dicts and UFCS** — dicts keyed by strings, ints, bools or DNA, with O(log n) lookup and
  `insert`/`remove` that return a new dict in O(1) amortized, so building or draining one in a
  fold is linear (string-keyed literals spread into records); any user function is
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
  **SQLite** bundled (ADR 0038, `--features db`) and **PostgreSQL** spoken directly over the
  wire (ADR 0044, `--features postgres`) with **zero new dependencies**. Both are opt-in
  builds — the release binaries the installers fetch carry neither, and a call says what to
  rebuild with — the v3 protocol has been frozen since
  2003 and SCRAM-SHA-256 needs only primitives already in the tree. Parameters are values, never
  interpolated text; the session is read-only from the startup packet; and **TLS is on by default
  and the server cannot turn it off** — two modes where `libpq` has six, because its default
  (`prefer`, which Go's `pgx` shares) continues in plaintext whenever something on the path says
  so. Transactions are values that roll back when dropped uncommitted, a result larger than
  memory is read a page at a time (`c.cursor(…)`), several statements share one round trip,
  and a login is bound to the TLS session it runs over (SCRAM-SHA-256-PLUS).
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

### Apps
- **`import std.ui`** (ADR 0055) — an app is three values: `init`, `update(model, event)` and
  `view(model)`. Views are data, rendered and diffed in Rust and patched into the page, so a
  frame touches only what changed; motion is declared in the view and run by the GPU.
- **Components accessible by construction**: forms with validation, tabs, menus, dialogs,
  toasts, tooltips, charts and a virtualised DataFrame table, in light and dark — checked with
  axe-core in CI. Each run serves on localhost behind a session token.
- **Hot reload**: `helix run --watch app.helix` shows each save in the open page with the
  app's state kept; a save that does not type-check is reported and the app keeps running.
- **A native window** (built with `--features desktop`): `ui.app({…, window: true})` shows the
  app in its own window on the platform's web view instead of a browser tab — and only the
  app's own page, nothing it could be navigated away to.
- **Testable without a browser**: `ui.replay(spec, events)` returns every screen as HTML, the
  same on all three engines. The library is compiled into `helix`, so a 10-line app runs from
  any directory:

```helix
import std.ui

fn view(n) = ui.column([ui.heading("Count: {n}"), ui.button("+", "inc", variant: "primary")])
fn update(n, e) = if e.msg == "inc" then n + 1 else n

ui.app({title: "Counter", init: 0, update: update, view: view})   # prints a link to open
```

See `examples/apps/gallery.helix` for every component.

### Cryptography
Native, audited-crate-backed primitives, misuse-resistant by construction: `sha256`,
`hmac_sha256`, `base64`/`hex` encode/decode, **AES-256-GCM** (fresh nonce per call, authenticated),
and **Ed25519** (deterministic, strict verification). Enough to sign a JWT or seal a payload
end-to-end in pure Helix.

### Safety & correctness
- **The differential oracle** — the JIT, the bytecode VM, and the tree-walker are checked for
  **bit-identical** results on tens of thousands of randomly generated programs per run, and on
  every runnable example and corpus program (output and exit status). A miscompilation in a
  shape those cover fails CI rather than shipping; an out-of-bounds native read falls back to
  the exact checked interpreter error. Engine agreement is the project's cardinal rule.
- **Memory-safe** (inherited from the Rust host) **and authority-confined** — a **capability
  sandbox** (ADR 0021) enforced at one registry chokepoint and carried per-evaluation, so
  self-generated code gets a *narrower* grant. It is off unless asked for, and asking is a
  `[capabilities]` table in `helix.toml`: **present means enforced**, it is a ceiling rather than
  a request, a dependency cannot widen it, and `helix build` bakes it into the artifact where no
  environment variable can reopen it. `helix effects` reports what each function does and what
  the values it builds can do — `unknown` where a callee cannot be seen, never silently pure — so
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
| Correctness is not negotiable | three engines differentially tested for bit-identical results |
| Honest about limits | measured benchmarks with caveats; permissive types never over-reject |
| Safe by default | memory-safe host + a denied-by-default capability model |

## Status & roadmap

A mature implementation, **not a prototype**: **1,062 tests** (621 library + 406 CLI + 3
native-df + 32 dual-engine differential), plus 168 programs run under both DataFrame backends
and a whole-tree type-check over 107 files — zero compiler warnings, with a differential oracle and a VM/tree-walker
parity gate on every change. Phase status (full plan in
[docs/ROADMAP.md](docs/ROADMAP.md)):

1. **Core language & interpreter** — done
2. **Type checker, module system & packages** — done (`helix.toml` manifests and
   workspaces, hash-pinned `helix.lock` via `helix add`/`sync`/`verify`)
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

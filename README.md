# Helix

> The language scientists wish they had, instead of the one they learned to tolerate.

Helix is a modern scientific programming language built in Rust, optimized for
data science, machine learning, AI, computational biology, and high-performance
scientific computing.

It aims to combine **Python's readability**, **R's data workflow**, **Rust's
safety**, **Julia's scientific elegance**, **SQL's intuitive data operations**,
and **Arrow's zero-copy memory model** — without inheriting their footguns.

## Install

Helix is a **single self-contained binary** — no runtime to install (no Python, no
system BLAS; the core links nothing external).

```sh
# one-line install — downloads the prebuilt binary for your platform,
# or falls back to a source build if no release is available
curl -LsSf https://raw.githubusercontent.com/areeb/helix/main/install.sh | sh

# or, with Rust installed, from a checkout:
cargo install --path .
```

Then use it like any language CLI:

```sh
helix run examples/tour.helix     # run a script
helix eval "print(1 + 2)"         # a one-liner
helix repl                        # interactive session
helix help                        # all commands
```

> Prebuilt one-line installs activate once the project is published on GitHub with a
> release tag (the [release workflow](.github/workflows/release.yml) is wired and
> ready); until then the installer source-builds, which needs Rust. The
> distribution plan — and how it aims to beat npm/pip/Mojo on the install
> experience — is [ADR 0009](docs/adr/0009-distribution-and-install.md).

## Status

Well past a prototype: a tree-walking interpreter **plus** a bytecode VM and a
Cranelift JIT (native code that beats Node/Python on scalar recursion), lazy
Polars/Arrow **DataFrames**, ndarray **tensors** with linear algebra, a static type
checker, a **module system**, **data access** (`http_get` + `parse_json`/`to_json`
for REST APIs), and a feature-gated **CPython interop** layer (call NumPy/polars/etc.
— see [docs/python-interop.md](docs/python-interop.md)). 130+ tests, zero warnings. The remaining roadmap (GPU, package manager, bundled
Python) is below.

## What works today

```text
# Immutable by default; mutability is explicit
x = 42
mut counter = 0
counter = counter + 1

# Arrays + statistics, no bracket soup
xs = [1, 2, 3, 4]
xs.mean()
xs.std()
xs.normalize()

# Multi-line dot-chains, no pipes or line-continuations
[10, 5, 8, 3, 9]
    .sort()
    .reverse()

# Comprehensions: `it` is the current element. `where` == `filter`.
scores
    .where(it >= 60)
    .map(it + 5)
    .mean()

# `if` is an expression that yields a value
grade = if score > 90 then "A" else "B"

# DataFrames (Polars/Arrow-backed) — the SAME `where`/`sort` verbs as arrays.
# `where(age > 40)` lowers to a native Polars filter, not an interpreter loop.
patients = read_csv("patients.csv")
patients
    .where(age > 40 and resting_hr < 75)
    .select(name, diagnosis)
    .sort(age)
genes.group(species).mean(expression)

# DNA sequences as a first-class type
seq = dna("ATGCGTAC")
seq.gc_content()
seq.reverse_complement()
seq.kmers(3)
```

### Language features in v0.1
- **Records** — structured data with named fields (Python dict + TS object):
  `{name: "Ada", age: 41}`, accessed with `.name` (no parens; `.method()` needs
  parens, so they never collide). Nested, arrays-of-records, and
  function-returning-record all work and are **type-checked** — a field typo is a
  compile-time error with a suggestion. Trailing commas allowed.
- **Local bindings** — `let a = x, b = y in expr` introduces intermediate values
  (sequential; scoped to the body). The natural way to write a function with
  steps: `fn variance(xs) = let m = xs.mean(), n = xs.count() in xs.map((it - m)
  ** 2).sum() / n`. (Helix uses `let … in` rather than indented blocks because
  indentation would collide with multi-line dot-chains — see ADR-0004.)
- **Tuples + destructuring** — `(a, b)` groups fixed-size values; `a, b = pair`
  unpacks them. Functions return several values cleanly: `q, r = divmod(17, 5)`.
  `zip`/`enumerate` now yield real tuples (`("Ada", 41)`), not 2-element arrays,
  and **lambda parameters destructure them**:
  `names.zip(ages).map((name, age) => "{name} ({age})")`. Destructuring length is
  type-checked (a tuple has a known arity).
- **Missing-safe access** — `.` already propagates `missing` through field access
  *and* method calls, so `user.name ?? "anon"` works with no `?.` operator needed.
- **Slicing & indexing** — Python-style `xs[1:3]`, `xs[:n]`, `xs[::2]`, `xs[::-1]`,
  with negative indices, on arrays, strings, DNA (a DNA slice stays `Dna`), and
  **tensors** (first axis: `t[0]` is a row, `t[1:3]` a sub-tensor, `t[i][j]` a
  scalar).
- **String interpolation** — any string interpolates `{expr}` (no `f` prefix;
  `{{`/`}}` for literal braces). Embedded expressions are full expressions and are
  type-checked: `print("mean {xs.mean()}, grade {if s >= 90 then "A" else "B"}")`.
- **`??` missing-default** (best-of-TypeScript) — `config.timeout ?? 30` yields the
  right-hand side only when the left is `missing`. Inside DataFrame predicates it
  lowers to Polars `fill_null`.
- **Static type checking** (ADR-0002): a bidirectional, localized inference pass
  runs *before* execution and catches type mistakes early — `5 + "x"`, calling a
  non-function, wrong arity, an unknown method, a non-boolean `if` — with the same
  caret-annotated, "did you mean …?" errors. **Permissive by design**: it errors
  only on *provable* mistakes and never rejects a program that would run, so
  DataFrame columns and dynamic data pass through untouched. Type annotations on
  function signatures are optional and checked: `fn area(w: Int, h: Int) -> Int =
  w * h`.
- Immutable bindings by default; `mut` for explicit mutability.
  Reassigning an immutable binding is a friendly compile-time-style error.
- `Int`, `Float`, `String`, `Bool`, `Array`, `Tensor`, `DataFrame`, `Dna`,
  `Function`, and `missing` values.
- **Tensors** — dense n-dimensional `f64` arrays (ndarray-backed): `tensor([[1,
  2], [3, 4]])`, `zeros([2,3])`, `ones(...)`, `eye(n)`; elementwise arithmetic
  with **NumPy-style broadcasting** (`a + 10`, `a + tensor([10,20])`); `shape`,
  `ndim`, `reshape`, `transpose`/`t`; whole-tensor and **axis-wise** reductions
  (`sum()`, `sum(0)`, `mean(1)`, `min`/`max`); `matmul`/`dot` (vector·vector,
  matrix·matrix, matrix·vector); and **linear algebra** — `det`, `inv`, `solve`,
  `norm` (pure-Rust, no BLAS dependency). The math stdlib broadcasts over tensors
  too (`sqrt(a)`).
- **`missing`** — one dedicated absent-value (ADR 0001), distinct from any real
  value and from float `NaN`. Propagates through math (`missing + 1` → `missing`),
  uses three-valued boolean logic (`true or missing` → `true`), and is tested
  with `.is_missing()` (never `==`, which propagates). Aggregations propagate
  (`[1, missing, 3].mean()` → `missing`); `.drop_missing()` opts out, visibly.
- Arithmetic (`+ - * / %`), comparison (`< > <= >= == !=`), and **word-based**
  boolean logic (`and`, `or`, `not`) — no symbol soup, no truthiness surprises.
- **Elementwise broadcasting** for arithmetic: `xs - xs.mean()`, `xs * 2`,
  `xs + ys`. `==` stays whole-value (no NumPy "ambiguous truth value" trap).
- **User-defined functions:** `fn name(params) = expr` (body is an expression;
  recursion works). A `=>` function is a first-class value you can store and call.
- Method calls with `.`, chainable across multiple lines.
- `if cond then a else b` as an **expression** (yields a value; `else` required).
- Comprehension methods: `map`, `filter`, `where`, `reduce`, `any`, `all`. The
  element is `it` by default (`xs.map(it + 1)`); name the binder with `=>` when
  nesting or when there's more than one (`grid.map(row => row.map(v => v + 1))`,
  `xs.reduce(0, (acc, x) => acc + x)`). `where` and `filter` are the same
  operation — the former is the data-query spelling DataFrames reuse in Phase 3.
- Negative indexing (`xs[-1]`).
- Built-ins: `print`, `range`, `dna`.
- Methods are **always called with `()`** — one rule, and the parens honestly
  signal "this computes" (which matters once big collections go lazy).
- Array methods: `mean`, `std`, `sum`, `min`, `max`, `count`, `normalize`,
  `sort`, `reverse`, `first`, `last`, `take`, `drop`, `zip`, `enumerate`,
  `map`, `filter`, `where`, `reduce`, `any`, `all`, `drop_missing`, `is_missing`.
- String methods: `upper`, `lower`, `count`, `reverse`.
- **DataFrames** backed by **Polars (latest), held as a lazy `LazyFrame`**:
  `read_csv(path)` / `read_parquet(path)`, then `where(predicate)`,
  `select(cols…)`, `sort(cols…)`, `group(keys…)` + a grouped
  `mean`/`sum`/`min`/`max`/`count`/`std`, plus `head(n)`, `count()`, `columns()`,
  and `write_parquet(df, path)` (streaming sink).
  Verbs only *extend the query plan*; it materializes once, at `print`/`count`,
  so a single chain is **delegated to Polars' lazy execution** (columnar,
  multi-threaded, with projection/predicate pushdown). Predicates like
  `age > 40 and resting_hr < 75` are **translated to Polars expressions** — the
  same `where` verb as arrays. Measured: a **50M-row filter+group+sort+head runs
  in ~0.2s from Parquet** (~2.3s from CSV), warm cache. See
  [docs/benchmarks.md](docs/benchmarks.md) — including caveats (warm-cache only;
  separate statements re-scan; not yet benchmarked vs pandas/DuckDB).
- **Math standard library** (broadcasts over arrays, propagates `missing`):
  `sqrt`, `cbrt`, `exp`, `ln`, `log10`, `log2`, `log(x, base)`, `sin`/`cos`/`tan`
  + inverses + hyperbolics, `floor`/`ceil`/`round`/`trunc`, `abs`, `sign`,
  `hypot`, `atan2`, `degrees`/`radians`, `min`/`max`; constants `pi`, `e`, `inf`.
- **`**` power operator** — right-associative, binds tighter than unary minus
  (`-2 ** 2 == -4`, `2 ** 3 ** 2 == 512`); stays `Int` when it can.
- DNA methods: `gc_content`, `complement`, `reverse_complement`, `kmers`,
  `length`.
- Errors that point a caret at the source and suggest a fix (including
  "did you mean ...?" via edit distance).

## Design principles (and how v0.1 honors them)

| Principle | In v0.1 |
|---|---|
| Simple syntax, dots over pipes | dot-chains, no `|>`, no `;` |
| One obvious way | methods always use `()`; one assignment operator |
| Immutable by default | `mut` is required to mutate |
| Excellent errors | caret + hint + typo suggestions |
| Memory efficiency | values share via `Rc` (zero-copy clones) |

## Roadmap

See [docs/ROADMAP.md](docs/ROADMAP.md). In short:

1. **Phase 1 — core interpreter** ✅ *(you are here)*
2. Phase 2 — type checker, modules, package manager
3. Phase 3 — DataFrame engine (Polars / Arrow)
4. Phase 4 — tensor engine
5. Phase 5 — JIT compilation
6. Phase 6 — GPU support

## Building

Requires a recent Rust toolchain.

```
cargo build --release
./target/release/helix examples/tour.helix
```

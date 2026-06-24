# Helix

> A scientific programming language designed for data-intensive research.

Helix is a modern scientific programming language implemented in Rust, designed for
data science, machine learning, artificial intelligence, computational biology, and
high-performance scientific computing.

It aims to combine **Python's readability**, **R's data workflow**, **Rust's
safety**, **Julia's scientific elegance**, **SQL's data operations**, and **Arrow's
zero-copy memory model**, while avoiding their respective pitfalls.

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

It is then used like any language CLI:

```sh
helix run examples/tour.helix     # run a script
helix eval "print(1 + 2)"         # a one-liner
helix repl                        # interactive session
helix help                        # all commands
```

> Prebuilt one-line installs activate once the project is published on GitHub with a
> release tag (the [release workflow](.github/workflows/release.yml) is prepared);
> until then the installer source-builds, which requires Rust. The distribution plan
> is described in [ADR 0009](docs/adr/0009-distribution-and-install.md).

## Status

The implementation extends beyond a prototype. It comprises a tree-walking
interpreter, a bytecode VM, and a Cranelift JIT (native code that outperforms
Node and Python on scalar recursion), lazy Polars/Arrow **DataFrames**, ndarray
**tensors** with linear algebra, a static type checker, a **module system**, **data
access** (`http_get` plus `parse_json`/`to_json` for REST APIs), **error handling**
(`try EXPR` yielding `{ok, value, error}`), **genomics** (`read_fasta`/`read_fastq`
sequences, and `read_vcf` variants into a queryable DataFrame), and a feature-gated
**CPython interop** layer (calling NumPy, polars, and similar
libraries; see [docs/python-interop.md](docs/python-interop.md)). The test suite
contains 130 or more tests and compiles with zero warnings. The remaining roadmap
(GPU support, package manager, bundled Python) is described below.

## Current capabilities

```text
# Immutable by default; mutability is explicit
x = 42
mut counter = 0
counter = counter + 1

# Arrays and statistics
xs = [1, 2, 3, 4]
xs.mean()
xs.std()
xs.normalize()

# Multi-line dot-chains, no pipes or line-continuations
[10, 5, 8, 3, 9]
    .sort()
    .reverse()

# Comprehensions: `it` is the current element; `where` is equivalent to `filter`.
scores
    .where(it >= 60)
    .map(it + 5)
    .mean()

# `if` is an expression that yields a value
grade = if score > 90 then "A" else "B"

# DataFrames (Polars/Arrow-backed) use the same `where`/`sort` verbs as arrays.
# `where(age > 40)` lowers to a native Polars filter rather than an interpreter loop.
patients = io.read_csv("patients.csv")
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
- **Records** — structured data with named fields, comparable to a Python dict or a
  TypeScript object: `{name: "Ada", age: 41}`, accessed with `.name` (no parens;
  `.method()` requires parens, so the two never collide). Nested records,
  arrays-of-records, and function-returning-record are all supported and
  **type-checked**: a field typo is a compile-time error with a suggestion. Trailing
  commas are allowed.
- **Local bindings** — `let a = x, b = y in expr` introduces intermediate values
  (sequential; scoped to the body). This is the standard way to write a function in
  multiple steps: `fn variance(xs) = let m = xs.mean(), n = xs.count() in xs.map((it
  - m) ** 2).sum() / n`. Helix uses `let … in` rather than indented blocks because
  indentation would collide with multi-line dot-chains (see ADR-0004).
- **Tuples and destructuring** — `(a, b)` groups fixed-size values; `a, b = pair`
  unpacks them. Functions return multiple values directly: `q, r = divmod(17, 5)`.
  `zip`/`enumerate` yield tuples (`("Ada", 41)`) rather than two-element arrays, and
  **lambda parameters destructure them**:
  `names.zip(ages).map((name, age) => "{name} ({age})")`. Destructuring length is
  type-checked, since a tuple has a known arity.
- **Missing-safe access** — `.` propagates `missing` through both field access and
  method calls, so `user.name ?? "anon"` works without a `?.` operator.
- **Slicing and indexing** — Python-style `xs[1:3]`, `xs[:n]`, `xs[::2]`, `xs[::-1]`,
  with negative indices, on arrays, strings, DNA (a DNA slice retains type `Dna`),
  and **tensors** (first axis: `t[0]` is a row, `t[1:3]` a sub-tensor, `t[i][j]` a
  scalar).
- **String interpolation** — any string interpolates `{expr}` (no `f` prefix;
  `{{`/`}}` for literal braces). Embedded expressions are full expressions and are
  type-checked: `print("mean {xs.mean()}, grade {if s >= 90 then "A" else "B"}")`.
- **`??` missing-default** — `config.timeout ?? 30` yields the right-hand side only
  when the left operand is `missing`. Inside DataFrame predicates it lowers to Polars
  `fill_null`.
- **Static type checking** (ADR-0002): a bidirectional, localized inference pass
  runs *before* execution and catches type mistakes early — `5 + "x"`, calling a
  non-function, incorrect arity, an unknown method, a non-boolean `if` — with the
  same caret-annotated, "did you mean …?" errors. **Permissive by design**: it
  errors only on *provable* mistakes and never rejects a program that would run, so
  DataFrame columns and dynamic data pass through untouched. Type annotations on
  function signatures are optional and checked: `fn area(w: Int, h: Int) -> Int =
  w * h`.
- Immutable bindings by default; `mut` for explicit mutability.
  Reassigning an immutable binding is a compile-time-style error.
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
- **`missing`** — a single dedicated absent-value (ADR 0001), distinct from any real
  value and from float `NaN`. It propagates through arithmetic (`missing + 1` →
  `missing`), uses three-valued boolean logic (`true or missing` → `true`), and is
  tested with `.is_missing()` (never `==`, which propagates). Aggregations propagate
  (`[1, missing, 3].mean()` → `missing`); `.drop_missing()` opts out explicitly.
- Arithmetic (`+ - * / %`), comparison (`< > <= >= == !=`), and **word-based**
  boolean logic (`and`, `or`, `not`), with no truthiness coercion.
- **Elementwise broadcasting** for arithmetic: `xs - xs.mean()`, `xs * 2`,
  `xs + ys`. `==` remains whole-value, avoiding the NumPy "ambiguous truth value"
  behavior.
- **User-defined functions:** `fn name(params) = expr` (the body is an expression;
  recursion is supported). A `=>` function is a first-class value that can be stored
  and called.
- Method calls with `.`, chainable across multiple lines.
- `if cond then a else b` as an **expression** (yields a value; `else` is required).
- Comprehension methods: `map`, `filter`, `where`, `reduce`, `any`, `all`. The
  element is `it` by default (`xs.map(it + 1)`); the binder is named with `=>` when
  nesting or when there is more than one (`grid.map(row => row.map(v => v + 1))`,
  `xs.reduce(0, (acc, x) => acc + x)`). `where` and `filter` are the same
  operation; the former is the data-query spelling that DataFrames reuse in Phase 3.
- Negative indexing (`xs[-1]`).
- Built-ins: `print`, `range`, `dna`.
- Methods are **always called with `()`** — a single rule, and the parens signal
  that the call performs computation, which is significant once large collections
  are evaluated lazily.
- Array methods: `mean`, `std`, `sum`, `min`, `max`, `count`, `normalize`,
  `sort`, `reverse`, `first`, `last`, `take`, `drop`, `zip`, `enumerate`,
  `map`, `filter`, `where`, `reduce`, `any`, `all`, `drop_missing`, `is_missing`.
- String methods: `upper`, `lower`, `count`, `reverse`.
- **DataFrames** backed by **Polars (latest), held as a lazy `LazyFrame`**:
  `io.read_csv(path)` / `io.read_parquet(path)`, then `where(predicate)`,
  `select(cols…)`, `sort(cols…)`, `group(keys…)` + a grouped
  `mean`/`sum`/`min`/`max`/`count`/`std`, plus `head(n)`, `count()`, `columns()`,
  and `io.write_parquet(df, path)` (streaming sink).
  Verbs only *extend the query plan*; it materializes once, at `print`/`count`,
  so a single chain is **delegated to Polars' lazy execution** (columnar,
  multi-threaded, with projection and predicate pushdown). Predicates such as
  `age > 40 and resting_hr < 75` are **translated to Polars expressions** using the
  same `where` verb as arrays. Measured: a **50M-row filter+group+sort+head runs
  in ~0.2s from Parquet** (~2.3s from CSV), warm cache. See
  [docs/benchmarks.md](docs/benchmarks.md), including caveats (warm-cache only;
  separate statements re-scan; not yet benchmarked against pandas/DuckDB).
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

## Design principles (and their realization in v0.1)

| Principle | In v0.1 |
|---|---|
| Simple syntax, dots over pipes | dot-chains, no `|>`, no `;` |
| One obvious way | methods always use `()`; one assignment operator |
| Immutable by default | `mut` is required to mutate |
| Informative errors | caret, hint, and typo suggestions |
| Memory efficiency | values share via `Rc` (zero-copy clones) |

## Roadmap

See [docs/ROADMAP.md](docs/ROADMAP.md). In summary:

1. **Phase 1 — core interpreter** (Done; current phase)
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

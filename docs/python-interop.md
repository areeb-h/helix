# Python interop (calling Python from Helix)

> **Status: v1.** Helix can import Python modules, read attributes, call functions,
> pass primitives + arrays, and turn Python exceptions into Helix errors. Zero-copy
> DataFrame/Tensor sharing and a bundled interpreter are planned (see the roadmap at
> the bottom). Design rationale: [ADR 0008](adr/0008-cpython-interop.md).

Helix does not yet replace NumPy, pysam, scikit-learn, or PyTorch; it calls them
instead. A pipeline is written in Helix, reaching into Python only where a library
is needed.

## Enabling it

Python interop is **off by default** so the standard `helix` binary stays
self-contained (no Python dependency). Build with the `python` feature:

```sh
cargo build --features python          # or: cargo run --features python <script>
```

A Python interpreter must be available on the system (the build links against it),
and any imported Python packages (`numpy`, `pysam`, …) must be installed in that
environment (`pip install numpy`). The standard library (`math`, `statistics`,
`json`, …) is always available.

Using `python` in a Helix built **without** the feature produces a clear error:

```
error: Helix was built without Python support
help: rebuild with `cargo build --features python`.
```

## Importing a module

Two equivalent forms:

```helix
import python.math as m         # statement form
print(m.sqrt(16.0))             # 4.0

stats = python.import("statistics")   # expression form (dynamic names)
print(stats.mean([1.0, 2.0, 3.0]))    # 2.0
```

`import python.<module>` maps to the Python module `<module>`; submodules use dots
(`import python.os.path as p`). The alias defaults to the last segment when `as` is
omitted.

## Calling functions and reading attributes

Attribute access and method calls forward straight to Python:

```helix
import python.math as m
print(m.pi)                 # 3.141592653589793   (attribute)
print(m.gcd(12, 18))        # 6                   (method call)
print(m.floor(3.7))         # 3
```

## What converts, and what stays opaque

The governing rule is as follows. **Immutable scalars convert to native Helix
values; everything else remains an opaque Python handle.**

| Python value | In Helix |
|---|---|
| `int` | `Int` |
| `float` | `Float` |
| `bool` | `Bool` |
| `str` | `String` |
| `None` | `missing` |
| `list`, `dict`, objects, NumPy arrays, … | **opaque `PyObject`** |

Rationale: auto-copying a Python `list` into a native Helix array would silently
lose the list's identity and mutability (a problem documented in other languages;
see the ADR). Containers therefore remain opaque until a copy is explicitly
requested:

```helix
builtins = python.import("builtins")
nums = builtins.list(builtins.range(0, 5))
print(nums)                 # [0, 1, 2, 3, 4]      — handle, shown as its Python value
print(to_array(nums).sum()) # 10                   — to_array makes it a native Array
```

A handle is still opaque (it is a Python object, not a Helix `Array`), but it
**prints as its `str(obj)`** so you can see what it holds. `to_array(x)` converts any
Python iterable (or an already-native array) into a Helix `Array`. The reverse
direction is automatic: Helix `Int`/`Float`/`Bool`/`String`/`missing`/`Array`/`Record`
(→ `dict`)/`Tensor` (→ NumPy)/`DataFrame` (→ Polars) convert to Python as arguments.

## Handles feel native: indexing, operators, expressions

The bridge forwards Python's protocols, so an opaque handle is still usable directly:

```helix
np = python.import("numpy")
a = np.arange(6)
print(a[2])          # 2                       — forwards to __getitem__
print(a[1:4])        # [1 2 3]                 — slicing forwards too (numpy semantics)
print(a[::-1])       # [5 4 3 2 1 0]           — steps / negatives included
print(a * 10)        # [ 0 10 20 30 40 50]     — forwards to __mul__
print(a[3] < a[4])   # true                    — forwards to __lt__
```

Indexing (`h[k]`), **slicing** (`h[a:b:step]`, open bounds and negative steps included),
and binary operators (`+ - * / // % ** == != < > <= >=`) on a handle dispatch through
Python. A record indexes a Python dict the same way (`d["key"]`). For anything fancier
(multi-axis `a[:, 0]`, keyword args), drop to `python.eval("…")`.

### `python.exec` / `python.eval` — the full-Python escape hatch

Some Python syntax has no Helix equivalent — most importantly **keyword arguments**
(`np.array(x, dtype="float64")`). `python.exec(code)` runs statements and
`python.eval(expr)` evaluates an expression, both in one **persistent namespace**, so
anything is expressible:

```helix
python.exec("import numpy as np")
grid = python.eval("np.linspace(0, 1, 5, dtype='float64')")   # kwargs, just works
print(python.eval("[x*x for x in range(5)]"))                 # comprehensions, slices…
```

`eval` converts its result by the same rules (scalars native, the rest opaque).

## DataFrames (zero-copy)

Helix DataFrames are Polars/Arrow-backed, so they cross to and from Python's
`polars` by **sharing the underlying Arrow buffers** — no row-by-row copying. This
needs the Python `polars` package (`pip install polars`).

- **Helix → Python:** pass a Helix DataFrame to any Python function and it arrives
  as a `polars.DataFrame`.
- **Python → Helix:** `to_dataframe(x)` returns a Python `polars.DataFrame` as a
  **first-class Helix DataFrame** — the standard lazy verbs (`where`/`select`/`sort`/
  `group`/`count`) then operate on it.

```helix
df = read_csv("examples/data/patients.csv")     # a Helix DataFrame

# hand it to Python — it's a polars.DataFrame there:
print(python.import("builtins").len(df))         # 8  (rows, counted in Python)

# round-trip through Python's polars and back:
pl = python.import("polars")
back = to_dataframe(pl.concat([df]))             # back is a Helix DataFrame again
print(back.where(@age > 40).select(@name))       # native verbs work on it
```

The missing-data models align directly: Helix's `missing` is Arrow's validity
bitmap, which is the same representation polars uses, so nulls round-trip with no
translation.

## Tensors (NumPy)

Helix Tensors (dense `f64`, ndarray-backed) cross to and from Python's NumPy. This
needs the Python `numpy` package (`pip install numpy`).

- **Helix → Python:** pass a Tensor to a Python function and it arrives as a NumPy
  `float64` array.
- **Python → Helix:** `to_tensor(x)` returns a NumPy `f64` array as a native Helix
  Tensor (the verbs `shape`/`reshape`/`matmul`/`sum`/… then operate on it).

```helix
t = tensor([[1.0, 2.0], [3.0, 4.0]])
np = python.import("numpy")
print(np.sum(t))                          # 10.0  (NumPy computes, returns a Float)

la = python.import("numpy.linalg")
inv = to_tensor(la.inv(t))                # a Helix Tensor again
print(t.matmul(inv))                      # native verb: ~ the identity matrix
```

Unlike DataFrames, **Tensor interop copies** at the boundary (it does not share
buffers): Helix tensors are immutable and shared, whereas NumPy arrays are mutable,
so each side receives an independent buffer, preserving Helix's immutability
guarantee. The copy is acceptable under the "cross the boundary once" rule. Only
`f64` arrays are supported (Helix tensors are `f64`).

## Errors

A Python exception aborts the program with a Helix-native diagnostic pointing at the
call site:

```helix
m = python.import("no_such_module_xyz")
```
```
error: python error: ModuleNotFoundError: No module named 'no_such_module_xyz'
  --> script.helix:1:5
  |
1 | m = python.import("no_such_module_xyz")
  |     ^
```

(Helix does not yet provide `try`/`catch`, so a Python error stops the program
rather than being recoverable. This will change when Helix gains error handling.)

## Performance: cross the boundary once

Each Helix→Python call has overhead and holds Python's GIL. A small Python function
should not be called in a tight Helix loop:

```helix
# Slow — crosses the language boundary ten million times:
for-style: ten_million_values.map(x => py_fn(x))

# Fast — cross once with the whole array:
py_fn(ten_million_values)
```

Many scientific libraries perform their heavy work in native C/Rust and release the
GIL during it, so a single call over a large array is inexpensive; the cost is
per-call.

## A complete example

See [`examples/python/interop.helix`](../examples/python/interop.helix):

```helix
import python.math as m
print("sqrt(16) =", m.sqrt(16.0))
print("pi =", m.pi)

stats = python.import("statistics")
print("mean =", stats.mean([1.0, 2.0, 3.0, 4.0]))

# handles feel native: indexing, operators, value-printing
np = python.import("numpy")
a = np.arange(6)
print("a[2] =", a[2], " a*10 =", a * 10)

# exec/eval share a namespace — the kwargs / full-Python escape hatch
python.exec("import numpy as N")
print("eval kwargs:", python.eval("N.linspace(0, 1, 5, dtype='float64')"))

t = to_tensor(python.eval("N.eye(3) * 5.0"))   # back to a native Helix tensor
print(t)
```

Run it with `cargo run --features python examples/python/interop.helix`.

## What's not here yet (roadmap)

- **Truly zero-copy (buffer-sharing) Tensors.** Tensor↔NumPy works now (above) but
  *copies*; a future DLPack path could share GPU/large buffers where the mutability
  semantics allow it. (`to_array` also copies plain Python lists, since lists are not
  Arrow-backed.)
- **A bundled Python.** The current feature build links the system Python; the plan
  is to bundle a relocatable CPython so a Python-enabled Helix ships self-contained.
- **Recoverable errors** (needs `try`/`catch`), and **Python → Helix** (calling
  Helix from Python for performance-critical sections).

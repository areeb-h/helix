# Helix standard library

Reusable Helix modules that ship with the toolchain. They are written in Helix itself
and compose the built-in core, so they inherit its semantics (notably missing-data
propagation) and never diverge from the engine.

## Using it

Import a module by its `std.` path from anywhere on the search path:

```helix
import std.stats as st
import std.seq.{mean_gc}      # or selectively
```

Imports resolve against, in order: the importing file's own directory, each entry of
the `HELIX_PATH` environment variable, then the install-relative `std/` directory
beside the binary. In a checkout, point `HELIX_PATH` at the repository root:

```sh
HELIX_PATH=. helix run myscript.helix
```

## Modules

- **`std.stats`** — descriptive helpers over the built-in aggregations:
  `standard_error`, `coefficient_of_variation`, `iqr`, `spread`, `zscores`.
- **`std.seq`** — sequence-analysis helpers over the DNA operations: `at_content`,
  `mean_gc`, `total_length`.

These are a seed, not a finished library; modules are added as common patterns emerge.

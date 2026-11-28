# General-code benchmarks

Programs made of what most Helix code is made of — records, strings, closures, `match`,
dicts, method calls, an app's `update` and `view` — none of which the JIT compiles: it takes
numeric kernels, and `helix jit-explain` reports nothing compiled for any program here
(`app.helix` aside, where one comprehension inside `std.ui` is). They run on the bytecode
VM, so they measure the VM and the runtime under it. The numeric side has its own set in
`bench/kernels/`.

Each program is small enough to run under callgrind in a few seconds, prints a few values
so a wrong answer shows, and gives the same output on all three engines.

| program | what it measures |
|---|---|
| `records.helix` | building a record per element, reading fields, `{...r, k: v}` on an element and on a state record carried through a `reduce` |
| `nested.helix` | records of records, tuples and arrays; field chains three deep; an update that rebuilds two levels |
| `strings.helix` | interpolation with and without format specs, `split`, `join`, `upper`, `replace`, `contains`, `ends_with` |
| `closures.helix` | lambdas that capture locals, functions returned from and passed to functions, calls through a function value |
| `match.helix` | `match` on records, tuples, literals, or-patterns and ranges, with guards — a reducer over an event stream |
| `dicts.helix` | counting and grouping with `insert` in a fold, `get`, `has`, `items` |
| `methods.helix` | chains of array and string methods, each a dispatch on the receiver's type |
| `calls.helix` | calls to small functions taking records, strings and arrays; tail recursion over a list; a recursive tree walk |
| `app.helix` | a todo app's `update` and `view` replayed over 210 events with `ui.replay` |

`run.sh` also measures five programs from `examples/`, because they are real code rather
than code written to be measured:

| name | program |
|---|---|
| `gallery`, `orbit` | `examples/apps/{gallery,orbit}.helix --replay` — whole apps, one rendered screen per event |
| `freezer_model`, `updates_model`, `lockout_model` | `examples/rules/*_model_test.helix` — rule code over records and dicts, run on every reachable state by `std.model` |

## Running

```
bench/general/run.sh /path/to/helix            # every workload: "<name> <instructions>"
bench/general/run.sh /path/to/helix records    # only the named ones
```

The number is callgrind's `Ir`, instructions executed. It is the same from run to run to
within a few thousand in a billion, so two builds can be compared on a busy machine, which
wall time cannot. Build both with the same profile (`cargo build --profile gate` keeps
symbols, so `callgrind_annotate` can name functions).

What each change to the runtime did to these numbers, and what was tried and not kept, is in
[`RESULTS.md`](RESULTS.md).

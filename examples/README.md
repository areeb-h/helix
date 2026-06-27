# Helix examples

Runnable programs grouped by topic. Run any of them with:

```sh
helix examples/<category>/<name>.helix
```

Every example in `language/`, `numerics/`, `dataframes/`, `statistics/`, and `bio/`
is self-contained and produces identical output on both engines (the `vmparity`
check covers them). New to Helix? Start with [`language/tour.helix`](language/tour.helix).

## language/ — the core language
| File | Shows |
|------|-------|
| [tour.helix](language/tour.helix) | A whirlwind tour of the whole language |
| [bindings.helix](language/bindings.helix) | `let`, `mut`, and immutability |
| [functions.helix](language/functions.helix) | Named functions, annotations, recursion |
| [closures.helix](language/closures.helix) | Lambdas and the implicit `it` |
| [records.helix](language/records.helix) | Records and field access |
| [tuples.helix](language/tuples.helix) | Tuples and destructuring |
| [strings.helix](language/strings.helix) | String methods |
| [interpolation.helix](language/interpolation.helix) | `"{x}"` interpolation + `:spec` formatting (`.2f`, `%`, hex, alignment) |
| [slicing.helix](language/slicing.helix) | Python-style slices `xs[1:3]`, `xs[::-1]` |
| [match.helix](language/match.helix) | Pattern matching with guards |
| [control-flow.helix](language/control-flow.helix) | `if`/`match`/`do`-blocks as expressions |
| [operators.helix](language/operators.helix) | Arithmetic, boolean, bitwise (`& \| ^ << >>`), `??` |
| [collections.helix](language/collections.helix) | Array verbs: map/filter/reduce, zip/zipmap, min_by, ranges |
| [error-handling.helix](language/error-handling.helix) | `try` and recovering from runtime errors |
| [errors.helix](language/errors.helix) | What good error messages look like |
| [named-arguments.helix](language/named-arguments.helix) | Calling with named arguments |
| [typed.helix](language/typed.helix) | The permissive static type checker |

## numerics/ — math and arrays
| File | Shows |
|------|-------|
| [math.helix](numerics/math.helix) | The math standard library (broadcast + `missing`) |
| [vectors.helix](numerics/vectors.helix) | Vector ops: dot/norm/cumsum/softmax/argsort/clamp/zscores |
| [tensors.helix](numerics/tensors.helix) | N-d tensors: matmul, transpose, solve |
| [random.helix](numerics/random.helix) | Reproducible seeded RNG (`random`/`randn`/shuffle/sample) |

## dataframes/ — tabular data
| File | Shows |
|------|-------|
| [dataframes.helix](dataframes/dataframes.helix) | Verbs: where/select/sort/group/join |
| [analysis.helix](dataframes/analysis.helix) | A fuller analysis pipeline |
| [io.helix](dataframes/io.helix) | Read/write CSV·JSON·Parquet, `vstack`, `unique`, `file_exists`, `sha256` |

## statistics/ — modeling and evaluation
| File | Shows |
|------|-------|
| [statistics.helix](statistics/statistics.helix) | Descriptive stats, t-test, correlation, regression |
| [regression.helix](statistics/regression.helix) | Polynomial model selection with AIC/BIC |
| [metrics.helix](statistics/metrics.helix) | Regression + classification metrics (RMSE, F1, confusion matrix) |
| [experiment.helix](statistics/experiment.helix) | An end-to-end experiment loop |

## bio/ — genomics (the flagship domain)
| File | Shows |
|------|-------|
| [genomics.helix](bio/genomics.helix) | DNA sequences, GC content, k-mers |
| [sequencing.helix](bio/sequencing.helix) | FASTQ reads and Phred quality |
| [alignment.helix](bio/alignment.helix) | Pairwise sequence alignment |
| [alignments.helix](bio/alignments.helix) | SAM/BAM alignment records + region queries |
| [annotations.helix](bio/annotations.helix) | GFF/BED genome annotations |
| [variants.helix](bio/variants.helix) | VCF/BCF variants + region queries |

## modules/ — multi-file programs
[`shapes.helix`](modules/shapes.helix) imports [`geometry.helix`](modules/geometry.helix).
Run `helix examples/modules/shapes.helix`.

## interop/ — outside the runtime (need optional features)
- [`python/`](python/) — CPython interop (`--features python`): DataFrames, tensors, calling Python.
- [`api/fetch.helix`](api/fetch.helix) — HTTP requests (needs network).

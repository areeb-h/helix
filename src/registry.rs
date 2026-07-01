//! The single source of truth for Helix's built-in functions and per-type methods.
//!
//! Before this registry, the set of builtin names lived in `BUILTIN_FNS` (duplicated
//! verbatim in the interpreter and the type checker) and each receiver type's method
//! names lived in 5–7 hand-maintained lists across the checker, the runtime, and the
//! error-hint sites — which had already drifted (e.g. `is_missing` and the tensor
//! method set differed between the checker and the runtime). Every consumer now derives
//! its name sets from the tables here, so the "checker says yes, runtime says no such
//! method" bug class is structural rather than a standing risk. The [`tests`] module
//! enforces that no name is declared twice.
//!
//! Builtins are keyed by a flat `path` — language primitives and domain functions
//! alike are bare names (`"sqrt"`, `"read_vcf"`, `"t_test"`); the old `bio.`/`stats.`/
//! `io.` namespace prefixes were dropped (ADR 0017), so anything that acts on data is
//! now a method instead. Methods are listed per-receiver below; `is_missing` and
//! `to_json` are universal across every receiver, so they live once in
//! [`UNIVERSAL_METHODS`] rather than repeated in each table.

/// A built-in function: its dotted path and whether it is pure (free of side effects
/// and reproducible). Impure builtins (I/O, output, network) must never be memoized.
pub struct BuiltinDef {
    pub path: &'static str,
    pub pure: bool,
}

/// Every built-in function, keyed by a flat name (no namespace prefixes — ADR 0017).
pub static BUILTINS: &[BuiltinDef] = &[
    // --- effectful / non-reproducible (I/O, output, network) -> not memoizable ---
    BuiltinDef { path: "print", pure: false },
    BuiltinDef { path: "emit", pure: false },
    BuiltinDef { path: "sleep", pure: false },
    BuiltinDef { path: "listen", pure: false },
    BuiltinDef { path: "read_csv", pure: false },
    BuiltinDef { path: "read_parquet", pure: false },
    BuiltinDef { path: "read_text", pure: false },
    BuiltinDef { path: "read_json", pure: false },
    BuiltinDef { path: "read_dir", pure: false },
    BuiltinDef { path: "file_exists", pure: false },
    BuiltinDef { path: "remove_file", pure: false },
    BuiltinDef { path: "mkdir", pure: false },
    BuiltinDef { path: "read_fasta", pure: false },
    BuiltinDef { path: "read_fastq", pure: false },
    BuiltinDef { path: "read_vcf", pure: false },
    BuiltinDef { path: "read_bcf", pure: false },
    BuiltinDef { path: "read_sam", pure: false },
    BuiltinDef { path: "read_bam", pure: false },
    BuiltinDef { path: "read_gff", pure: false },
    BuiltinDef { path: "read_bed", pure: false },
    BuiltinDef { path: "http_get", pure: false },
    BuiltinDef { path: "http_post", pure: false },
    BuiltinDef { path: "http_request", pure: false },
    // --- constructors / conversions ---
    BuiltinDef { path: "dna", pure: true },
    BuiltinDef { path: "range", pure: true },
    BuiltinDef { path: "tensor", pure: true },
    BuiltinDef { path: "zeros", pure: true },
    BuiltinDef { path: "ones", pure: true },
    BuiltinDef { path: "eye", pure: true },
    BuiltinDef { path: "to_array", pure: true },
    BuiltinDef { path: "to_dataframe", pure: true },
    BuiltinDef { path: "to_tensor", pure: true },
    BuiltinDef { path: "dataframe", pure: true },
    // --- math standard library (broadcast + propagate missing) ---
    BuiltinDef { path: "sqrt", pure: true },
    BuiltinDef { path: "cbrt", pure: true },
    BuiltinDef { path: "abs", pure: true },
    BuiltinDef { path: "exp", pure: true },
    BuiltinDef { path: "ln", pure: true },
    BuiltinDef { path: "log10", pure: true },
    BuiltinDef { path: "log2", pure: true },
    BuiltinDef { path: "log", pure: true },
    BuiltinDef { path: "sin", pure: true },
    BuiltinDef { path: "cos", pure: true },
    BuiltinDef { path: "tan", pure: true },
    BuiltinDef { path: "asin", pure: true },
    BuiltinDef { path: "acos", pure: true },
    BuiltinDef { path: "atan", pure: true },
    BuiltinDef { path: "atan2", pure: true },
    BuiltinDef { path: "sinh", pure: true },
    BuiltinDef { path: "cosh", pure: true },
    BuiltinDef { path: "tanh", pure: true },
    // neural-net activations (elementwise; broadcast over arrays/tensors)
    BuiltinDef { path: "relu", pure: true },
    BuiltinDef { path: "sigmoid", pure: true },
    // index of the largest / smallest element (classification readout)
    BuiltinDef { path: "argmax", pure: true },
    BuiltinDef { path: "argmin", pure: true },
    BuiltinDef { path: "floor", pure: true },
    BuiltinDef { path: "ceil", pure: true },
    BuiltinDef { path: "round", pure: true },
    BuiltinDef { path: "trunc", pure: true },
    BuiltinDef { path: "sign", pure: true },
    // IEEE float predicates — guard a `NaN`/`inf` before a comparison would raise.
    BuiltinDef { path: "is_nan", pure: true },
    BuiltinDef { path: "is_finite", pure: true },
    BuiltinDef { path: "is_infinite", pure: true },
    // Monotonic seconds (f64) since the process started — for in-language timing.
    // Impure: the value is non-deterministic, so never use it in a computed result.
    BuiltinDef { path: "clock_monotonic", pure: false },
    BuiltinDef { path: "degrees", pure: true },
    BuiltinDef { path: "radians", pure: true },
    BuiltinDef { path: "hypot", pure: true },
    BuiltinDef { path: "min", pure: true },
    BuiltinDef { path: "max", pure: true },
    BuiltinDef { path: "erf", pure: true },
    BuiltinDef { path: "gcd", pure: true },
    // codepoint <-> single-character string (URL/percent decoding, ANSI escapes, byte work)
    BuiltinDef { path: "chr", pure: true },
    BuiltinDef { path: "ord", pure: true },
    // exact rationals
    BuiltinDef { path: "rational", pure: true },
    BuiltinDef { path: "numerator", pure: true },
    BuiltinDef { path: "denominator", pure: true },
    BuiltinDef { path: "to_float", pure: true },
    BuiltinDef { path: "to_int", pure: true },
    BuiltinDef { path: "dict", pure: true },
    BuiltinDef { path: "lll", pure: true },
    BuiltinDef { path: "lll_exact", pure: true },
    // reverse-mode autodiff: wrap a leaf, read its forward value, get a gradient
    BuiltinDef { path: "variable", pure: true },
    BuiltinDef { path: "value_of", pure: true },
    BuiltinDef { path: "gradient", pure: true },
    // general pairwise sequence alignment (NW/SW) over arbitrary arrays
    BuiltinDef { path: "align", pure: true },
    // content hash — pure (same string → same digest), so safely memoizable
    BuiltinDef { path: "sha256", pure: true },
    BuiltinDef { path: "hmac_sha256", pure: true },
    BuiltinDef { path: "base64_encode", pure: true },
    BuiltinDef { path: "base64_decode", pure: true },
    BuiltinDef { path: "hex_encode", pure: true },
    BuiltinDef { path: "hex_decode", pure: true },
    // AES-256-GCM. keygen/encrypt draw fresh randomness (NOT memoizable); decrypt is pure.
    BuiltinDef { path: "aes_keygen", pure: false },
    BuiltinDef { path: "aes_encrypt", pure: false },
    BuiltinDef { path: "aes_decrypt", pure: true },
    // Ed25519 signatures. keygen draws randomness (impure); sign/verify are deterministic.
    BuiltinDef { path: "ed25519_keygen", pure: false },
    BuiltinDef { path: "ed25519_sign", pure: true },
    BuiltinDef { path: "ed25519_verify", pure: true },
    // --- random (reproducible: seeded + pure, so safely memoizable) ---
    BuiltinDef { path: "random", pure: true },
    BuiltinDef { path: "randn", pure: true },
    BuiltinDef { path: "random_int", pure: true },
    // --- experimentation: domain grids + model-eval metrics ---
    BuiltinDef { path: "linspace", pure: true },
    BuiltinDef { path: "mse", pure: true },
    BuiltinDef { path: "rmse", pure: true },
    BuiltinDef { path: "mae", pure: true },
    BuiltinDef { path: "r2_score", pure: true },
    BuiltinDef { path: "aic", pure: true },
    BuiltinDef { path: "bic", pure: true },
    // --- classification metrics (mirror the regression metrics; (y_true, y_pred)) ---
    BuiltinDef { path: "accuracy", pure: true },
    BuiltinDef { path: "precision", pure: true },
    BuiltinDef { path: "recall", pure: true },
    BuiltinDef { path: "f1_score", pure: true },
    BuiltinDef { path: "confusion_matrix", pure: true },
    // --- statistics: symmetric tests/fits + distributions (no privileged receiver) ---
    BuiltinDef { path: "normal_cdf", pure: true },
    BuiltinDef { path: "normal_pdf", pure: true },
    BuiltinDef { path: "correlation", pure: true },
    BuiltinDef { path: "t_test", pure: true },
    BuiltinDef { path: "linear_regression", pure: true },
    BuiltinDef { path: "multiple_regression", pure: true },
    // Lightweight OLS (no inference) — the fast fit for model-selection loops.
    BuiltinDef { path: "least_squares", pure: true },
    // (descriptive stats `iqr`/`zscores`/… are now array methods; `mean_gc`/
    // `at_content`/`total_length` are sequence-array/Dna methods.)
    // --- testing / assertions (guards that raise a catchable error on failure) ---
    // Impure so they always run (never memoized away) — the check is the point.
    BuiltinDef { path: "assert", pure: false },
    BuiltinDef { path: "assert_eq", pure: false },
    BuiltinDef { path: "assert_close", pure: false },
    // (JSON, charts, and format export are now methods: `value.to_json()`,
    // `str.parse_json()`, `xs.bar_chart()`, `data.to_html()`, `df.write_csv(p)`, …)
];

/// Methods universal to every receiver type (handled before the per-type dispatch).
/// `to_json` serializes any value (so it lives here, never in a per-type table).
pub static UNIVERSAL_METHODS: &[&str] = &["is_missing", "to_json"];

/// Array methods (comprehension verbs, aggregations, statistics, transforms,
/// descriptive stats, charts, and tabular export/write).
pub static ARRAY_METHODS: &[&str] = &[
    "mean", "std", "median", "var", "quantile", "summary", "sum", "min", "max", "count", "length",
    "index_of", "normalize", "sort", "reverse", "first", "last", "map", "filter", "where", "reduce",
    "scan", "any",
    "all", "take", "drop", "take_while", "drop_while", "position", "zip", "enumerate", "top",
    "frequencies", "unique", "drop_missing",
    "join", "concat", "flatten", "min_by", "max_by", "argmin", "argmax", "zipmap", "sort_by", "zscores", "iqr",
    "spread", "standard_error", "coefficient_of_variation", "dot", "norm", "cumsum", "product",
    "argsort", "clamp", "softmax", "bootstrap", "contains", "mean_gc",
    "total_length", "bar_chart", "histogram", "line_chart", "sparkline", "scatter", "svg_bar",
    "svg_line", "write_csv", "write_tsv", "write_json", "to_html", "to_markdown", "to_table",
    "to_dict", "write_fasta", "write_fastq", "shuffle", "sample", "choice",
];

/// String methods.
pub static STRING_METHODS: &[&str] = &[
    "upper", "lower", "count", "length", "reverse", "trim", "split", "replace", "contains",
    "starts_with", "ends_with", "take", "drop", "repeat", "ljust", "rjust", "center", "phred",
    "parse_json", "to_float", "to_int", "write_to", "append_to",
];

/// DNA-sequence methods.
pub static DNA_METHODS: &[&str] = &[
    "gc_content", "reverse_complement", "complement", "kmers", "windows", "codons", "kmer_counts",
    "canonical_kmer_counts", "align", "find", "find_all", "gc_skew", "longest_homopolymer",
    "length", "count", "at_content", "base_counts", "hamming",
];

/// Tensor methods (shape, aggregations, linear algebra).
pub static TENSOR_METHODS: &[&str] = &[
    "shape", "ndim", "count", "sum", "mean", "min", "max", "flatten", "reshape", "transpose", "t",
    "matmul", "dot", "norm", "det", "inv", "solve", "softmax",
];

/// DataFrame methods (column verbs + value methods + serialize/write).
pub static DF_METHODS: &[&str] = &[
    "where", "filter", "select", "sort", "group", "with", "join", "vstack", "unique", "column",
    "head", "count", "columns", "cache", "write_csv", "write_tsv", "write_json", "write_parquet",
    "to_html", "to_markdown", "to_table",
];

/// Grouped-DataFrame aggregations.
pub static GROUPBY_METHODS: &[&str] = &["mean", "sum", "min", "max", "count", "std"];

/// Keyed-map (`Dict`) methods — O(log n) lookup, sorted (deterministic) enumeration.
pub static DICT_METHODS: &[&str] =
    &["get", "contains", "keys", "values", "items", "insert", "remove", "count", "length"];

/// Network-handle (`Net`) methods — the HTTP server surface (`src/serve.rs`). A
/// listener (from `listen(port)`) has `accept`; a connection (from `accept()`) has
/// `respond`. Effects, dispatched at runtime like the other opaque types.
pub static NET_METHODS: &[&str] = &["accept", "poll", "request", "respond", "sse", "send"];

/// Look up a builtin by its dotted path.
pub fn lookup(path: &str) -> Option<&'static BuiltinDef> {
    BUILTINS.iter().find(|b| b.path == path)
}

/// Iterator over every builtin's dotted path.
pub fn names() -> impl Iterator<Item = &'static str> {
    BUILTINS.iter().map(|b| b.path)
}

/// Whether `name` is a *known* builtin that is impure — the memoization analysis's
/// "this call has a side effect" test. A non-builtin name (e.g. a user function) is
/// not classified here; the caller judges those separately.
pub fn is_impure_builtin(name: &str) -> bool {
    lookup(name).is_some_and(|b| !b.pure)
}

/// A receiver type's full method-name set including the universal methods, for
/// "did you mean?" suggestions and error hints (`is_missing` is universal, so it is
/// chained in here rather than repeated in every per-type table).
pub fn methods_of(table: &[&'static str]) -> Vec<&'static str> {
    table.iter().copied().chain(UNIVERSAL_METHODS.iter().copied()).collect()
}

/// The method tables by receiver type — the single source for `helix doc <Type>`
/// introspection and the method-uniqueness test.
pub fn type_method_tables() -> [(&'static str, &'static [&'static str]); 8] {
    [
        ("Array", ARRAY_METHODS),
        ("String", STRING_METHODS),
        ("Dna", DNA_METHODS),
        ("Tensor", TENSOR_METHODS),
        ("DataFrame", DF_METHODS),
        ("GroupBy", GROUPBY_METHODS),
        ("Dict", DICT_METHODS),
        ("Net", NET_METHODS),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// No builtin path may be declared twice — the guarantee that makes the registry a
    /// true single source of truth (rustc's `symbols!{}` uniqueness, as a test).
    #[test]
    fn builtin_paths_are_unique() {
        let mut seen = HashSet::new();
        for b in BUILTINS {
            assert!(seen.insert(b.path), "duplicate builtin path `{}`", b.path);
        }
    }

    /// No method name may be repeated within a receiver's table, and no per-type table
    /// may redeclare a universal method (`is_missing` lives only in UNIVERSAL_METHODS).
    #[test]
    fn method_names_are_unique_and_disjoint_from_universal() {
        for (who, table) in [
            ("Array", ARRAY_METHODS),
            ("String", STRING_METHODS),
            ("Dna", DNA_METHODS),
            ("Tensor", TENSOR_METHODS),
            ("DataFrame", DF_METHODS),
            ("GroupBy", GROUPBY_METHODS),
            ("Net", NET_METHODS),
        ] {
            let mut seen = HashSet::new();
            for &m in table {
                assert!(seen.insert(m), "duplicate {who} method `{m}`");
                assert!(
                    !UNIVERSAL_METHODS.contains(&m),
                    "{who} method `{m}` duplicates a universal method"
                );
            }
        }
    }
}

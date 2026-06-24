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
//! Builtins are keyed by a dotted `path`: language primitives keep a bare name
//! (`"sqrt"`), and domain functions live under a namespace (`"bio.read_vcf"`,
//! `"stats.t_test"`). Methods are *not* namespaced — `recv.method()` — so their tables
//! are plain name lists; `is_missing` is universal across every receiver, so it is held
//! once in [`UNIVERSAL_METHODS`] rather than repeated in each table.

/// A built-in function: its dotted path and whether it is pure (free of side effects
/// and reproducible). Impure builtins (I/O, output, network) must never be memoized.
pub struct BuiltinDef {
    pub path: &'static str,
    pub pure: bool,
}

/// Every built-in function, keyed by dotted path. Language primitives are bare names;
/// domain functions will move under namespaces in a later step.
pub static BUILTINS: &[BuiltinDef] = &[
    // --- effectful / non-reproducible (I/O, output, network) -> not memoizable ---
    BuiltinDef { path: "print", pure: false },
    BuiltinDef { path: "io.read_csv", pure: false },
    BuiltinDef { path: "io.read_parquet", pure: false },
    BuiltinDef { path: "bio.read_fasta", pure: false },
    BuiltinDef { path: "bio.read_fastq", pure: false },
    BuiltinDef { path: "bio.read_vcf", pure: false },
    BuiltinDef { path: "io.write_parquet", pure: false },
    BuiltinDef { path: "http.get", pure: false },
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
    BuiltinDef { path: "floor", pure: true },
    BuiltinDef { path: "ceil", pure: true },
    BuiltinDef { path: "round", pure: true },
    BuiltinDef { path: "trunc", pure: true },
    BuiltinDef { path: "sign", pure: true },
    BuiltinDef { path: "degrees", pure: true },
    BuiltinDef { path: "radians", pure: true },
    BuiltinDef { path: "hypot", pure: true },
    BuiltinDef { path: "min", pure: true },
    BuiltinDef { path: "max", pure: true },
    BuiltinDef { path: "erf", pure: true },
    // --- statistics ---
    BuiltinDef { path: "stats.normal_cdf", pure: true },
    BuiltinDef { path: "stats.normal_pdf", pure: true },
    BuiltinDef { path: "stats.correlation", pure: true },
    BuiltinDef { path: "stats.t_test", pure: true },
    BuiltinDef { path: "stats.linear_regression", pure: true },
    BuiltinDef { path: "stats.multiple_regression", pure: true },
    // --- statistics: descriptive helpers (formerly the std.stats Helix module) ---
    BuiltinDef { path: "stats.standard_error", pure: true },
    BuiltinDef { path: "stats.coefficient_of_variation", pure: true },
    BuiltinDef { path: "stats.iqr", pure: true },
    BuiltinDef { path: "stats.spread", pure: true },
    BuiltinDef { path: "stats.zscores", pure: true },
    // --- sequence helpers (formerly the std.seq Helix module) ---
    BuiltinDef { path: "bio.at_content", pure: true },
    BuiltinDef { path: "bio.mean_gc", pure: true },
    BuiltinDef { path: "bio.total_length", pure: true },
    // --- data formats ---
    BuiltinDef { path: "json.parse", pure: true },
    BuiltinDef { path: "json.stringify", pure: true },
];

/// Methods universal to every receiver type (handled before the per-type dispatch).
pub static UNIVERSAL_METHODS: &[&str] = &["is_missing"];

/// Array methods (comprehension verbs, aggregations, statistics, transforms).
pub static ARRAY_METHODS: &[&str] = &[
    "mean", "std", "median", "var", "quantile", "summary", "sum", "min", "max", "count",
    "normalize", "sort", "reverse", "first", "last", "map", "filter", "where", "reduce", "any",
    "all", "take", "drop", "zip", "enumerate", "top", "drop_missing",
];

/// String methods.
pub static STRING_METHODS: &[&str] = &["upper", "lower", "count", "reverse"];

/// DNA-sequence methods.
pub static DNA_METHODS: &[&str] =
    &["gc_content", "reverse_complement", "complement", "kmers", "find", "length"];

/// Tensor methods (shape, aggregations, linear algebra).
pub static TENSOR_METHODS: &[&str] = &[
    "shape", "ndim", "count", "sum", "mean", "min", "max", "flatten", "reshape", "transpose", "t",
    "matmul", "dot", "norm", "det", "inv", "solve",
];

/// DataFrame methods (column verbs + value methods).
pub static DF_METHODS: &[&str] = &[
    "where", "filter", "select", "sort", "group", "with", "join", "column", "head", "count",
    "columns", "cache",
];

/// Grouped-DataFrame aggregations.
pub static GROUPBY_METHODS: &[&str] = &["mean", "sum", "min", "max", "count", "std"];

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

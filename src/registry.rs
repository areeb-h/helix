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

/// A coarse *purpose* category for a builtin — the one place the ~120 free functions are
/// grouped by what they do, for `helix describe` and human navigation. The user-facing names
/// stay flat (ADR 0017 removed the `math.`/`io.`/`stats.` prefixes); this is metadata, not a
/// namespace. A name not listed here defaults to `"core"`. The [`tests`] module asserts every
/// builtin lands in a real category (nothing silently falls through).
pub fn category_of(name: &str) -> &'static str {
    match name {
        // Filesystem I/O (readers + the two writers).
        "read_csv" | "read_parquet" | "read_text" | "read_json" | "read_dir" | "file_exists"
        | "remove_file" | "mkdir" | "read_fasta" | "read_fastq" | "read_vcf" | "read_bcf"
        | "read_sam" | "read_bam" | "read_gff" | "read_bed" => "io",
        // Networking — client + server.
        "listen" | "http_get" | "http_post" | "http_request" | "http_stream" => "net",
        // Program output / streaming sinks.
        "print" | "emit" | "write" | "elog" => "output",
        // Console input.
        "read_int" => "input",
        // Time / pacing.
        "sleep" | "clock_monotonic" => "time",
        // Constructors — make a value of a given type.
        "dna" | "headers" | "range" | "tensor" | "zeros" | "ones" | "eye" | "to_array" | "to_dataframe"
        | "to_tensor" | "dataframe" | "dict" | "rational" | "linspace" => "constructor",
        // Scalar conversions / accessors.
        "chr" | "ord" | "to_float" | "to_int" | "numerator" | "denominator" => "conversion",
        // Math — elementwise, broadcasting, missing-propagating.
        "sqrt" | "cbrt" | "abs" | "exp" | "ln" | "log10" | "log2" | "log" | "sin" | "cos"
        | "tan" | "asin" | "acos" | "atan" | "atan2" | "sinh" | "cosh" | "tanh" | "floor"
        | "ceil" | "round" | "trunc" | "sign" | "is_nan" | "is_finite" | "is_infinite"
        | "degrees" | "radians" | "hypot" | "min" | "max" | "clamp" | "erf" | "gcd" | "isqrt"
        | "primes" | "argmax" | "argmin" => "math",
        // Neural-net activations.
        "relu" | "sigmoid" => "nn",
        // Cryptography.
        "sha256" | "hmac_sha256" | "aes_keygen" | "aes_encrypt" | "aes_decrypt"
        | "ed25519_keygen" | "ed25519_sign" | "ed25519_verify" => "crypto",
        // Encoding.
        "base64_encode" | "base64_decode" | "hex_encode" | "hex_decode" | "url_encode"
        | "url_decode" | "url_decode_lenient" => "encoding",
        "parse_cookies" | "parse_set_cookie" | "cookie_jar" => "http",
        // Reproducible random.
        "random" | "randn" | "random_int" => "random",
        // Statistics, ML metrics, regression.
        "mse" | "rmse" | "mae" | "r2_score" | "aic" | "bic" | "accuracy" | "precision"
        | "recall" | "f1_score" | "confusion_matrix" | "normal_cdf" | "normal_pdf"
        | "correlation" | "t_test" | "linear_regression" | "multiple_regression"
        | "least_squares" => "stats",
        // Lattice / number-theoretic.
        "lll" | "lll_exact" => "linalg",
        // Automatic differentiation.
        "variable" | "value_of" | "gradient" => "autodiff",
        // Bioinformatics.
        "align" => "bio",
        // Assertions / test helpers.
        "assert" | "assert_eq" | "assert_close" | "assert_error" => "assert",
        // Raising a domain error — a guard like the asserts, minus the condition.
        "raise" => "assert",
        // Locating a module's own shipped files. `io` because that is what it exists to
        // feed: the argument of a `read_*` call that must not depend on the working
        // directory. It performs no I/O itself, which is why it is `pure`.
        "source_path" => "io",
        _ => "core",
    }
}

/// Every built-in function, keyed by a flat name (no namespace prefixes — ADR 0017).
pub static BUILTINS: &[BuiltinDef] = &[
    // --- effectful / non-reproducible (I/O, output, network) -> not memoizable ---
    BuiltinDef { path: "print", pure: false },
    BuiltinDef { path: "emit", pure: false },
    BuiltinDef { path: "write", pure: false },
    BuiltinDef { path: "elog", pure: false },
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
    BuiltinDef { path: "http_stream", pure: false },
    // --- constructors / conversions ---
    BuiltinDef { path: "dna", pure: true },
    BuiltinDef { path: "headers", pure: true },
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
    // Read one integer from stdin (a line). Impure (non-deterministic input), so it
    // never appears in a computed/memoized result.
    BuiltinDef { path: "read_int", pure: false },
    BuiltinDef { path: "degrees", pure: true },
    BuiltinDef { path: "radians", pure: true },
    BuiltinDef { path: "hypot", pure: true },
    BuiltinDef { path: "min", pure: true },
    BuiltinDef { path: "max", pure: true },
    BuiltinDef { path: "clamp", pure: true },
    BuiltinDef { path: "erf", pure: true },
    BuiltinDef { path: "gcd", pure: true },
    BuiltinDef { path: "isqrt", pure: true },
    BuiltinDef { path: "primes", pure: true },
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
    // Percent-encoding (RFC 3986) — pure, and the pair a URL cannot be built without.
    BuiltinDef { path: "url_encode", pure: true },
    BuiltinDef { path: "url_decode", pure: true },
    BuiltinDef { path: "url_decode_lenient", pure: true },
    // Cookies, both directions — pure parsing of a header value.
    BuiltinDef { path: "parse_cookies", pure: true },
    BuiltinDef { path: "parse_set_cookie", pure: true },
    // A fresh cookie jar. NOT pure: two calls yield two distinct, independently
    // mutable jars, so it must not be memoized.
    BuiltinDef { path: "cookie_jar", pure: false },
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
    BuiltinDef { path: "assert_error", pure: false },
    BuiltinDef { path: "assert_close", pure: false },
    // `raise(message[, help])` — the unconditional form. A library reporting a caller's
    // mistake had only `assert`, which hard-codes "assertion failed: " and cannot carry a
    // `help:` line, so a domain error read as "this library is broken". Impure for the same
    // reason as the asserts: raising is the point, and must never be memoized away.
    BuiltinDef { path: "raise", pure: false },
    // `source_path(rel)` — `rel` resolved against the directory of the FILE THE CALL IS
    // WRITTEN IN, so a package can read the data it ships regardless of the process's
    // working directory. Pure: it only computes a string, and performs no I/O itself.
    BuiltinDef { path: "source_path", pure: true },
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
    "frequencies", "unique", "drop_missing", "drop_nan",
    "join", "concat", "flatten", "min_by", "max_by", "argmin", "argmax", "zipmap", "sort_by", "zscores", "iqr",
    "spread", "standard_error", "coefficient_of_variation", "dot", "norm", "cumsum", "product",
    "argsort", "clamp", "softmax", "bootstrap", "contains", "mean_gc",
    "total_length", "bar_chart", "histogram", "line_chart", "sparkline", "scatter", "svg_bar",
    "svg_line", "write_csv", "write_tsv", "write_json", "to_html", "to_markdown", "to_table",
    "to_dict", "write_fasta", "write_fastq", "shuffle", "sample", "choice",
    "windows", "chunks", "flat_map", "count_where",
];

/// String methods.
pub static STRING_METHODS: &[&str] = &[
    "upper", "lower", "count", "length", "chars", "reverse", "trim", "split", "replace", "contains",
    "starts_with", "ends_with", "take", "drop", "repeat", "ljust", "rjust", "center", "phred",
    "parse_json", "to_float", "to_int", "write_to", "append_to", "index_of", "split_once",
    "concat", "replace_first", "last_index_of",
];

/// HTTP-header methods (the `Headers` value): case-insensitive reads, wire order.
pub static HEADERS_METHODS: &[&str] = &[
    "get", "get_all", "has", "contains", "keys", "values", "items", "count", "length",
    "to_dict",
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
    "where", "filter", "drop_missing", "drop_nan", "select", "sort", "group", "with", "join", "vstack", "unique", "column",
    "head", "count", "columns", "cache", "write_csv", "write_tsv", "write_json", "write_parquet",
    "to_html", "to_markdown", "to_table",
];

/// Grouped-DataFrame aggregations.
pub static GROUPBY_METHODS: &[&str] = &["mean", "sum", "min", "max", "count", "std"];

/// Keyed-map (`Dict`) methods — O(log n) lookup, sorted (deterministic) enumeration.
pub static DICT_METHODS: &[&str] = &[
    "get", "expect", "contains", "has", "keys", "values", "items", "insert", "remove", "count", "length",
];

/// Record methods for **dynamic** field access — `get(k[, default])` (value or missing/
/// default), `has(k)`, `keys()`/`values()`/`items()`. The escape hatch for consuming
/// unknown-shape data (a parsed JSON response) where a static `rec.field` would be a compile
/// error. Static field access is the normal path; these are for runtime-unknown shapes.
pub static RECORD_METHODS: &[&str] = &["get", "expect", "has", "keys", "values", "items"];

/// Network-handle (`Net`) methods — the HTTP server surface (`src/serve.rs`). A
/// listener (from `listen(port)`) has `accept`; a connection (from `accept()`) has
/// `respond`. Effects, dispatched at runtime like the other opaque types.
pub static NET_METHODS: &[&str] = &[
    "accept", "poll", "request", "respond", "sse", "send", "next", "status", "close",
    // Cooperative event-loop server (keep-alive, one thread serves many connections):
    "accept_poll", "poll_request", "is_open", "wait",
    // Cookie jar (the `Net` value also carries a jar): inspect and clear.
    "cookies", "clear",
];

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
pub fn type_method_tables() -> [(&'static str, &'static [&'static str]); 10] {
    [
        ("Array", ARRAY_METHODS),
        ("String", STRING_METHODS),
        ("Dna", DNA_METHODS),
        ("Tensor", TENSOR_METHODS),
        ("DataFrame", DF_METHODS),
        ("GroupBy", GROUPBY_METHODS),
        ("Dict", DICT_METHODS),
        ("Net", NET_METHODS),
        ("Record", RECORD_METHODS),
        // LAST deliberately: Headers shares names (count, get, keys, items) with the
        // common types, and whichever table lists a name first becomes its owner for
        // did-you-mean examples — "xs.count()" must not become "x.count()" because a
        // header map also counts.
        ("Headers", HEADERS_METHODS),
    ]
}

/// Is this name a free builtin? O(1) — the set is built once, because this is asked on
/// EVERY method call: it is the gate on the UFCS builtin fallback, and a linear scan of
/// 135 defs per call would tax exactly the hot loops the fallback must not touch. None
/// of the hot method names (`get`, `sum`, `map`, `count`, …) are builtins, so for them
/// this single lookup is the entire cost of the feature.
pub fn is_builtin_name(name: &str) -> bool {
    use std::sync::OnceLock;
    static SET: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| BUILTINS.iter().map(|b| b.path).collect()).contains(name)
}

/// Does this receiver TYPE claim this method in its static table? O(1), same reasoning
/// as [`is_builtin_name`] — consulted only for builtin-named methods, where it decides
/// whether the method owns the name (and the fallback must stay out) or does not (and a
/// failed dispatch may retry as `name(recv, …)`).
pub fn type_owns_method(type_name: &str, method: &str) -> bool {
    use std::sync::OnceLock;
    type Table = std::collections::HashMap<&'static str, std::collections::HashSet<&'static str>>;
    static MAP: OnceLock<Table> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        type_method_tables()
            .iter()
            .map(|(t, methods)| (*t, methods.iter().copied().collect()))
            .collect()
    });
    UNIVERSAL_METHODS.contains(&method)
        || map.get(type_name).is_some_and(|m| m.contains(method))
}

/// Is this name a method on ANY receiver type (including the universal ones)?
///
/// The gate on the UFCS fallback in the parser: a name that some type answers must keep
/// meaning "method call", so a misspelled or wrong-receiver method still produces the
/// method error and its did-you-mean, and no call that resolves today changes meaning.
/// Only a name no type claims can be a free function called in method position.
pub fn is_any_method(name: &str) -> bool {
    UNIVERSAL_METHODS.contains(&name)
        || type_method_tables().iter().any(|(_, t)| t.contains(&name))
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

    /// Every builtin lands in a real category — a new one that isn't added to `category_of`
    /// falls through to `"core"` and fails here, so the catalog stays fully organized.
    #[test]
    fn every_builtin_has_a_category() {
        for b in BUILTINS {
            assert_ne!(
                category_of(b.path),
                "core",
                "builtin `{}` has no category — add it to `category_of`",
                b.path
            );
        }
    }

    /// No method name may be repeated within a receiver's table, and no per-type table
    /// may redeclare a universal method (`is_missing` lives only in UNIVERSAL_METHODS).
    #[test]
    fn method_names_are_unique_and_disjoint_from_universal() {
        // Iterate the single source (`type_method_tables`) so every receiver — including Dict
        // and Record — is covered, not a hand-maintained subset that can drift.
        for (who, table) in type_method_tables() {
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

//! Migration helper for the **removed** stdlib namespaces.
//!
//! Helix once exposed domain builtins under compile-time prefixes (`bio.read_vcf`,
//! `stats.t_test`, `io.write_csv`, `json.parse`, `chart.bar`, …). Those prefixes
//! were never real modules — just a naming convention rewritten into flat calls —
//! so they were dropped (ADR 0017): readers and symmetric operations became plain
//! free functions (`read_vcf`, `t_test`), and everything that acts on data became a
//! method (`value.to_json()`, `xs.bar_chart()`, `df.write_csv(p)`).
//!
//! Nothing rewrites anymore. This module survives only to turn an *old* `ns.member`
//! call into a precise migration error, so anyone with pre-ADR-0017 code gets a
//! pointer to the new spelling instead of a bare "not defined".

/// The removed namespace names — recognized only to produce the migration hint.
const REMOVED_NAMESPACES: &[&str] = &["bio", "stats", "io", "json", "http", "chart", "export"];

/// Whether `name` was one of the removed namespace prefixes. Used by the checker to
/// flag a bare `stats` (used as a value) and an old `stats.member(...)` call.
pub fn is_namespace(name: &str) -> bool {
    REMOVED_NAMESPACES.contains(&name)
}

/// If `ns.member` names an old namespaced builtin, return the hint describing its
/// new spelling (a free function, or a method on the data). `None` for anything
/// that wasn't a removed namespace.
pub fn migration_hint(ns: &str, member: &str) -> Option<String> {
    if !is_namespace(ns) {
        return None;
    }
    // Free functions (just drop the prefix): readers, http, symmetric stats/dists.
    let free = matches!(
        (ns, member),
        ("io", "read_csv")
            | ("io", "read_parquet")
            | ("bio", "read_fasta")
            | ("bio", "read_fastq")
            | ("bio", "read_vcf")
            | ("bio", "read_bcf")
            | ("bio", "read_sam")
            | ("bio", "read_bam")
            | ("bio", "read_gff")
            | ("bio", "read_bed")
            | ("http", "get")
            | ("stats", "t_test")
            | ("stats", "correlation")
            | ("stats", "linear_regression")
            | ("stats", "multiple_regression")
            | ("stats", "normal_cdf")
            | ("stats", "normal_pdf")
    );
    if free {
        let f = if ns == "http" && member == "get" { "http_get" } else { member };
        return Some(format!(
            "`{ns}.{member}` is now the free function `{f}(...)` — drop the `{ns}.` prefix."
        ));
    }
    // Everything that acts on data is now a method; map old member → new method.
    let method = match (ns, member) {
        ("json", "parse") => "parse_json",
        ("json", "stringify") => "to_json",
        ("io", "write") => "write_to",
        ("io", "append") => "append_to",
        ("io", "write_csv") => "write_csv",
        ("io", "write_tsv") => "write_tsv",
        ("io", "write_json") => "write_json",
        ("io", "write_parquet") => "write_parquet",
        ("bio", "write_fasta") => "write_fasta",
        ("bio", "write_fastq") => "write_fastq",
        ("bio", "mean_gc") => "mean_gc",
        ("bio", "total_length") => "total_length",
        ("bio", "at_content") => "at_content",
        ("stats", "iqr") => "iqr",
        ("stats", "spread") => "spread",
        ("stats", "zscores") => "zscores",
        ("stats", "standard_error") => "standard_error",
        ("stats", "coefficient_of_variation") => "coefficient_of_variation",
        ("chart", "bar") => "bar_chart",
        ("chart", "hist") => "histogram",
        ("chart", "line") => "line_chart",
        ("chart", "scatter") => "scatter",
        ("chart", "sparkline") => "sparkline",
        ("export", "markdown") => "to_markdown",
        ("export", "html") => "to_html",
        ("export", "svg_bar") => "svg_bar",
        ("export", "svg_line") => "svg_line",
        _ => {
            return Some(format!(
                "the `{ns}.` namespace was removed — call `{member}` directly, or as a method."
            ))
        }
    };
    Some(format!("`{ns}.{member}` is now the method `data.{method}(...)` — call it on the data."))
}

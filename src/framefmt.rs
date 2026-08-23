//! The **frozen frame text format** (ADR 0033, Stage 0) — what `print(df)` and
//! string interpolation emit when output is not an interactive terminal.
//!
//! Before this module, that text was whatever the engine's own `Display` printed
//! (polars' box table), which made piped program output depend on the DataFrame
//! engine and on `POLARS_FMT_*` environment variables — a violation of the
//! byte-identity doctrine `render.rs` states for every other value. This module
//! OWNS the format: engine-agnostic (built purely from the ADR 0012 seam),
//! deterministic, and environment-insensitive. When the native engine lands
//! (ADR 0033 Stage 1), frame output does not change by a byte.
//!
//! ## The spec
//!
//! ```text
//! region  samples  mean_af
//! ------  -------  -------
//! east         12      0.5
//! west    missing     0.25
//! (2 rows)
//! ```
//!
//! 1. The preview is the first [`PREVIEW_ROWS`] rows, pulled through the seam
//!    (`head` → `column_values`).
//! 2. Cells format exactly as the language's own scalars: `Int` as plain digits
//!    (no thousands grouping — `"{n}"` in interpolation has none), `Float` via
//!    [`crate::value::fmt_float`] (`2.0`, not `2`), `Bool` as `true`/`false`,
//!    `Missing` as `missing`, strings as their text (unquoted — a table column,
//!    like the rich renderer) with `\n`/`\t` escaped so a cell cannot break the
//!    table's line structure.
//! 3. A cell longer than [`CELL_MAX`] characters is cut and marked with `…`.
//! 4. Column width = the widest of the header and the shown cells. A column whose
//!    shown cells are all numeric (`Int`/`Float`/`Missing`) is right-aligned;
//!    anything else is left-aligned. Two spaces separate columns; lines carry no
//!    trailing whitespace.
//! 5. The footer is `(N rows)` — `(1 row)` when N is 1 — or
//!    `(showing K of N rows)` when the preview truncates. A frame with no
//!    columns prints `(empty dataframe)`.
//!
//! Changing anything above is a versioned, release-noted format change — this
//! text is program output, and programs get diffed.

use crate::backend::DataHandle;
use crate::error::HelixError;
use crate::value::{fmt_float, Value};

/// Rows shown before the footer truncates the preview.
pub const PREVIEW_ROWS: usize = 10;

/// The widest a single cell may render.
const CELL_MAX: usize = 60;

/// Render a frame in the frozen format. Materializes the plan (this is the
/// printing path — the same "printing is the materialization point" contract the
/// engine Display had), so a failing query surfaces as a positioned error.
pub fn frame_text(df: &dyn DataHandle, line: usize, col: usize) -> Result<String, HelixError> {
    let names = df.column_names(line, col)?;
    if names.is_empty() {
        return Ok("(empty dataframe)".to_string());
    }
    let total = df.row_count(line, col)?;
    let preview = df.head(PREVIEW_ROWS);
    let cols: Vec<Vec<Value>> =
        names.iter().map(|n| preview.column_values(n, line, col)).collect::<Result<_, _>>()?;
    let shown = cols.first().map(|c| c.len()).unwrap_or(0);

    let cells: Vec<Vec<String>> =
        cols.iter().map(|c| c.iter().map(cell_text).collect()).collect();
    let numeric: Vec<bool> = cols
        .iter()
        .map(|c| c.iter().all(|v| matches!(v, Value::Int(_) | Value::Float(_) | Value::Missing)))
        .collect();
    let widths: Vec<usize> = names
        .iter()
        .zip(&cells)
        .map(|(h, col)| col.iter().map(|s| s.chars().count()).max().unwrap_or(0).max(h.chars().count()))
        .collect();

    let mut out = String::new();
    let push_row = |fields: &dyn Fn(usize) -> String, out: &mut String| {
        let mut line_s = String::new();
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                line_s.push_str("  ");
            }
            let f = fields(i);
            let pad = w.saturating_sub(f.chars().count());
            if numeric[i] {
                line_s.push_str(&" ".repeat(pad));
                line_s.push_str(&f);
            } else {
                line_s.push_str(&f);
                line_s.push_str(&" ".repeat(pad));
            }
        }
        out.push_str(line_s.trim_end());
        out.push('\n');
    };

    push_row(&|i| names[i].clone(), &mut out);
    push_row(&|i| "-".repeat(widths[i]), &mut out);
    // Row index over column-major storage — the closure reads `cells[i][r]` per
    // column, so there is no single iterator to lift this onto.
    #[allow(clippy::needless_range_loop)]
    for r in 0..shown {
        push_row(&|i| cells[i][r].clone(), &mut out);
    }
    let noun = if total == 1 { "row" } else { "rows" };
    if shown < total {
        out.push_str(&format!("(showing {shown} of {total} {noun})"));
    } else {
        out.push_str(&format!("({total} {noun})"));
    }
    Ok(out)
}

/// The infallible variant for `Display`, which has no position to error at: a
/// failing query renders as a placeholder (the fallible `display_value` path is
/// what `print`/interpolation actually use, and it errors properly).
pub fn frame_text_lossy(df: &dyn DataHandle) -> String {
    match frame_text(df, 0, 0) {
        Ok(s) => s,
        Err(e) => format!("<dataframe — query failed: {}>", e.message),
    }
}

/// One cell, by the frozen rules (spec item 2 and 3).
fn cell_text(v: &Value) -> String {
    let s = match v {
        Value::Int(i) => i.to_string(),
        Value::Float(x) => fmt_float(*x),
        Value::Bool(b) => b.to_string(),
        Value::Missing => "missing".to_string(),
        Value::Str(s) => (**s).clone(),
        Value::Dna(s) => (**s).clone(),
        other => other.to_string(),
    };
    let s = if s.contains('\n') || s.contains('\t') {
        s.replace('\n', "\\n").replace('\t', "\\t")
    } else {
        s
    };
    if s.chars().count() > CELL_MAX {
        let cut: String = s.chars().take(CELL_MAX - 1).collect();
        format!("{cut}…")
    } else {
        s
    }
}

#[cfg(test)]
#[cfg(feature = "dataframes")]
mod tests {
    use super::*;
    use crate::backend::{build_frame, ColData};

    fn demo() -> crate::backend::Df {
        build_frame(
            vec![
                ("region".to_string(), ColData::Str(vec!["east".into(), "west".into()])),
                ("samples".to_string(), ColData::IntOpt(vec![Some(12), None])),
                ("mean_af".to_string(), ColData::Float(vec![Some(0.5), Some(0.25)])),
            ],
            0,
            0,
        )
        .expect("demo frame builds")
    }

    /// One row is a row: the footer singularizes, per spec rule 5.
    #[test]
    fn a_single_row_footer_is_singular() {
        let df = build_frame(vec![("x".to_string(), ColData::Int(vec![7]))], 0, 0).unwrap();
        let text = frame_text(&*df, 0, 0).unwrap();
        assert!(text.ends_with("(1 row)"), "footer was: {text:?}");
    }

    /// The spec's own example, byte for byte — the doc comment and the code can
    /// never drift apart.
    #[test]
    fn the_spec_example_is_the_output() {
        let text = frame_text(&*demo(), 0, 0).unwrap();
        let expected = "\
region  samples  mean_af
------  -------  -------
east         12      0.5
west    missing     0.25
(2 rows)";
        assert_eq!(text, expected);
    }

    #[test]
    fn floats_read_as_floats_and_ints_have_no_grouping() {
        let df = build_frame(
            vec![
                ("n".to_string(), ColData::Int(vec![1234567])),
                ("x".to_string(), ColData::Float(vec![Some(2.0)])),
            ],
            0,
            0,
        )
        .unwrap();
        let text = frame_text(&*df, 0, 0).unwrap();
        assert!(text.contains("1234567"), "no thousands grouping in the frozen format: {text}");
        assert!(text.contains("2.0"), "integral floats keep their point: {text}");
    }

    #[test]
    fn a_long_preview_truncates_with_an_honest_footer() {
        let df = build_frame(
            vec![("i".to_string(), ColData::Int((0..25).collect()))],
            0,
            0,
        )
        .unwrap();
        let text = frame_text(&*df, 0, 0).unwrap();
        assert!(text.ends_with("(showing 10 of 25 rows)"), "{text}");
        assert!(!text.contains("\n10\n"), "row 10 must not be shown: {text}");
    }
}

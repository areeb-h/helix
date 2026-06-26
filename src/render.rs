//! Rich terminal rendering for `print` and the REPL echo.
//!
//! Output *is* the product for a data/bio language, so Helix renders values far
//! better than a flat `print`: a homogeneous array of records becomes an aligned
//! table, numbers carry thousands separators, long collections are elided to a
//! head/tail with a count, and leaves are color-coded.
//!
//! The whole rich layer is gated on **stdout being a terminal** (overridable via
//! `HELIX_RICH`), so piped/redirected output is **byte-identical** to the plain
//! [`crate::value::display_value`] path — scripts stay scriptable and every test
//! and `vmparity` snapshot stays stable. Color is additionally gated on `NO_COLOR`
//! and `HELIX_COLOR`. None of this is on a hot path (it runs once per `print`).

use std::io::IsTerminal;

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::{display_value, fmt_float, Value};

// ANSI SGR color codes, applied only when `RenderOpts.color` is set.
const NUM: &str = "36"; // cyan      — Int / Float
const STR: &str = "32"; // green     — String
const DNA: &str = "33"; // yellow    — DNA sequence
const BOOL: &str = "35"; // magenta  — Bool
const MISSING: &str = "90"; // gray  — missing / elision markers
const KEY: &str = "34"; // blue      — record field names
const HEADER: &str = "1"; // bold    — table column headers
const DIM: &str = "2"; // dim        — brackets, rules, separators

/// Longest a single table cell renders before it is truncated with `…`.
const CELL_MAX: usize = 32;

/// How rendering should behave for one `print`. Built by [`RenderOpts::auto`] from
/// the environment; constructed explicitly in tests to exercise the rich path off
/// a real terminal.
#[derive(Clone, Debug)]
pub struct RenderOpts {
    /// Master switch: when false, callers fall back to the plain `display_value`
    /// path and this module is bypassed entirely.
    pub(crate) rich: bool,
    /// Emit ANSI color escapes.
    pub(crate) color: bool,
    /// Terminal width to fit tables/lists into.
    pub(crate) width: usize,
    /// Maximum rows (table) / elements (list) shown before head/tail elision.
    pub(crate) max_rows: usize,
}

impl RenderOpts {
    /// Detect from the environment: rich only on a TTY (or `HELIX_RICH=1`), color
    /// only when rich and not suppressed by `NO_COLOR`/`HELIX_COLOR=never`.
    pub fn auto() -> Self {
        let tty = std::io::stdout().is_terminal();
        let rich = match std::env::var("HELIX_RICH").ok().as_deref() {
            Some("1") | Some("always") => true,
            Some("0") | Some("never") => false,
            _ => tty,
        };
        let color = rich
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("HELIX_COLOR").ok().as_deref() != Some("never");
        RenderOpts { rich, color, width: term_width(), max_rows: 20 }
    }

    fn as_plain(&self) -> Self {
        RenderOpts { color: false, ..self.clone() }
    }
}

/// Terminal column count: the kernel's window size, else `$COLUMNS`, else 80.
#[cfg(unix)]
fn term_width() -> usize {
    // SAFETY: TIOCGWINSZ writes a `winsize` we zero-initialize; failure is detected
    // via the return code and the value left untouched.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    env_cols().unwrap_or(80)
}

#[cfg(not(unix))]
fn term_width() -> usize {
    env_cols().unwrap_or(80)
}

fn env_cols() -> Option<usize> {
    std::env::var("COLUMNS").ok()?.parse().ok()
}

/// Render the arguments of one `print(...)` call into the string to emit. The plain
/// path is a byte-for-byte mirror of the old behavior (`display_value` joined by a
/// space); the rich path renders each value and joins with a newline when any part
/// spans multiple lines (so a table never gets jammed onto one line).
pub fn render_print(args: &[Value], line: usize, col: usize) -> Result<String, HelixError> {
    let opts = RenderOpts::auto();
    if !opts.rich {
        let mut parts = Vec::with_capacity(args.len());
        for v in args {
            parts.push(display_value(v, line, col)?);
        }
        return Ok(parts.join(" "));
    }
    let mut parts = Vec::with_capacity(args.len());
    for v in args {
        match v {
            // A DataFrame still materializes its lazy plan here (a failed query is a
            // real error), exactly like the plain path.
            Value::DataFrame(_) => parts.push(display_value(v, line, col)?),
            _ => parts.push(render_value_with(v, &opts)),
        }
    }
    Ok(if parts.iter().any(|p| p.contains('\n')) {
        parts.join("\n")
    } else {
        parts.join(" ")
    })
}

/// Render a single value for the REPL's auto-echo. Falls back to plain `Display`
/// for anything fallible (a DataFrame) so the notebook echo never errors out.
pub fn render_echo(v: &Value) -> String {
    let opts = RenderOpts::auto();
    if !opts.rich || matches!(v, Value::DataFrame(_)) {
        return v.to_string();
    }
    render_value_with(v, &opts)
}

/// The rich entry point for a top-level (non-DataFrame) value.
pub(crate) fn render_value_with(v: &Value, opts: &RenderOpts) -> String {
    match v {
        Value::Array(a) => render_array(&a.to_values(), opts),
        Value::Record(f) => render_record(f, opts),
        // Tuples are small by nature → always inline.
        Value::Tuple(_) => render_compact(v, opts),
        _ => color_leaf(v, opts, false),
    }
}

// ---- leaves ----

fn paint(opts: &RenderOpts, code: &str, s: &str) -> String {
    if opts.color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Render a scalar leaf. `nested` quotes strings (matching `Display` inside
/// collections); a top-level string prints unquoted.
fn color_leaf(v: &Value, opts: &RenderOpts, nested: bool) -> String {
    match v {
        Value::Int(i) => paint(opts, NUM, &fmt_int(*i)),
        Value::Float(x) => paint(opts, NUM, &fmt_float_rich(*x)),
        Value::Bool(b) => paint(opts, BOOL, &b.to_string()),
        Value::Missing => paint(opts, MISSING, "missing"),
        Value::Str(s) => {
            let t = if nested { format!("\"{s}\"") } else { (**s).clone() };
            paint(opts, STR, &t)
        }
        Value::Dna(s) => paint(opts, DNA, s),
        Value::Unit => "()".to_string(),
        // Tensors, functions, group-bys, py-objects: keep their existing Display.
        other => other.to_string(),
    }
}

/// Format an `i64` with `_` thousands separators (mirroring Helix's own numeric
/// literal syntax, e.g. `1_000_000`). Handles `i64::MIN` via a 128-bit magnitude.
fn fmt_int(i: i64) -> String {
    let mag = (i as i128).unsigned_abs().to_string();
    let grouped = group_thousands(&mag);
    if i < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Like [`fmt_float`] but with grouped integer digits, and scientific notation for
/// extreme magnitudes (where separators would be noise).
fn fmt_float_rich(x: f64) -> String {
    if !x.is_finite() {
        return fmt_float(x);
    }
    let a = x.abs();
    if a != 0.0 && !(1e-4..1e15).contains(&a) {
        return format!("{x:e}");
    }
    let base = fmt_float(x);
    match base.find('.') {
        Some(dot) => {
            let (intp, frac) = base.split_at(dot);
            let neg = intp.starts_with('-');
            let digits = if neg { &intp[1..] } else { intp };
            format!("{}{}{}", if neg { "-" } else { "" }, group_thousands(digits), frac)
        }
        None => base,
    }
}

fn group_thousands(digits: &str) -> String {
    let n = digits.len();
    let mut out = String::with_capacity(n + n / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(ch);
    }
    out
}

// ---- compact (single-line) rendering, for list elements and nested cells ----

/// Render `v` as a single colored line — used for list elements and nested cells.
fn render_compact(v: &Value, opts: &RenderOpts) -> String {
    match v {
        Value::Array(a) => {
            let inner: Vec<String> =
                a.to_values().iter().map(|e| render_compact(e, opts)).collect();
            format!("{}{}{}", paint(opts, DIM, "["), inner.join(", "), paint(opts, DIM, "]"))
        }
        Value::Tuple(items) => {
            let mut inner: Vec<String> =
                items.iter().map(|e| render_compact(e, opts)).collect();
            if items.len() == 1 {
                inner.push(String::new()); // trailing comma → `(x,)`
            }
            format!("{}{}{}", paint(opts, DIM, "("), inner.join(", "), paint(opts, DIM, ")"))
        }
        Value::Record(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(k, val)| {
                    format!("{}: {}", paint(opts, KEY, k.as_str()), render_compact(val, opts))
                })
                .collect();
            format!("{}{}{}", paint(opts, DIM, "{"), inner.join(", "), paint(opts, DIM, "}"))
        }
        Value::DataFrame(_) => paint(opts, DIM, "<dataframe>"),
        leaf => color_leaf(leaf, opts, true),
    }
}

fn compact_plain(v: &Value, opts: &RenderOpts) -> String {
    render_compact(v, &opts.as_plain())
}

// ---- records ----

fn render_record(fields: &[(Symbol, Value)], opts: &RenderOpts) -> String {
    let inline = render_compact(&Value::Record(std::rc::Rc::new(fields.to_vec())), opts);
    let plain_w = dw(&compact_plain(&Value::Record(std::rc::Rc::new(fields.to_vec())), opts));
    if plain_w <= opts.width || fields.len() <= 1 {
        return inline;
    }
    // Too wide: one field per line.
    let mut s = paint(opts, DIM, "{");
    s.push('\n');
    for (k, val) in fields {
        s.push_str("  ");
        s.push_str(&paint(opts, KEY, k.as_str()));
        s.push_str(": ");
        s.push_str(&render_compact(val, opts));
        s.push_str(",\n");
    }
    s.push_str(&paint(opts, DIM, "}"));
    s
}

// ---- arrays: table or list ----

fn render_array(vals: &[Value], opts: &RenderOpts) -> String {
    if vals.is_empty() {
        return paint(opts, DIM, "[]");
    }
    if let Some(keys) = table_keys(vals) {
        return render_table(vals, &keys, opts);
    }
    render_list(vals, opts)
}

/// `Some(keys)` iff every element is a non-empty record with the *same field names
/// in the same order* — the shape that renders as a table.
fn table_keys(vals: &[Value]) -> Option<Vec<Symbol>> {
    let first = match &vals[0] {
        Value::Record(f) if !f.is_empty() => f,
        _ => return None,
    };
    let keys: Vec<Symbol> = first.iter().map(|(k, _)| *k).collect();
    for v in &vals[1..] {
        let Value::Record(f) = v else { return None };
        if f.len() != keys.len() {
            return None;
        }
        if f.iter().zip(&keys).any(|((k, _), want)| k != want) {
            return None;
        }
    }
    Some(keys)
}

/// Per-cell content + style + whether it's numeric (right-aligned).
struct Cell {
    plain: String,
    style: &'static str,
    numeric: bool,
}

fn cell_of(v: &Value, opts: &RenderOpts) -> Cell {
    let (plain, style, numeric) = match v {
        Value::Int(i) => (fmt_int(*i), NUM, true),
        Value::Float(x) => (fmt_float_rich(*x), NUM, true),
        Value::Bool(b) => (b.to_string(), BOOL, false),
        Value::Missing => ("missing".to_string(), MISSING, true),
        Value::Str(s) => ((**s).clone(), STR, false), // unquoted in a table column
        Value::Dna(s) => ((**s).clone(), DNA, false),
        Value::DataFrame(_) => ("<dataframe>".to_string(), DIM, false),
        nested => (compact_plain(nested, opts), DIM, false),
    };
    Cell { plain: truncate_str(&plain, CELL_MAX), style, numeric }
}

fn render_table(vals: &[Value], keys: &[Symbol], opts: &RenderOpts) -> String {
    let ncol = keys.len();
    let headers: Vec<String> = keys.iter().map(|k| k.as_str().to_string()).collect();

    let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(vals.len());
    let mut col_numeric = vec![true; ncol];
    for v in vals {
        let Value::Record(f) = v else { continue };
        let mut row = Vec::with_capacity(ncol);
        for (i, (_, val)) in f.iter().enumerate() {
            let cell = cell_of(val, opts);
            if !cell.numeric {
                col_numeric[i] = false;
            }
            row.push(cell);
        }
        rows.push(row);
    }

    // Column widths: the widest of the (already cell-capped) header and cells.
    let mut widths: Vec<usize> =
        headers.iter().map(|h| dw(h).min(CELL_MAX)).collect();
    for row in &rows {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(dw(&c.plain));
        }
    }

    // Fit to terminal width: keep columns left-to-right while they fit (always keep
    // at least the first), counting any we have to drop.
    let sep_w = 3; // " │ "
    let mut kept: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for (i, &w) in widths.iter().enumerate() {
        let add = w + if kept.is_empty() { 0 } else { sep_w };
        if !kept.is_empty() && used + add > opts.width {
            break;
        }
        used += add;
        kept.push(i);
    }
    let dropped = ncol - kept.len();

    // Row elision: head + tail with a count between.
    let n = rows.len();
    let (head, tail) = if n > opts.max_rows {
        let t = (opts.max_rows / 5).max(1);
        (opts.max_rows - t, t)
    } else {
        (n, 0)
    };

    let mut out = String::new();
    // Header.
    emit_row_strs(&mut out, &headers, &kept, &widths, &col_numeric, HEADER, opts);
    // Rule.
    out.push('\n');
    let rule: String = kept
        .iter()
        .map(|&i| "─".repeat(widths[i]))
        .collect::<Vec<_>>()
        .join("─┼─");
    out.push_str(&paint(opts, DIM, &rule));
    // Data rows.
    let emit_data = |out: &mut String, row: &[Cell]| {
        out.push('\n');
        for (j, &i) in kept.iter().enumerate() {
            if j > 0 {
                out.push_str(&paint(opts, DIM, " │ "));
            }
            emit_cell(out, &row[i].plain, row[i].style, widths[i], col_numeric[i], opts);
        }
    };
    for row in rows.iter().take(head) {
        emit_data(&mut out, row);
    }
    if n > opts.max_rows {
        out.push('\n');
        out.push_str(&paint(opts, MISSING, &format!("  … {} more rows", n - head - tail)));
        for row in rows.iter().skip(n - tail) {
            emit_data(&mut out, row);
        }
    }
    if dropped > 0 {
        out.push('\n');
        out.push_str(&paint(opts, MISSING, &format!("  …+{dropped} more columns")));
    }
    out
}

/// Emit a row whose cells are all the same style (used for the header).
fn emit_row_strs(
    out: &mut String,
    cells: &[String],
    kept: &[usize],
    widths: &[usize],
    col_numeric: &[bool],
    style: &'static str,
    opts: &RenderOpts,
) {
    for (j, &i) in kept.iter().enumerate() {
        if j > 0 {
            out.push_str(&paint(opts, DIM, " │ "));
        }
        emit_cell(out, &cells[i], style, widths[i], col_numeric[i], opts);
    }
}

/// Truncate `plain` to `width`, color it, and pad to `width` (right- or
/// left-aligned). Width math uses the *plain* text so ANSI codes never throw it off.
fn emit_cell(
    out: &mut String,
    plain: &str,
    style: &'static str,
    width: usize,
    right: bool,
    opts: &RenderOpts,
) {
    let shown = truncate_str(plain, width);
    let pad = width.saturating_sub(dw(&shown));
    let colored = paint(opts, style, &shown);
    if right {
        out.push_str(&" ".repeat(pad));
        out.push_str(&colored);
    } else {
        out.push_str(&colored);
        out.push_str(&" ".repeat(pad));
    }
}

fn render_list(vals: &[Value], opts: &RenderOpts) -> String {
    let plain_parts: Vec<String> = vals.iter().map(|v| compact_plain(v, opts)).collect();
    let inline_w = 2 + plain_parts.iter().map(|p| dw(p)).sum::<usize>()
        + 2 * plain_parts.len().saturating_sub(1);
    let multiline = plain_parts.iter().any(|p| p.contains('\n'));

    if vals.len() <= opts.max_rows && inline_w <= opts.width && !multiline {
        let parts: Vec<String> =
            vals.iter().map(|v| render_compact(v, opts)).collect();
        return format!("{}{}{}", paint(opts, DIM, "["), parts.join(", "), paint(opts, DIM, "]"));
    }

    let n = vals.len();
    let (head, tail) = if n > opts.max_rows {
        let t = (opts.max_rows / 5).max(1);
        (opts.max_rows - t, t)
    } else {
        (n, 0)
    };
    let mut s = paint(opts, DIM, "[");
    s.push('\n');
    for v in vals.iter().take(head) {
        s.push_str("  ");
        s.push_str(&render_compact(v, opts));
        s.push_str(",\n");
    }
    if n > opts.max_rows {
        s.push_str("  ");
        s.push_str(&paint(opts, MISSING, &format!("… {} more", n - head - tail)));
        s.push('\n');
        for v in vals.iter().skip(n - tail) {
            s.push_str("  ");
            s.push_str(&render_compact(v, opts));
            s.push_str(",\n");
        }
    }
    s.push_str(&paint(opts, DIM, "]"));
    s
}

// ---- small helpers ----

/// Display width (column count). Approximates with `char` count — fine for the
/// ASCII-dominant data Helix prints; wide-CJK alignment is a deliberate non-goal.
fn dw(s: &str) -> usize {
    s.chars().count()
}

fn truncate_str(s: &str, max: usize) -> String {
    if dw(s) <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn opts(color: bool) -> RenderOpts {
        RenderOpts { rich: true, color, width: 80, max_rows: 20 }
    }

    fn rec(pairs: &[(&str, Value)]) -> Value {
        Value::Record(Rc::new(
            pairs.iter().map(|(k, v)| (Symbol::intern(k), v.clone())).collect(),
        ))
    }

    #[test]
    fn ints_get_thousands_separators() {
        assert_eq!(fmt_int(1_000_000), "1_000_000");
        assert_eq!(fmt_int(-12_345), "-12_345");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(i64::MIN), "-9_223_372_036_854_775_808");
    }

    #[test]
    fn floats_group_and_go_scientific() {
        assert_eq!(fmt_float_rich(1234.5), "1_234.5");
        assert_eq!(fmt_float_rich(2.0), "2.0");
        assert!(fmt_float_rich(1e20).contains('e'));
        assert!(fmt_float_rich(0.00001).contains('e'));
    }

    #[test]
    fn record_array_renders_as_a_table() {
        let vals = vec![
            rec(&[("gene", Value::Str(Rc::new("BRCA1".into()))), ("len", Value::Int(81188))]),
            rec(&[("gene", Value::Str(Rc::new("TP53".into()))), ("len", Value::Int(19149))]),
        ];
        let out = render_value_with(&Value::array(vals), &opts(false));
        // Header names present, a rule line, separators, and the formatted number.
        assert!(out.contains("gene"), "got:\n{out}");
        assert!(out.contains("len"), "got:\n{out}");
        assert!(out.contains("─"), "expected a rule line, got:\n{out}");
        assert!(out.contains('│'), "expected a column separator, got:\n{out}");
        assert!(out.contains("81_188"), "expected grouped number, got:\n{out}");
        // The number column is right-aligned: "81_188" is wider than "19_149"==
        // same width here; check both rows share the column width by line lengths.
        assert!(out.lines().count() >= 4); // header, rule, 2 rows
    }

    #[test]
    fn long_array_is_elided() {
        let vals: Vec<Value> = (0..100).map(Value::Int).collect();
        let out = render_value_with(&Value::array(vals), &opts(false));
        assert!(out.contains("more"), "expected an elision marker, got:\n{out}");
        // Far fewer than 100 lines.
        assert!(out.lines().count() < 40, "should be elided, got {} lines", out.lines().count());
    }

    #[test]
    fn color_wraps_leaves_in_ansi() {
        let out = render_value_with(&Value::Int(42), &opts(true));
        assert!(out.starts_with("\x1b[36m") && out.ends_with("\x1b[0m"), "got: {out:?}");
        // No color → no escapes.
        let plain = render_value_with(&Value::Int(42), &opts(false));
        assert_eq!(plain, "42");
    }

    #[test]
    fn short_scalar_array_is_inline() {
        let vals = vec![Value::Int(1), Value::Int(2), Value::Int(3)];
        let out = render_value_with(&Value::array(vals), &opts(false));
        assert_eq!(out, "[1, 2, 3]");
    }

    #[test]
    fn top_level_string_is_unquoted_nested_is_quoted() {
        let s = Value::Str(Rc::new("hi".into()));
        assert_eq!(render_value_with(&s, &opts(false)), "hi");
        let arr = Value::array(vec![s]);
        assert_eq!(render_value_with(&arr, &opts(false)), "[\"hi\"]");
    }
}

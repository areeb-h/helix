//! Output & export — writing files and serializing results to formats.
//!
//! Two families:
//!   * **writers** (`write_to`, `append_to`, `write_csv`, `write_json`,
//!     `write_fasta`, `write_fastq`) — effectful, return `Unit`, write to
//!     disk with buffered/streaming I/O.
//!   * **serializers** (`to_markdown`, `to_html`, `svg_bar`,
//!     `svg_line`) — pure, return a `Str` you can `print` or `write_to`.
//!
//! All of this is purely additive — no existing (hot) path is touched, so the
//! runtime benchmarks are unaffected. The writers themselves stay efficient:
//! whole-string writes go straight to the file, a DataFrame CSV streams through the
//! backend's `CsvWriter`, and the bio writers build into one buffer then write once.

use std::io::Write;
use std::rc::Rc;

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::{fmt_float, Value};

// ---------------------------------------------------------------------------
// writers (effectful)
// ---------------------------------------------------------------------------

/// `text.write_to(path)` — write text to a file, truncating it.
pub fn write_text(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let (path, text) = path_and_text("write_to", args, line, col)?;
    std::fs::write(path, text).map_err(|e| io_err("write", path, e, line, col))?;
    Ok(Value::Unit)
}

/// `text.append_to(path)` — append text to a file (creating it if absent).
pub fn append_text(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let (path, text) = path_and_text("append_to", args, line, col)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| io_err("open for append", path, e, line, col))?;
    f.write_all(text.as_bytes()).map_err(|e| io_err("append", path, e, line, col))?;
    Ok(Value::Unit)
}

/// `data.write_csv(path)` — a DataFrame (streamed via the backend) or an array
/// of records (serialized here) to comma-separated values.
pub fn write_csv(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    write_delimited("write_csv", b',', ',', args, line, col)
}

/// `data.write_tsv(path)` — tab-separated variant of [`write_csv`].
pub fn write_tsv(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    write_delimited("write_tsv", b'\t', '\t', args, line, col)
}

fn write_delimited(
    who: &str,
    sep_byte: u8,
    sep: char,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    if args.len() != 2 {
        return Err(HelixError::new(format!("`{who}` takes (data, path)"), line, col));
    }
    let path = as_path(who, &args[1], line, col)?;
    match &args[0] {
        // DataFrame: stream through the engine's CSV writer (no Value materialization).
        Value::DataFrame(df) => df.write_csv(path, sep_byte, line, col)?,
        // Array of records: serialize here.
        other => {
            let (headers, rows) = tabular(who, other, line, col)?;
            let text = delimited_string(&headers, &rows, sep);
            std::fs::write(path, text).map_err(|e| io_err("write", path, e, line, col))?;
        }
    }
    Ok(Value::Unit)
}

/// `value.write_json(path)` — serialize any value as JSON to a file.
pub fn write_json(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.len() != 2 {
        return Err(HelixError::new("`write_json` takes (value, path)", line, col));
    }
    let path = as_path("write_json", &args[1], line, col)?;
    let json = crate::json::stringify(&args[0]).map_err(|e| HelixError::new(e, line, col))?;
    std::fs::write(path, json).map_err(|e| io_err("write", path, e, line, col))?;
    Ok(Value::Unit)
}

/// `records.write_fasta(path)` — `{id|name, seq}` records to FASTA (70-col wrap).
pub fn write_fasta(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let (arr, path) = records_arg("write_fasta", args, line, col)?;
    let mut out = String::new();
    for v in arr.to_values().iter() {
        let rec = as_record("write_fasta", v, line, col)?;
        let id = field_str(rec, &["id", "name"]).unwrap_or_default();
        let seq = field_str(rec, &["seq", "sequence"]).ok_or_else(|| {
            HelixError::new("`write_fasta` needs a `seq` field on each record", line, col)
        })?;
        out.push('>');
        out.push_str(&id);
        out.push('\n');
        wrap_into(&mut out, &seq, 70);
    }
    std::fs::write(path, out).map_err(|e| io_err("write", path, e, line, col))?;
    Ok(Value::Unit)
}

/// `records.write_fastq(path)` — `{id|name, seq, qual}` records to FASTQ.
pub fn write_fastq(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let (arr, path) = records_arg("write_fastq", args, line, col)?;
    let mut out = String::new();
    for v in arr.to_values().iter() {
        let rec = as_record("write_fastq", v, line, col)?;
        let id = field_str(rec, &["id", "name"]).unwrap_or_default();
        let seq = field_str(rec, &["seq", "sequence"]).ok_or_else(|| {
            HelixError::new("`write_fastq` needs a `seq` field on each record", line, col)
        })?;
        let qual = field_str(rec, &["qual", "quality"]).ok_or_else(|| {
            HelixError::new("`write_fastq` needs a `qual` field on each record", line, col)
        })?;
        if qual.len() != seq.len() {
            return Err(HelixError::new(
                format!("FASTQ record `{id}`: seq is {} bases but qual is {}", seq.len(), qual.len()),
                line,
                col,
            ));
        }
        out.push('@');
        out.push_str(&id);
        out.push('\n');
        out.push_str(&seq);
        out.push_str("\n+\n");
        out.push_str(&qual);
        out.push('\n');
    }
    std::fs::write(path, out).map_err(|e| io_err("write", path, e, line, col))?;
    Ok(Value::Unit)
}

// ---------------------------------------------------------------------------
// serializers (pure → Str)
// ---------------------------------------------------------------------------

/// `data.to_markdown()` — a GitHub-flavored Markdown table.
pub fn to_markdown(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let v = one_arg("to_markdown", args, line, col)?;
    let (headers, rows) = tabular("to_markdown", v, line, col)?;
    let numeric: Vec<bool> = (0..headers.len()).map(|c| numeric_col(&rows, c)).collect();

    let mut s = String::new();
    s.push_str("| ");
    s.push_str(&headers.iter().map(|h| md_escape(h)).collect::<Vec<_>>().join(" | "));
    s.push_str(" |\n|");
    for n in &numeric {
        s.push_str(if *n { " ---: |" } else { " :--- |" });
    }
    for row in &rows {
        s.push_str("\n| ");
        let cells: Vec<String> = row.iter().map(|v| md_escape(&plain_cell(v))).collect();
        s.push_str(&cells.join(" | "));
        s.push_str(" |");
    }
    Ok(string(s))
}

/// `rows.to_table()` — a whitespace-aligned plain-text table for console output, so a
/// program prints a clean grid without hand-placing every column. Each column auto-sizes
/// to its widest cell (header included); numeric columns right-align, the rest left-align
/// (matching the `{x:N}` format-spec default). A `missing` shows as `·`. Accepts a
/// DataFrame or an array of records with matching fields.
pub fn to_table(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let v = one_arg("to_table", args, line, col)?;
    let (headers, rows) = tabular("to_table", v, line, col)?;
    let numeric: Vec<bool> = (0..headers.len()).map(|c| numeric_col(&rows, c)).collect();
    let cell_text = |v: &Value| -> String {
        if matches!(v, Value::Missing) { "·".to_string() } else { plain_cell(v) }
    };

    // Column widths = max Unicode-scalar count over the header and every cell.
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(cell_text(cell).chars().count());
        }
    }
    let pad = |text: &str, w: usize, right: bool| -> String {
        let fill = w.saturating_sub(text.chars().count());
        if right { format!("{}{text}", " ".repeat(fill)) } else { format!("{text}{}", " ".repeat(fill)) }
    };

    // Two-space gutter between columns; trailing pad on the last cell is trimmed so no
    // line carries dangling whitespace.
    let mut s = String::new();
    let hdr: Vec<String> =
        headers.iter().enumerate().map(|(c, h)| pad(h, widths[c], numeric[c])).collect();
    s.push_str(hdr.join("  ").trim_end());
    s.push('\n');
    let seps: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    s.push_str(&seps.join("  "));
    for row in &rows {
        s.push('\n');
        let cells: Vec<String> = (0..headers.len())
            .map(|c| pad(&row.get(c).map(&cell_text).unwrap_or_default(), widths[c], numeric[c]))
            .collect();
        s.push_str(cells.join("  ").trim_end());
    }
    Ok(string(s))
}

/// `data.to_html()` — a self-contained, styled HTML document with a table.
pub fn to_html(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let v = one_arg("to_html", args, line, col)?;
    let (headers, rows) = tabular("to_html", v, line, col)?;
    let numeric: Vec<bool> = (0..headers.len()).map(|c| numeric_col(&rows, c)).collect();

    let mut s = String::from(HTML_HEAD);
    s.push_str("<table>\n<thead><tr>");
    for (c, h) in headers.iter().enumerate() {
        let cls = if numeric[c] { " class=\"num\"" } else { "" };
        s.push_str(&format!("<th{cls}>{}</th>", html_escape(h)));
    }
    s.push_str("</tr></thead>\n<tbody>\n");
    for row in &rows {
        s.push_str("<tr>");
        for (c, cell) in row.iter().enumerate() {
            let cls = if matches!(cell, Value::Missing) {
                " class=\"missing\"".to_string()
            } else if numeric[c] {
                " class=\"num\"".to_string()
            } else {
                String::new()
            };
            let text = if matches!(cell, Value::Missing) {
                "·".to_string()
            } else {
                html_escape(&plain_cell(cell))
            };
            s.push_str(&format!("<td{cls}>{text}</td>"));
        }
        s.push_str("</tr>\n");
    }
    s.push_str("</tbody>\n</table>\n</body>\n</html>\n");
    Ok(string(s))
}

/// `value.to_json()` — serialize any value to a JSON string. A DataFrame is
/// materialized to an array of row records first (so `df.to_json()` round-trips
/// through `read_json`-style consumers); every other value goes straight through
/// `json::stringify`.
pub fn to_json(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let v = one_arg("to_json", args, line, col)?;
    let json = match v {
        Value::DataFrame(_) => {
            let (headers, rows) = tabular("to_json", v, line, col)?;
            let recs: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let fields: Vec<(Symbol, Value)> = headers
                        .iter()
                        .zip(r)
                        .map(|(h, val)| (Symbol::intern(h), val.clone()))
                        .collect();
                    Value::Record(Rc::new(fields))
                })
                .collect();
            crate::json::stringify(&Value::array(recs)).map_err(|e| HelixError::new(e, line, col))?
        }
        other => crate::json::stringify(other).map_err(|e| HelixError::new(e, line, col))?,
    };
    Ok(string(json))
}

/// `values.svg_bar(labels?)` — a clean horizontal-bar SVG chart.
pub fn svg_bar(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.is_empty() || args.len() > 2 {
        return Err(HelixError::new("`svg_bar` takes (values [, labels])", line, col));
    }
    let values = nums("svg_bar", &args[0], line, col)?;
    let labels = args.get(1).map(label_strs);
    Ok(string(render_svg_bar(&values, labels.as_deref())))
}

/// `values.svg_line()` — a clean line-plot SVG chart.
pub fn svg_line(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    let v = one_arg("svg_line", args, line, col)?;
    let values = nums("svg_line", v, line, col)?;
    Ok(string(render_svg_line(&values)))
}

// ---------------------------------------------------------------------------
// SVG rendering
// ---------------------------------------------------------------------------

const BAR_FILL: &str = "#4c9aff";
const LINE_STROKE: &str = "#4c9aff";
const AXIS: &str = "#9aa5b1";
const TEXT: &str = "#334155";

fn render_svg_bar(values: &[f64], labels: Option<&[String]>) -> String {
    if values.is_empty() {
        return svg_empty();
    }
    let maxv = values.iter().copied().fold(0.0_f64, f64::max).max(f64::MIN_POSITIVE);
    let (w, lm, rm, tm, bm) = (760.0, 120.0, 70.0, 24.0, 24.0);
    let (bh, gap) = (26.0, 10.0);
    let area = w - lm - rm;
    let h = tm + values.len() as f64 * (bh + gap) + bm;

    let mut s = svg_open(w, h);
    s.push_str(&format!(
        "<line x1=\"{lm}\" y1=\"{tm}\" x2=\"{lm}\" y2=\"{}\" stroke=\"{AXIS}\" stroke-width=\"1\"/>\n",
        tm + values.len() as f64 * (bh + gap)
    ));
    for (i, &v) in values.iter().enumerate() {
        let y = tm + i as f64 * (bh + gap);
        let bw = (v.max(0.0) / maxv) * area;
        s.push_str(&format!(
            "<rect x=\"{lm}\" y=\"{y:.1}\" width=\"{bw:.1}\" height=\"{bh}\" rx=\"3\" fill=\"{BAR_FILL}\"/>\n"
        ));
        if let Some(ls) = labels
            && let Some(lbl) = ls.get(i)
        {
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" class=\"lbl\">{}</text>\n",
                lm - 8.0,
                y + bh * 0.7,
                svg_escape(lbl)
            ));
        }
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" class=\"val\">{}</text>\n",
            lm + bw + 6.0,
            y + bh * 0.7,
            num_label(v)
        ));
    }
    s.push_str("</svg>\n");
    s
}

fn render_svg_line(values: &[f64]) -> String {
    if values.is_empty() {
        return svg_empty();
    }
    let (w, h, lm, rm, tm, bm) = (760.0, 380.0, 64.0, 24.0, 24.0, 36.0);
    let ymin = values.iter().copied().fold(f64::INFINITY, f64::min);
    let ymax = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let yspan = (ymax - ymin).max(f64::MIN_POSITIVE);
    let n = values.len();
    let px = |i: usize| -> f64 {
        if n <= 1 {
            lm
        } else {
            lm + (i as f64 / (n - 1) as f64) * (w - lm - rm)
        }
    };
    let py = |v: f64| -> f64 { tm + (1.0 - (v - ymin) / yspan) * (h - tm - bm) };

    let mut s = svg_open(w, h);
    // axes
    s.push_str(&format!(
        "<line x1=\"{lm}\" y1=\"{tm}\" x2=\"{lm}\" y2=\"{}\" stroke=\"{AXIS}\"/>\n",
        h - bm
    ));
    s.push_str(&format!(
        "<line x1=\"{lm}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{AXIS}\"/>\n",
        h - bm,
        w - rm,
        h - bm
    ));
    // y range labels
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" class=\"lbl\">{}</text>\n",
        lm - 6.0,
        tm + 8.0,
        num_label(ymax)
    ));
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" class=\"lbl\">{}</text>\n",
        lm - 6.0,
        h - bm,
        num_label(ymin)
    ));
    // polyline
    let pts: String =
        values.iter().enumerate().map(|(i, &v)| format!("{:.1},{:.1}", px(i), py(v))).collect::<Vec<_>>().join(" ");
    s.push_str(&format!(
        "<polyline points=\"{pts}\" fill=\"none\" stroke=\"{LINE_STROKE}\" stroke-width=\"2\" stroke-linejoin=\"round\"/>\n"
    ));
    s.push_str("</svg>\n");
    s
}

fn svg_open(w: f64, h: f64) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" font-family=\"-apple-system, Segoe UI, Roboto, sans-serif\">\n\
         <style>.lbl{{fill:{AXIS};font-size:13px}}.val{{fill:{TEXT};font-size:13px}}</style>\n\
         <rect width=\"{w}\" height=\"{h}\" fill=\"white\"/>\n"
    )
}

fn svg_empty() -> String {
    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"200\" height=\"40\"><text x=\"8\" y=\"24\">(no data)</text></svg>\n".to_string()
}

fn num_label(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9e18 {
        (x as i64).to_string()
    } else {
        fmt_float(x)
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

const HTML_HEAD: &str = "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
<style>\n\
:root{color-scheme:light dark}\n\
body{font-family:-apple-system,Segoe UI,Roboto,sans-serif;margin:2rem;color:#1f2933}\n\
table{border-collapse:collapse;font-size:14px;box-shadow:0 1px 3px rgba(0,0,0,.12);border-radius:8px;overflow:hidden}\n\
thead th{background:#3b82f6;color:#fff;font-weight:600;text-align:left;padding:.55rem .9rem}\n\
th.num,td.num{text-align:right;font-variant-numeric:tabular-nums}\n\
td{padding:.5rem .9rem;border-top:1px solid #e5e7eb}\n\
tbody tr:nth-child(even){background:#f7f9fc}\n\
tbody tr:hover{background:#eef4ff}\n\
td.missing{color:#9aa5b1;text-align:center}\n\
</style>\n</head>\n<body>\n";

fn string(s: String) -> Value {
    Value::Str(Rc::new(s))
}

fn one_arg<'a>(who: &str, args: &'a [Value], line: usize, col: usize) -> Result<&'a Value, HelixError> {
    match args {
        [v] => Ok(v),
        _ => Err(HelixError::new(format!("`{who}` takes one argument"), line, col)),
    }
}

fn path_and_text<'a>(
    who: &str,
    args: &'a [Value],
    line: usize,
    col: usize,
) -> Result<(&'a str, &'a str), HelixError> {
    if args.len() != 2 {
        return Err(HelixError::new(format!("`{who}` takes (path, text)"), line, col));
    }
    let path = as_path(who, &args[0], line, col)?;
    let text = match &args[1] {
        Value::Str(s) => s.as_str(),
        Value::Dna(s) => s.as_str(),
        other => return Err(type_err(who, "text (a string)", other, line, col)),
    };
    Ok((path, text))
}

fn as_path<'a>(who: &str, v: &'a Value, line: usize, col: usize) -> Result<&'a str, HelixError> {
    match v {
        Value::Str(s) => Ok(s.as_str()),
        other => Err(type_err(who, "a string path", other, line, col)),
    }
}

type ArrayRc = std::rc::Rc<crate::value::ArrayData>;

fn records_arg<'a>(
    who: &str,
    args: &'a [Value],
    line: usize,
    col: usize,
) -> Result<(&'a ArrayRc, &'a str), HelixError> {
    if args.len() != 2 {
        return Err(HelixError::new(format!("`{who}` takes (records, path)"), line, col));
    }
    let path = as_path(who, &args[1], line, col)?;
    match &args[0] {
        Value::Array(a) => Ok((a, path)),
        other => Err(type_err(who, "an array of records", other, line, col)),
    }
}

fn as_record<'a>(
    who: &str,
    v: &'a Value,
    line: usize,
    col: usize,
) -> Result<&'a [(Symbol, Value)], HelixError> {
    match v {
        Value::Record(f) => Ok(f),
        other => Err(type_err(who, "an array of records", other, line, col)),
    }
}

/// Extract `(headers, rows)` from a DataFrame (full materialization through the
/// seam) or a homogeneous array of records.
fn tabular(
    who: &str,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<(Vec<String>, Vec<Vec<Value>>), HelixError> {
    match v {
        Value::DataFrame(df) => {
            let names = df.column_names(line, col)?;
            let cols: Vec<Vec<Value>> =
                names.iter().map(|n| df.column_values(n, line, col)).collect::<Result<_, _>>()?;
            let nrows = cols.first().map(|c| c.len()).unwrap_or(0);
            let rows = (0..nrows)
                .map(|r| cols.iter().map(|c| c[r].clone()).collect())
                .collect();
            Ok((names, rows))
        }
        Value::Array(a) => records_table(&a.to_values()).ok_or_else(|| {
            HelixError::new(
                format!("`{who}` expects a DataFrame or an array of records with matching fields"),
                line,
                col,
            )
        }),
        other => Err(type_err(who, "a DataFrame or an array of records", other, line, col)),
    }
}

fn records_table(vals: &[Value]) -> Option<(Vec<String>, Vec<Vec<Value>>)> {
    let first = match vals.first() {
        Some(Value::Record(f)) if !f.is_empty() => f,
        _ => return None,
    };
    let keys: Vec<Symbol> = first.iter().map(|(k, _)| *k).collect();
    let mut rows = Vec::with_capacity(vals.len());
    for v in vals {
        let Value::Record(f) = v else { return None };
        if f.len() != keys.len() || f.iter().zip(&keys).any(|((k, _), w)| k != w) {
            return None;
        }
        rows.push(f.iter().map(|(_, val)| val.clone()).collect());
    }
    Some((keys.iter().map(|k| k.as_str().to_string()).collect(), rows))
}

fn numeric_col(rows: &[Vec<Value>], c: usize) -> bool {
    rows.iter()
        .all(|r| matches!(r.get(c), None | Some(Value::Int(_) | Value::Float(_) | Value::Missing)))
}

/// A *machine-readable* cell value — plain (no thousands separators, no color).
fn plain_cell(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => fmt_float(*f),
        Value::Str(s) => (**s).clone(),
        Value::Dna(s) => (**s).clone(),
        Value::Bool(b) => b.to_string(),
        Value::Missing => String::new(),
        other => other.to_string(),
    }
}

fn delimited_string(headers: &[String], rows: &[Vec<Value>], sep: char) -> String {
    let mut s = String::new();
    push_delimited_row(&mut s, headers.iter().map(|h| h.as_str().to_string()), sep);
    for row in rows {
        s.push('\n');
        push_delimited_row(&mut s, row.iter().map(plain_cell), sep);
    }
    s.push('\n');
    s
}

fn push_delimited_row(s: &mut String, fields: impl Iterator<Item = String>, sep: char) {
    let mut first = true;
    for f in fields {
        if !first {
            s.push(sep);
        }
        first = false;
        s.push_str(&csv_quote(&f, sep));
    }
}

/// RFC 4180 quoting: wrap in `"..."` and double internal quotes when a field holds
/// the separator, a quote, or a newline.
fn csv_quote(field: &str, sep: char) -> String {
    if field.contains(sep) || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

fn field_str(rec: &[(Symbol, Value)], names: &[&str]) -> Option<String> {
    for name in names {
        if let Some((_, v)) = rec.iter().find(|(k, _)| k.as_str() == *name) {
            return Some(match v {
                Value::Str(s) => (**s).clone(),
                Value::Dna(s) => (**s).clone(),
                Value::Missing => return None,
                other => other.to_string(),
            });
        }
    }
    None
}

/// Append `seq` to `out` wrapped at `width` columns, each line newline-terminated.
fn wrap_into(out: &mut String, seq: &str, width: usize) {
    let bytes = seq.as_bytes();
    if bytes.is_empty() {
        out.push('\n');
        return;
    }
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + width).min(bytes.len());
        // Sequences are ASCII, so byte slicing is char-safe.
        out.push_str(&seq[i..end]);
        out.push('\n');
        i = end;
    }
}

fn nums(who: &str, v: &Value, line: usize, col: usize) -> Result<Vec<f64>, HelixError> {
    let arr = match v {
        Value::Array(a) => a,
        other => return Err(type_err(who, "an array of numbers", other, line, col)),
    };
    let mut out = Vec::new();
    for x in arr.to_values().iter() {
        match x {
            Value::Int(i) => out.push(*i as f64),
            Value::Float(f) => out.push(*f),
            other => {
                return Err(HelixError::new(
                    format!("`{who}` needs numbers, but the array holds a {}", other.type_name()),
                    line,
                    col,
                ))
            }
        }
    }
    Ok(out)
}

fn label_strs(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a
            .to_values()
            .iter()
            .map(|x| match x {
                Value::Str(s) => (**s).clone(),
                Value::Dna(s) => (**s).clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn svg_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn io_err(action: &str, path: &str, e: std::io::Error, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("could not {action} `{path}`: {e}"), line, col)
}

fn type_err(who: &str, expected: &str, got: &Value, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!("`{who}` expects {expected}, but got a {}", got.type_name()),
        line,
        col,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pairs: &[(&str, Value)]) -> Value {
        Value::Record(Rc::new(pairs.iter().map(|(k, v)| (Symbol::intern(k), v.clone())).collect()))
    }
    fn text(v: Value) -> String {
        match v {
            Value::Str(s) => (*s).clone(),
            _ => panic!("expected a string"),
        }
    }
    fn table() -> Value {
        Value::array(vec![
            rec(&[("gene", Value::Str(Rc::new("BRCA1".into()))), ("len", Value::Int(81188))]),
            rec(&[("gene", Value::Str(Rc::new("TP53".into()))), ("len", Value::Int(19149))]),
        ])
    }

    #[test]
    fn markdown_has_aligned_columns() {
        let md = text(to_markdown(&[table()], 0, 0).unwrap());
        assert!(md.contains("| gene | len |"), "got:\n{md}");
        assert!(md.contains("---:"), "numeric column right-aligned:\n{md}");
        assert!(md.contains("BRCA1"));
        // Numbers are plain (machine-readable), not grouped with `_`.
        assert!(md.contains("81188") && !md.contains("81_188"));
    }

    #[test]
    fn html_is_a_self_contained_document() {
        let html = text(to_html(&[table()], 0, 0).unwrap());
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<table>") && html.contains("BRCA1"));
        assert!(html.contains("class=\"num\""), "numeric cells flagged:\n{html}");
    }

    #[test]
    fn csv_quotes_fields_with_separators() {
        assert_eq!(csv_quote("plain", ','), "plain");
        assert_eq!(csv_quote("a,b", ','), "\"a,b\"");
        assert_eq!(csv_quote("she said \"hi\"", ','), "\"she said \"\"hi\"\"\"");
    }

    #[test]
    fn delimited_string_round_trips_headers_and_rows() {
        let (h, r) = tabular("t", &table(), 0, 0).unwrap();
        let csv = delimited_string(&h, &r, ',');
        assert_eq!(csv, "gene,len\nBRCA1,81188\nTP53,19149\n");
    }

    #[test]
    fn fasta_wrapping_breaks_long_sequences() {
        let mut s = String::new();
        wrap_into(&mut s, &"A".repeat(150), 70);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), 70);
        assert_eq!(lines[2].len(), 10);
    }

    #[test]
    fn svg_bar_is_valid_svg_with_rects() {
        let svg = text(svg_bar(&[Value::array(vec![Value::Int(3), Value::Int(8)])], 0, 0).unwrap());
        assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
        assert!(svg.matches("<rect").count() >= 2);
    }
}

//! Terminal charts — `chart.bar`, `chart.hist`, `chart.line`, `chart.scatter`,
//! `chart.sparkline`. Each returns a `Str` value (so it composes with `print`,
//! interpolation, records), rendered with block/braille glyphs and colored through
//! the shared [`crate::render`] theme when stdout is a terminal (plain when piped,
//! so charts in tests/scripts are deterministic ASCII-ish text).
//!
//! These delegate *rendering*, not statistics: binning and scaling are done here,
//! but coloring/width all come from `render::RenderOpts::auto()`, so a chart honors
//! `HELIX_THEME`, `NO_COLOR`, and the terminal width exactly like a table does.

use std::rc::Rc;

use crate::error::HelixError;
use crate::render::{
    self, dw, paint_axis, paint_bar, paint_dim, paint_header, paint_num, RenderOpts,
};
use crate::value::Value;

/// `chart.bar(values [, labels])` — a horizontal bar chart.
pub fn bar(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.is_empty() || args.len() > 2 {
        return Err(arity("chart.bar", "values and optional labels", line, col));
    }
    let values = nums(&args[0], "chart.bar", line, col)?;
    let labels = match args.get(1) {
        Some(v) => Some(strs(v, "chart.bar", line, col)?),
        None => None,
    };
    if let Some(ls) = &labels
        && ls.len() != values.len()
    {
        return Err(HelixError::new(
            format!("`chart.bar` got {} values but {} labels", values.len(), ls.len()),
            line,
            col,
        ));
    }
    let opts = RenderOpts::auto();
    Ok(string(bar_chart(&values, labels.as_deref(), None, &opts)))
}

/// `chart.hist(values [, bins])` — a histogram of a numeric array.
pub fn hist(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.is_empty() || args.len() > 2 {
        return Err(arity("chart.hist", "values and an optional bin count", line, col));
    }
    let values = nums(&args[0], "chart.hist", line, col)?;
    if values.is_empty() {
        return Ok(string("(no data)".to_string()));
    }
    let bins = match args.get(1) {
        Some(Value::Int(b)) if *b >= 1 && *b <= 200 => *b as usize,
        Some(Value::Int(_)) => return Err(HelixError::new("`chart.hist` bins must be 1..=200", line, col)),
        Some(other) => return Err(type_err("chart.hist", "an integer bin count", other, line, col)),
        None => default_bins(values.len()),
    };
    let (counts, labels) = histogram_bins(&values, bins);
    let opts = RenderOpts::auto();
    let counts_f: Vec<f64> = counts.iter().map(|&c| c as f64).collect();
    Ok(string(bar_chart(&counts_f, Some(&labels), Some("count"), &opts)))
}

/// `chart.sparkline(values)` — a one-line inline sparkline.
pub fn sparkline(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.len() != 1 {
        return Err(arity("chart.sparkline", "a single numeric array", line, col));
    }
    let values = nums(&args[0], "chart.sparkline", line, col)?;
    let opts = RenderOpts::auto();
    Ok(string(spark(&values, &opts)))
}

/// `chart.line(values)` — a braille line plot of a single series (index on x).
pub fn line(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.len() != 1 {
        return Err(arity("chart.line", "a single numeric array", line, col));
    }
    let ys = nums(&args[0], "chart.line", line, col)?;
    let xs: Vec<f64> = (0..ys.len()).map(|i| i as f64).collect();
    let opts = RenderOpts::auto();
    Ok(string(plot(&xs, &ys, true, &opts)))
}

/// `chart.scatter(xs, ys)` — a braille scatter plot of paired coordinates.
pub fn scatter(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    if args.len() != 2 {
        return Err(arity("chart.scatter", "x and y numeric arrays", line, col));
    }
    let xs = nums(&args[0], "chart.scatter", line, col)?;
    let ys = nums(&args[1], "chart.scatter", line, col)?;
    if xs.len() != ys.len() {
        return Err(HelixError::new(
            format!("`chart.scatter` got {} x-values but {} y-values", xs.len(), ys.len()),
            line,
            col,
        ));
    }
    let opts = RenderOpts::auto();
    Ok(string(plot(&xs, &ys, false, &opts)))
}

// ---- bar / histogram rendering ----

const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// Render a horizontal bar chart. `unit_label`, when set, heads the value column.
fn bar_chart(values: &[f64], labels: Option<&[String]>, unit_label: Option<&str>, opts: &RenderOpts) -> String {
    if values.is_empty() {
        return "(no data)".to_string();
    }
    let maxv = values.iter().copied().fold(0.0_f64, f64::max);
    let valstrs: Vec<String> = values.iter().map(|v| fmt_num(*v)).collect();
    let labelw = labels
        .map(|ls| ls.iter().map(|s| dw(s)).max().unwrap_or(0))
        .unwrap_or(0)
        .min(24);
    let valw = valstrs.iter().map(|s| dw(s)).max().unwrap_or(0);
    // Chrome = label + " │ " + bar + " " + value.
    let chrome = labelw + 3 + 1 + valw;
    let barw = opts.width.saturating_sub(chrome).clamp(8, 120);

    let mut out = String::new();
    if let Some(u) = unit_label {
        out.push_str(&paint_header(opts, &format!("{:>labelw$}   {}", "", u)));
        out.push('\n');
    }
    for (i, &v) in values.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if labelw > 0 {
            let lbl = labels.map(|ls| ls[i].as_str()).unwrap_or("");
            let lbl = ellipsize(lbl, labelw);
            out.push_str(&paint_axis(opts, &format!("{lbl:>labelw$}")));
            out.push(' ');
        }
        out.push_str(&paint_dim(opts, "│"));
        out.push(' ');
        out.push_str(&paint_bar(opts, &bar_glyphs(v, maxv, barw)));
        // pad to bar width so values line up
        let used = bar_cells(v, maxv, barw);
        out.push_str(&" ".repeat(barw.saturating_sub(used)));
        out.push(' ');
        out.push_str(&paint_num(opts, &valstrs[i]));
    }
    out
}

/// Number of (possibly fractional) cells a bar occupies, rounded up for padding.
fn bar_cells(v: f64, maxv: f64, barw: usize) -> usize {
    if maxv <= 0.0 || v <= 0.0 {
        return 0;
    }
    ((v / maxv) * barw as f64).ceil() as usize
}

fn bar_glyphs(v: f64, maxv: f64, barw: usize) -> String {
    if maxv <= 0.0 || v <= 0.0 {
        return String::new();
    }
    let units = (v / maxv) * barw as f64;
    let mut full = units.floor() as usize;
    let mut rem = ((units - full as f64) * 8.0).round() as usize;
    if rem == 8 {
        full += 1;
        rem = 0;
    }
    full = full.min(barw);
    let mut s = "█".repeat(full);
    if rem > 0 && full < barw {
        s.push(EIGHTHS[rem]);
    }
    s
}

fn default_bins(n: usize) -> usize {
    ((n as f64).sqrt().ceil() as usize).clamp(5, 20)
}

/// Bin `values` into `bins` equal-width buckets; return counts + range labels.
fn histogram_bins(values: &[f64], bins: usize) -> (Vec<usize>, Vec<String>) {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(f64::MIN_POSITIVE);
    let width = span / bins as f64;
    let mut counts = vec![0usize; bins];
    for &v in values {
        let mut idx = ((v - min) / width).floor() as usize;
        idx = idx.min(bins - 1);
        counts[idx] += 1;
    }
    let labels = (0..bins)
        .map(|i| {
            let lo = min + i as f64 * width;
            let hi = lo + width;
            format!("{}–{}", fmt_num(lo), fmt_num(hi))
        })
        .collect();
    (counts, labels)
}

// ---- sparkline ----

const SPARKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn spark(values: &[f64], opts: &RenderOpts) -> String {
    if values.is_empty() {
        return String::new();
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    let s: String = values
        .iter()
        .map(|&v| {
            let t = if span > 0.0 { (v - min) / span } else { 0.0 };
            SPARKS[((t * 7.0).round() as usize).min(7)]
        })
        .collect();
    paint_bar(opts, &s)
}

// ---- braille line / scatter plots ----

/// A braille drawing surface: `w`×`h` cells, each a 2×4 grid of dots, giving a
/// `2w`×`4h` virtual pixel canvas.
struct Braille {
    w: usize,
    h: usize,
    dots: Vec<u8>,
}

// Dot → bit mask: columns ×2, rows ×4 (Unicode braille layout).
const DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

impl Braille {
    fn new(w: usize, h: usize) -> Self {
        Braille { w, h, dots: vec![0; w * h] }
    }

    fn set(&mut self, px: i32, py: i32) {
        if px < 0 || py < 0 {
            return;
        }
        let (px, py) = (px as usize, py as usize);
        let (col, row) = (px / 2, py / 4);
        if col < self.w && row < self.h {
            self.dots[row * self.w + col] |= DOTS[py % 4][px % 2];
        }
    }

    /// Bresenham line between two virtual pixels.
    fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y) = (x0, y0);
        let mut err = dx + dy;
        loop {
            self.set(x, y);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    fn rows(&self) -> Vec<String> {
        (0..self.h)
            .map(|r| {
                (0..self.w)
                    .map(|c| char::from_u32(0x2800 + self.dots[r * self.w + c] as u32).unwrap())
                    .collect()
            })
            .collect()
    }
}

fn plot(xs: &[f64], ys: &[f64], connect: bool, opts: &RenderOpts) -> String {
    if xs.is_empty() {
        return "(no data)".to_string();
    }
    let ymin = ys.iter().copied().fold(f64::INFINITY, f64::min);
    let ymax = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let xmin = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let xmax = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    // y-axis gutter sized to the longest of the two edge labels.
    let (lo_lbl, hi_lbl) = (fmt_num(ymin), fmt_num(ymax));
    let gutter = dw(&lo_lbl).max(dw(&hi_lbl)).max(dw(&fmt_num((ymin + ymax) / 2.0)));
    let h = 15usize;
    let w = opts.width.saturating_sub(gutter + 2).clamp(12, 90);

    let mut canvas = Braille::new(w, h);
    let px = |x: f64| -> i32 {
        let t = if xmax > xmin { (x - xmin) / (xmax - xmin) } else { 0.5 };
        (t * (2 * w - 1) as f64).round() as i32
    };
    let py = |y: f64| -> i32 {
        let t = if ymax > ymin { (y - ymin) / (ymax - ymin) } else { 0.5 };
        ((1.0 - t) * (4 * h - 1) as f64).round() as i32
    };

    if connect {
        for pair in xs.iter().zip(ys).collect::<Vec<_>>().windows(2) {
            let (a, b) = (pair[0], pair[1]);
            canvas.line(px(*a.0), py(*a.1), px(*b.0), py(*b.1));
        }
        if xs.len() == 1 {
            canvas.set(px(xs[0]), py(ys[0]));
        }
    } else {
        for (x, y) in xs.iter().zip(ys) {
            canvas.set(px(*x), py(*y));
        }
    }

    // Compose: y labels on the left, a vertical axis, then the braille rows; a
    // bottom axis with the x range.
    let rows = canvas.rows();
    let mut out = String::new();
    for (r, row) in rows.iter().enumerate() {
        let label = if r == 0 {
            hi_lbl.clone()
        } else if r + 1 == h {
            lo_lbl.clone()
        } else {
            String::new()
        };
        out.push_str(&paint_axis(opts, &format!("{label:>gutter$} ")));
        out.push_str(&paint_dim(opts, "│"));
        out.push_str(&paint_bar(opts, row));
        out.push('\n');
    }
    // Bottom axis.
    out.push_str(&paint_axis(opts, &format!("{:>gutter$} ", "")));
    out.push_str(&paint_dim(opts, &format!("╰{}", "─".repeat(w))));
    out.push('\n');
    let xlo = fmt_num(xmin);
    let xhi = fmt_num(xmax);
    let span = w.saturating_sub(dw(&xlo) + dw(&xhi)).max(1);
    out.push_str(&paint_axis(
        opts,
        &format!("{:>gutter$}  {}{}{}", "", xlo, " ".repeat(span), xhi),
    ));
    out
}

// ---- shared helpers ----

fn fmt_num(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9e18 {
        render::fmt_int(x as i64)
    } else {
        render::fmt_float_rich(x)
    }
}

fn ellipsize(s: &str, max: usize) -> String {
    if dw(s) <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn string(s: String) -> Value {
    Value::Str(Rc::new(s))
}

/// Pull a numeric `Vec<f64>` from an array argument; reject non-numeric elements.
fn nums(v: &Value, who: &str, line: usize, col: usize) -> Result<Vec<f64>, HelixError> {
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
                )
                .hint("drop missing/strings first, e.g. `xs.drop_missing()`."))
            }
        }
    }
    Ok(out)
}

fn strs(v: &Value, who: &str, line: usize, col: usize) -> Result<Vec<String>, HelixError> {
    let arr = match v {
        Value::Array(a) => a,
        other => return Err(type_err(who, "an array of labels", other, line, col)),
    };
    Ok(arr
        .to_values()
        .iter()
        .map(|x| match x {
            Value::Str(s) => (**s).clone(),
            Value::Dna(s) => (**s).clone(),
            other => other.to_string(),
        })
        .collect())
}

fn arity(who: &str, expected: &str, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("`{who}` expects {expected}"), line, col)
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

    fn arr(xs: &[i64]) -> Value {
        Value::array(xs.iter().map(|&i| Value::Int(i)).collect())
    }
    fn text(v: Value) -> String {
        match v {
            Value::Str(s) => (*s).clone(),
            _ => panic!("expected a string"),
        }
    }

    #[test]
    fn bar_draws_blocks_and_values() {
        let out = text(bar(&[arr(&[3, 1, 4, 1, 5])], 0, 0).unwrap());
        assert!(out.contains('█'), "expected bar glyphs:\n{out}");
        assert!(out.contains('5'), "expected the max value labeled:\n{out}");
        assert_eq!(out.lines().count(), 5);
    }

    #[test]
    fn bar_with_labels_aligns() {
        let labels = Value::array(vec![
            Value::Str(Rc::new("a".into())),
            Value::Str(Rc::new("bb".into())),
        ]);
        let out = text(bar(&[arr(&[10, 20]), labels], 0, 0).unwrap());
        assert!(out.contains('│'), "expected an axis:\n{out}");
        assert!(out.contains("bb"));
    }

    #[test]
    fn histogram_has_a_count_header_and_bins() {
        let data: Vec<Value> = (0..100).map(|i| Value::Int(i % 10)).collect();
        let out = text(hist(&[Value::array(data), Value::Int(5)], 0, 0).unwrap());
        assert!(out.contains("count"), "expected a count header:\n{out}");
        assert!(out.contains('–'), "expected range labels:\n{out}");
    }

    #[test]
    fn sparkline_is_one_line_of_blocks() {
        let out = text(sparkline(&[arr(&[1, 5, 2, 8, 3])], 0, 0).unwrap());
        assert!(!out.contains('\n'), "sparkline is inline:\n{out}");
        assert_eq!(dw(&out), 5);
        assert!(out.chars().all(|c| SPARKS.contains(&c)));
    }

    #[test]
    fn line_plot_has_axes_and_braille() {
        let out = text(line(&[arr(&[1, 3, 2, 5, 4, 6])], 0, 0).unwrap());
        assert!(out.contains('╰'), "expected a bottom axis:\n{out}");
        assert!(out.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)), "braille:\n{out}");
    }

    #[test]
    fn non_numeric_array_is_a_clean_error() {
        let bad = Value::array(vec![Value::Str(Rc::new("x".into()))]);
        assert!(bar(&[bad], 0, 0).is_err());
    }
}

//! Rich terminal rendering for `print`, the REPL echo, and DataFrames.
//!
//! Output *is* the product for a data/bio language, so Helix renders values far
//! better than a flat `print`: a homogeneous array of records (or a DataFrame)
//! becomes a bordered, color-coded table, numbers carry thousands separators, long
//! collections are elided to a head/tail with a count, and leaves are color-coded.
//!
//! The whole rich layer is gated on **stdout being a terminal** (overridable via
//! `HELIX_RICH`), so piped/redirected output is **byte-identical** to the plain
//! [`crate::value::display_value`] path — scripts stay scriptable and every test
//! and `vmparity` snapshot stays stable. Color is additionally gated on `NO_COLOR`
//! and `HELIX_COLOR`. The palette ([`Theme`]) and table frame ([`BoxStyle`]) are
//! customizable via `HELIX_THEME` / `HELIX_BOX`, and the glyphs a chart draws with
//! via `HELIX_PLOT`. None of this is on a hot path.

use std::io::IsTerminal;

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::{display_value, fmt_float, Value};

/// Longest a single table cell renders before it is truncated with `…`.
const CELL_MAX: usize = 40;

/// A semantic role a value plays when rendered, mapped to a color by the [`Theme`].
#[derive(Clone, Copy)]
pub(crate) enum Role {
    Num,
    Str,
    Dna,
    Bool,
    Missing,
    Key,
    Header,
    Dim,
    Bar,
    Axis,
}

/// A color palette: an ANSI SGR parameter string per [`Role`] (empty = no color).
/// Selected by name via `HELIX_THEME`.
#[derive(Clone)]
pub struct Theme {
    num: &'static str,
    string: &'static str,
    dna: &'static str,
    boolean: &'static str,
    missing: &'static str,
    key: &'static str,
    header: &'static str,
    dim: &'static str,
    bar: &'static str,
    axis: &'static str,
}

impl Theme {
    fn code(&self, role: Role) -> &'static str {
        match role {
            Role::Num => self.num,
            Role::Str => self.string,
            Role::Dna => self.dna,
            Role::Bool => self.boolean,
            Role::Missing => self.missing,
            Role::Key => self.key,
            Role::Header => self.header,
            Role::Dim => self.dim,
            Role::Bar => self.bar,
            Role::Axis => self.axis,
        }
    }

    /// Resolve a theme by name; unknown names fall back to the default palette.
    pub fn named(name: &str) -> Theme {
        match name {
            "vivid" => Theme {
                num: "96",
                string: "92",
                dna: "93",
                boolean: "95",
                missing: "90",
                key: "94",
                header: "1;97",
                dim: "90",
                bar: "96",
                axis: "90",
            },
            "ocean" => Theme {
                num: "36",
                string: "38;5;38",
                dna: "38;5;43",
                boolean: "38;5;75",
                missing: "90",
                key: "38;5;39",
                header: "1;38;5;45",
                dim: "90",
                bar: "38;5;38",
                axis: "38;5;239",
            },
            "warm" => Theme {
                num: "33",
                string: "38;5;215",
                dna: "38;5;179",
                boolean: "35",
                missing: "90",
                key: "38;5;209",
                header: "1;38;5;208",
                dim: "90",
                bar: "38;5;208",
                axis: "90",
            },
            // Structure-only: bold headers and dim chrome, but no hue — for users
            // who want the layout without the colors.
            "mono" => Theme {
                num: "",
                string: "",
                dna: "",
                boolean: "",
                missing: "2",
                key: "1",
                header: "1",
                dim: "2",
                bar: "",
                axis: "2",
            },
            _ => Theme {
                num: "36",     // cyan
                string: "32",  // green
                dna: "33",     // yellow
                boolean: "35", // magenta
                missing: "90", // gray
                key: "34",     // blue
                header: "1",   // bold
                dim: "2",      // dim
                bar: "32",     // green
                axis: "2",     // dim
            },
        }
    }
}

/// The box-drawing characters that frame a table, selected via `HELIX_BOX`
/// (`rounded` default, `square`, `ascii`, or `none` for a borderless layout).
#[derive(Clone, Copy, PartialEq)]
pub struct BoxStyle {
    tl: char,
    tr: char,
    bl: char,
    br: char,
    h: char,
    v: char,
    cross: char,
    t_down: char,
    t_up: char,
    t_left: char,
    t_right: char,
    /// `none` draws no outer frame (header + rule + rows only).
    bordered: bool,
}

impl BoxStyle {
    pub fn named(name: &str) -> BoxStyle {
        match name {
            "square" => BoxStyle {
                tl: '┌', tr: '┐', bl: '└', br: '┘', h: '─', v: '│', cross: '┼',
                t_down: '┬', t_up: '┴', t_left: '┤', t_right: '├', bordered: true,
            },
            "ascii" => BoxStyle {
                tl: '+', tr: '+', bl: '+', br: '+', h: '-', v: '|', cross: '+',
                t_down: '+', t_up: '+', t_left: '+', t_right: '+', bordered: true,
            },
            "none" => BoxStyle {
                tl: ' ', tr: ' ', bl: ' ', br: ' ', h: '─', v: '│', cross: '┼',
                t_down: '─', t_up: '─', t_left: '─', t_right: '─', bordered: false,
            },
            _ => BoxStyle {
                tl: '╭', tr: '╮', bl: '╰', br: '╯', h: '─', v: '│', cross: '┼',
                t_down: '┬', t_up: '┴', t_left: '┤', t_right: '├', bordered: true,
            },
        }
    }
}

/// How rendering should behave for one `print`. Built by [`RenderOpts::auto`] from
/// the environment; constructed explicitly in tests to exercise the rich path off
/// a real terminal.
#[derive(Clone)]
pub struct RenderOpts {
    pub(crate) rich: bool,
    pub(crate) color: bool,
    pub(crate) width: usize,
    pub(crate) max_rows: usize,
    pub(crate) theme: Theme,
    pub(crate) boxes: BoxStyle,
    pub(crate) plot: crate::chart::PlotGlyphs,
}

impl RenderOpts {
    /// The environment as this process sees it, detected ONCE.
    ///
    /// This used to run per `print`: an `isatty`, a `TIOCGWINSZ` ioctl, six `env::var`
    /// lookups and three string matches, for every line a program ever wrote. Measured on
    /// a 200k-line program at 1.20 us of overhead per call, against 0.01 s for the same
    /// 200k iterations doing no I/O at all.
    ///
    /// Detecting once is safe because nothing can change the answer under a running
    /// program: no Helix builtin sets an environment variable, and the only `env::set_var`
    /// in the tree writes `HELIX_CACHE` from the package manager, which no render path
    /// reads. A test that needs different options constructs `RenderOpts` directly, which
    /// is what the field docs already say it is for.
    pub fn auto() -> Self {
        static BASE: std::sync::OnceLock<RenderOpts> = std::sync::OnceLock::new();
        let base = BASE.get_or_init(Self::detect);
        if base.rich {
            // Width is the one answer that CAN change under a running program -- a
            // terminal gets resized -- and asking costs an ioctl. Rich output is
            // human-paced, so that is affordable. The piped path, which is where a
            // program writes a million lines, never asks.
            RenderOpts { width: term_width(), ..base.clone() }
        } else {
            base.clone()
        }
    }

    /// Detect from the environment: rich only on a TTY (or `HELIX_RICH=1`), color
    /// only when rich and not suppressed by `NO_COLOR`/`HELIX_COLOR=never`.
    fn detect() -> Self {
        let tty = std::io::stdout().is_terminal();
        let rich = match std::env::var("HELIX_RICH").ok().as_deref() {
            Some("1") | Some("always") => true,
            Some("0") | Some("never") => false,
            _ => tty,
        };
        let color = rich
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("HELIX_COLOR").ok().as_deref() != Some("never");
        let theme = Theme::named(std::env::var("HELIX_THEME").as_deref().unwrap_or("default"));
        let boxes = BoxStyle::named(std::env::var("HELIX_BOX").as_deref().unwrap_or("rounded"));
        let plot = crate::chart::PlotGlyphs::named(
            std::env::var("HELIX_PLOT").as_deref().unwrap_or("braille"),
        );
        RenderOpts { rich, color, width: term_width(), max_rows: 20, theme, boxes, plot }
    }

    fn as_plain(&self) -> Self {
        RenderOpts { color: false, ..self.clone() }
    }

    /// True when this terminal was told it cannot draw beyond ASCII (`HELIX_BOX=ascii`).
    ///
    /// A report reuses the table's answer rather than asking its own question: a terminal
    /// whose font cannot draw `─` cannot draw `·` either, and one flag the user already
    /// knows about beats a second one they would have to discover.
    pub(crate) fn ascii_only(&self) -> bool {
        self.boxes.h == '-'
    }

    /// A horizontal rule `n` columns wide, in the active box style.
    pub(crate) fn rule(&self, n: usize) -> String {
        self.boxes.h.to_string().repeat(n)
    }

    /// Color `s` for `role` when color is on; otherwise return it unchanged.
    pub(crate) fn paint(&self, role: Role, s: &str) -> String {
        let code = self.theme.code(role);
        if self.color && !code.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
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
        // ONE argument is the overwhelmingly common call, and `join` on a one-element Vec
        // allocates a second String purely to copy the first into it. Hand the rendered
        // value straight back: identical bytes, one allocation instead of three.
        if let [v] = args {
            return display_value(v, line, col);
        }
        let mut parts = Vec::with_capacity(args.len());
        for v in args {
            parts.push(display_value(v, line, col)?);
        }
        return Ok(parts.join(" "));
    }
    let mut parts = Vec::with_capacity(args.len());
    for v in args {
        match v {
            Value::DataFrame(df) => parts.push(render_dataframe(df, &opts, line, col)?),
            _ => parts.push(render_value_with(v, &opts)),
        }
    }
    Ok(if parts.iter().any(|p| p.contains('\n')) {
        parts.join("\n")
    } else {
        parts.join(" ")
    })
}

/// Render a single value for the REPL's auto-echo. Never errors (a failed DataFrame
/// query falls back to plain `Display`), so the notebook echo can't blow up.
pub fn render_echo(v: &Value) -> String {
    let opts = RenderOpts::auto();
    if !opts.rich {
        return v.to_string();
    }
    match v {
        Value::DataFrame(df) => render_dataframe(df, &opts, 0, 0).unwrap_or_else(|_| v.to_string()),
        _ => render_value_with(v, &opts),
    }
}

/// The rich entry point for a top-level (non-DataFrame) value.
pub(crate) fn render_value_with(v: &Value, opts: &RenderOpts) -> String {
    match v {
        Value::Array(a) => render_array(&a.to_values(), opts),
        Value::Record(f) => render_record(f, opts),
        Value::Tuple(_) => render_compact(v, opts),
        _ => color_leaf(v, opts, false),
    }
}

// ---- leaves ----

/// Render a scalar leaf. `nested` quotes strings (matching `Display` inside
/// collections); a top-level string prints unquoted.
fn color_leaf(v: &Value, opts: &RenderOpts, nested: bool) -> String {
    match v {
        Value::Int(i) => opts.paint(Role::Num, &fmt_int(*i)),
        Value::Float(x) => opts.paint(Role::Num, &fmt_float_rich(*x)),
        Value::Bool(b) => opts.paint(Role::Bool, &b.to_string()),
        Value::Missing => opts.paint(Role::Missing, "missing"),
        Value::Str(s) => {
            let t = if nested { format!("\"{s}\"") } else { (**s).clone() };
            opts.paint(Role::Str, &t)
        }
        Value::Dna(s) => opts.paint(Role::Dna, s),
        Value::Unit => "()".to_string(),
        other => other.to_string(),
    }
}

/// Format an `i64` with `_` thousands separators (mirroring Helix's own numeric
/// literal syntax, e.g. `1_000_000`). Handles `i64::MIN` via a 128-bit magnitude.
pub(crate) fn fmt_int(i: i64) -> String {
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
pub(crate) fn fmt_float_rich(x: f64) -> String {
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
            format!("{}{}{}", opts.paint(Role::Dim, "["), inner.join(", "), opts.paint(Role::Dim, "]"))
        }
        Value::Tuple(items) => {
            let mut inner: Vec<String> = items.iter().map(|e| render_compact(e, opts)).collect();
            if items.len() == 1 {
                inner.push(String::new()); // trailing comma → `(x,)`
            }
            format!("{}{}{}", opts.paint(Role::Dim, "("), inner.join(", "), opts.paint(Role::Dim, ")"))
        }
        Value::Record(fields) => {
            // Canonical sorted field order — the same rule `Display` applies;
            // the sweep found the rich (TTY) path still showing insertion
            // order, so `print(r)` and `"{r}"` disagreed in one session.
            let inner: Vec<String> = sorted_fields(fields)
                .map(|(k, val)| {
                    format!("{}: {}", opts.paint(Role::Key, k.as_str()), render_compact(val, opts))
                })
                .collect();
            format!("{}{}{}", opts.paint(Role::Dim, "{"), inner.join(", "), opts.paint(Role::Dim, "}"))
        }
        Value::DataFrame(_) => opts.paint(Role::Dim, "<dataframe>"),
        leaf => color_leaf(leaf, opts, true),
    }
}

fn compact_plain(v: &Value, opts: &RenderOpts) -> String {
    render_compact(v, &opts.as_plain())
}

// ---- records ----

/// Fields in canonical (sorted-by-name) order — one interner resolve per
/// name, mirroring the plain printer's rule and its perf shape.
fn sorted_fields(fields: &[(Symbol, Value)]) -> impl Iterator<Item = &(Symbol, Value)> {
    let mut order: Vec<usize> = (0..fields.len()).collect();
    let names: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
    if !names.windows(2).all(|w| w[0] <= w[1]) {
        order.sort_unstable_by(|&a, &b| names[a].cmp(names[b]));
    }
    order.into_iter().map(move |i| &fields[i])
}

fn render_record(fields: &[(Symbol, Value)], opts: &RenderOpts) -> String {
    let inline = render_compact(&Value::Record(std::rc::Rc::new(fields.to_vec())), opts);
    let plain_w = dw(&compact_plain(&Value::Record(std::rc::Rc::new(fields.to_vec())), opts));
    if plain_w <= opts.width || fields.len() <= 1 {
        return inline;
    }
    // Values start at a COMMON COLUMN. The multi-line form is only reached when a record
    // is too wide to inline, which is exactly the case where a reader scans DOWN the
    // values rather than reading across one pair. Ragged starts (`count: 7` above
    // `median: 3.0` above `min: 1.5`) make that scan do work for nothing.
    //
    // The pad is measured from the PLAIN key: `paint` may have wrapped it in escapes
    // that occupy no columns, and padding by the painted width would misalign exactly
    // when colour is on.
    let keyw = fields.iter().map(|(k, _)| dw(k.as_str())).max().unwrap_or(0);
    let mut s = opts.paint(Role::Dim, "{");
    s.push('\n');
    for (k, val) in sorted_fields(fields) {
        s.push_str("  ");
        s.push_str(&opts.paint(Role::Key, k.as_str()));
        s.push(':');
        s.push_str(&" ".repeat(keyw - dw(k.as_str()) + 1));
        s.push_str(&render_compact(val, opts));
        s.push_str(",\n");
    }
    s.push_str(&opts.paint(Role::Dim, "}"));
    s
}

// ---- arrays: table or list ----

fn render_array(vals: &[Value], opts: &RenderOpts) -> String {
    if vals.is_empty() {
        return opts.paint(Role::Dim, "[]");
    }
    if let Some(keys) = table_keys(vals) {
        let headers: Vec<String> = keys.iter().map(|k| k.as_str().to_string()).collect();
        let rows: Vec<Vec<Value>> = vals
            .iter()
            .map(|v| match v {
                // BY KEY, not by position — rows may be construction-ordered.
                Value::Record(f) => keys
                    .iter()
                    .map(|k| {
                        f.iter()
                            .find(|(k2, _)| k2 == k)
                            .map(|(_, val)| val.clone())
                            .unwrap_or(Value::Missing)
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        return render_table(&headers, &rows, None, opts);
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
    // Canonical header order (sorted), and rows may carry the same field SET
    // in any construction order — the table is a VIEW of values, and records
    // print order-canonically everywhere now.
    let mut keys: Vec<Symbol> = first.iter().map(|(k, _)| *k).collect();
    keys.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for v in &vals[1..] {
        let Value::Record(f) = v else { return None };
        if f.len() != keys.len() {
            return None;
        }
        if !f.iter().all(|(k, _)| keys.contains(k)) {
            return None;
        }
    }
    Some(keys)
}

// ---- the shared table renderer (record arrays and DataFrames) ----

struct Cell {
    plain: String,
    role: Role,
    numeric: bool,
}

fn cell_of(v: &Value, opts: &RenderOpts) -> Cell {
    let (plain, role, numeric) = match v {
        Value::Int(i) => (fmt_int(*i), Role::Num, true),
        Value::Float(x) => (fmt_float_rich(*x), Role::Num, true),
        Value::Bool(b) => (b.to_string(), Role::Bool, false),
        Value::Missing => ("missing".to_string(), Role::Missing, true),
        Value::Str(s) => ((**s).clone(), Role::Str, false), // unquoted in a table column
        Value::Dna(s) => ((**s).clone(), Role::Dna, false),
        Value::DataFrame(_) => ("<dataframe>".to_string(), Role::Dim, false),
        nested => (compact_plain(nested, opts), Role::Dim, false),
    };
    Cell { plain: truncate_str(&plain, CELL_MAX), role, numeric }
}

/// Render a table from explicit headers + rows. `total_rows`, when given (a
/// DataFrame), drives the "showing N of M" footer; otherwise (a record array) rows
/// past `max_rows` are elided in place with a head/tail and a `… N more rows` line.
fn render_table(
    headers: &[String],
    rows: &[Vec<Value>],
    total_rows: Option<usize>,
    opts: &RenderOpts,
) -> String {
    let ncol = headers.len();
    if ncol == 0 {
        return opts.paint(Role::Dim, "(empty)");
    }

    // Decide which rows to actually render (head/tail elision for record arrays).
    let n = rows.len();
    let elide = total_rows.is_none() && n > opts.max_rows;
    let (head, tail) = if elide {
        let t = (opts.max_rows / 5).max(1);
        (opts.max_rows - t, t)
    } else {
        (n, 0)
    };
    let shown_idx: Vec<usize> =
        (0..head).chain((n - tail)..n).collect::<Vec<_>>();

    // Build cells for the shown rows.
    let mut col_numeric = vec![true; ncol];
    let mut cell_rows: Vec<Vec<Cell>> = Vec::with_capacity(shown_idx.len());
    for &r in &shown_idx {
        let mut cells = Vec::with_capacity(ncol);
        for (c, numeric) in col_numeric.iter_mut().enumerate() {
            let v = rows[r].get(c).unwrap_or(&Value::Missing);
            let cell = cell_of(v, opts);
            if !cell.numeric {
                *numeric = false;
            }
            cells.push(cell);
        }
        cell_rows.push(cells);
    }

    // Column widths.
    let mut widths: Vec<usize> = headers.iter().map(|h| dw(h).min(CELL_MAX)).collect();
    for row in &cell_rows {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(dw(&cell.plain));
        }
    }

    // Fit columns to terminal width (each column costs width + 2 padding + 1 sep).
    let mut kept: Vec<usize> = Vec::new();
    let mut used = 1usize;
    for (c, &w) in widths.iter().enumerate() {
        let add = w + 3;
        if !kept.is_empty() && used + add > opts.width {
            break;
        }
        used += add;
        kept.push(c);
    }
    let dropped = ncol - kept.len();

    let b = &opts.boxes;
    let mut out = String::new();

    // Border helper: a horizontal rule using the given junction chars.
    let rule = |left: char, mid: char, right: char| -> String {
        let mut s = String::new();
        s.push(left);
        for (j, &c) in kept.iter().enumerate() {
            if j > 0 {
                s.push(mid);
            }
            for _ in 0..widths[c] + 2 {
                s.push(b.h);
            }
        }
        s.push(right);
        s
    };

    if b.bordered {
        out.push_str(&opts.paint(Role::Dim, &rule(b.tl, b.t_down, b.tr)));
        out.push('\n');
    }
    // Header.
    emit_table_row(&mut out, &headers_as_cells(headers), &kept, &widths, &col_numeric, Some(Role::Header), opts);
    out.push('\n');
    out.push_str(&opts.paint(Role::Dim, &rule(b.t_right, b.cross, b.t_left)));
    // Data rows (with the elision marker spliced in for record arrays).
    for (i, row) in cell_rows.iter().enumerate() {
        out.push('\n');
        if elide && i == head {
            let hidden = n - head - tail;
            out.push_str(&opts.paint(Role::Missing, &format!("  … {hidden} more rows")));
            out.push('\n');
        }
        emit_table_row(&mut out, row, &kept, &widths, &col_numeric, None, opts);
    }
    if b.bordered {
        out.push('\n');
        out.push_str(&opts.paint(Role::Dim, &rule(b.bl, b.t_up, b.br)));
    }
    // Footer notes.
    if let Some(total) = total_rows
        && total > n
    {
        out.push('\n');
        out.push_str(&opts.paint(Role::Missing, &format!("  showing {n} of {total} rows")));
    }
    if dropped > 0 {
        out.push('\n');
        out.push_str(&opts.paint(Role::Missing, &format!("  …+{dropped} more columns")));
    }
    out
}

fn headers_as_cells(headers: &[String]) -> Vec<Cell> {
    headers
        .iter()
        .map(|h| Cell { plain: truncate_str(h, CELL_MAX), role: Role::Header, numeric: false })
        .collect()
}

/// Emit one table row (header or data). `force_role` overrides each cell's own role
/// (used to bold the header). Cells are padded to the column width *after* coloring,
/// measuring the plain text so ANSI codes never skew alignment.
fn emit_table_row(
    out: &mut String,
    cells: &[Cell],
    kept: &[usize],
    widths: &[usize],
    col_numeric: &[bool],
    force_role: Option<Role>,
    opts: &RenderOpts,
) {
    let b = &opts.boxes;
    let sep = opts.paint(Role::Dim, &b.v.to_string());
    out.push_str(&sep);
    for (j, &c) in kept.iter().enumerate() {
        if j > 0 {
            out.push_str(&sep);
        }
        let cell = &cells[c];
        let role = force_role.unwrap_or(cell.role);
        out.push(' ');
        emit_padded(out, &cell.plain, role, widths[c], col_numeric[c], opts);
        out.push(' ');
    }
    out.push_str(&sep);
}

/// Truncate `plain` to `width`, color it, and pad to `width` (right- or
/// left-aligned). Width math uses the *plain* text so ANSI codes never throw it off.
fn emit_padded(
    out: &mut String,
    plain: &str,
    role: Role,
    width: usize,
    right: bool,
    opts: &RenderOpts,
) {
    let shown = truncate_str(plain, width);
    let pad = width.saturating_sub(dw(&shown));
    let colored = opts.paint(role, &shown);
    if right {
        out.push_str(&" ".repeat(pad));
        out.push_str(&colored);
    } else {
        out.push_str(&colored);
        out.push_str(&" ".repeat(pad));
    }
}

// ---- DataFrames ----

/// Render a DataFrame as a rich table: pull a head preview through the backend seam
/// (so all engine types stay confined there), render it, and note the total rows.
fn render_dataframe(
    df: &std::rc::Rc<crate::backend::Df>,
    opts: &RenderOpts,
    line: usize,
    col: usize,
) -> Result<String, HelixError> {
    let names = df.column_names(line, col)?;
    if names.is_empty() {
        // Nothing to tabulate — defer to the engine's own rendering.
        return display_value(&Value::DataFrame(df.clone()), line, col);
    }
    let total = df.row_count(line, col)?;
    let preview = df.head(opts.max_rows);
    // Column-major from the seam → row-major for the table.
    let cols: Vec<Vec<Value>> =
        names.iter().map(|n| preview.column_values(n, line, col)).collect::<Result<_, _>>()?;
    let shown = cols.first().map(|c| c.len()).unwrap_or(0);
    let rows: Vec<Vec<Value>> =
        (0..shown).map(|r| cols.iter().map(|c| c[r].clone()).collect()).collect();
    Ok(render_table(&names, &rows, Some(total), opts))
}

// ---- non-table arrays ----

fn render_list(vals: &[Value], opts: &RenderOpts) -> String {
    let plain_parts: Vec<String> = vals.iter().map(|v| compact_plain(v, opts)).collect();
    let inline_w = 2 + plain_parts.iter().map(|p| dw(p)).sum::<usize>()
        + 2 * plain_parts.len().saturating_sub(1);
    let multiline = plain_parts.iter().any(|p| p.contains('\n'));

    if vals.len() <= opts.max_rows && inline_w <= opts.width && !multiline {
        let parts: Vec<String> = vals.iter().map(|v| render_compact(v, opts)).collect();
        return format!(
            "{}{}{}",
            opts.paint(Role::Dim, "["),
            parts.join(", "),
            opts.paint(Role::Dim, "]")
        );
    }

    let n = vals.len();
    let (head, tail) = if n > opts.max_rows {
        let t = (opts.max_rows / 5).max(1);
        (opts.max_rows - t, t)
    } else {
        (n, 0)
    };
    let mut s = opts.paint(Role::Dim, "[");
    s.push('\n');
    for v in vals.iter().take(head) {
        s.push_str("  ");
        s.push_str(&render_compact(v, opts));
        s.push_str(",\n");
    }
    if n > opts.max_rows {
        s.push_str("  ");
        s.push_str(&opts.paint(Role::Missing, &format!("… {} more", n - head - tail)));
        s.push('\n');
        for v in vals.iter().skip(n - tail) {
            s.push_str("  ");
            s.push_str(&render_compact(v, opts));
            s.push_str(",\n");
        }
    }
    s.push_str(&opts.paint(Role::Dim, "]"));
    s
}

// ---- small helpers shared with the chart module ----

/// Display width (column count). Approximates with `char` count — fine for the
/// ASCII-dominant data Helix prints; wide-CJK alignment is a deliberate non-goal.
pub(crate) fn dw(s: &str) -> usize {
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

// ---- the chart module's view of rendering ----

/// Paint helper exposed to [`crate::chart`] so charts share the active theme/color.
pub(crate) fn paint_axis(opts: &RenderOpts, s: &str) -> String {
    opts.paint(Role::Axis, s)
}
pub(crate) fn paint_bar(opts: &RenderOpts, s: &str) -> String {
    opts.paint(Role::Bar, s)
}
pub(crate) fn paint_num(opts: &RenderOpts, s: &str) -> String {
    opts.paint(Role::Num, s)
}
pub(crate) fn paint_dim(opts: &RenderOpts, s: &str) -> String {
    opts.paint(Role::Dim, s)
}
pub(crate) fn paint_header(opts: &RenderOpts, s: &str) -> String {
    opts.paint(Role::Header, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn opts(color: bool) -> RenderOpts {
        RenderOpts {
            rich: true,
            color,
            width: 80,
            max_rows: 20,
            theme: Theme::named("default"),
            boxes: BoxStyle::named("rounded"),
            plot: crate::chart::PlotGlyphs::Braille,
        }
    }

    fn rec(pairs: &[(&str, Value)]) -> Value {
        Value::Record(Rc::new(pairs.iter().map(|(k, v)| (Symbol::intern(k), v.clone())).collect()))
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
    fn record_array_renders_as_a_bordered_table() {
        let vals = vec![
            rec(&[("gene", Value::Str(Rc::new("BRCA1".into()))), ("len", Value::Int(81188))]),
            rec(&[("gene", Value::Str(Rc::new("TP53".into()))), ("len", Value::Int(19149))]),
        ];
        let out = render_value_with(&Value::array(vals), &opts(false));
        assert!(out.contains("gene") && out.contains("len"), "got:\n{out}");
        assert!(out.contains('╭') && out.contains('╰'), "expected a rounded box, got:\n{out}");
        assert!(out.contains('│'), "expected a column separator, got:\n{out}");
        assert!(out.contains("81_188"), "expected a grouped number, got:\n{out}");
    }

    #[test]
    fn box_style_is_selectable() {
        let vals = vec![rec(&[("a", Value::Int(1))]), rec(&[("a", Value::Int(2))])];
        let mut o = opts(false);
        o.boxes = BoxStyle::named("ascii");
        let out = render_value_with(&Value::array(vals), &o);
        assert!(out.contains('+') && out.contains('-'), "ascii box, got:\n{out}");
        assert!(!out.contains('╭'));
    }

    #[test]
    fn long_array_is_elided() {
        let vals: Vec<Value> = (0..100).map(Value::Int).collect();
        let out = render_value_with(&Value::array(vals), &opts(false));
        assert!(out.contains("more"), "expected an elision marker, got:\n{out}");
        assert!(out.lines().count() < 40, "should be elided, got {} lines", out.lines().count());
    }

    #[test]
    fn color_wraps_leaves_in_ansi() {
        let out = render_value_with(&Value::Int(42), &opts(true));
        assert!(out.starts_with("\x1b[36m") && out.ends_with("\x1b[0m"), "got: {out:?}");
        assert_eq!(render_value_with(&Value::Int(42), &opts(false)), "42");
    }

    #[test]
    fn mono_theme_drops_hue_but_keeps_structure() {
        let mut o = opts(true);
        o.theme = Theme::named("mono");
        // A number has no color in mono (empty code → no escape).
        assert_eq!(render_value_with(&Value::Int(7), &o), "7");
    }

    #[test]
    fn short_scalar_array_is_inline() {
        let vals = vec![Value::Int(1), Value::Int(2), Value::Int(3)];
        assert_eq!(render_value_with(&Value::array(vals), &opts(false)), "[1, 2, 3]");
    }

    #[test]
    fn top_level_string_is_unquoted_nested_is_quoted() {
        let s = Value::Str(Rc::new("hi".into()));
        assert_eq!(render_value_with(&s, &opts(false)), "hi");
        let arr = Value::array(vec![s]);
        assert_eq!(render_value_with(&arr, &opts(false)), "[\"hi\"]");
    }
}

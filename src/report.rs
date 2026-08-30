//! Command reports — the block a CLI subcommand prints when it finishes.
//!
//! ONE structure renders both forms. A terminal gets an aligned, coloured block; a pipe,
//! a log or a test gets `label: value` lines. Both are generated from the SAME rows in
//! the same order, so the two cannot drift into telling different stories — which is
//! exactly what two hand-written `println!` blocks per command would become.
//!
//! Everything degrades in the order the rest of the CLI already degrades: rich only on a
//! terminal (or `HELIX_RICH=1`), colour only when rich and not refused by `NO_COLOR`, and
//! the rule character comes from `HELIX_BOX`, so a terminal that cannot draw `─` gets `-`
//! here for the same reason its tables do.

use crate::render::{dw, Role, RenderOpts};

/// A byte count for a READER.
///
/// The same distinction `chart::fmt_axis` draws: `{:.1} MB` is a fine way to say 6.7 MB
/// and a useless way to say 39 bytes, which it renders as `0.0 MB`. A size is read to
/// answer "how big, roughly" — so the unit follows the magnitude, and the decimal is
/// dropped once it stops carrying information.
pub fn bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    let (v, unit) = if n < K {
        return format!("{n:.0} B");
    } else if n < K * K {
        (n / K, "KB")
    } else if n < K * K * K {
        (n / (K * K), "MB")
    } else {
        (n / (K * K * K), "GB")
    };
    // Three significant figures is the whole requirement: `6.7 MB`, `123 MB`, `2.4 GB`.
    if v >= 100.0 { format!("{v:.0} {unit}") } else { format!("{v:.1} {unit}") }
}

/// One line of a report.
pub enum Row {
    /// `label   value`, with an optional continuation beneath it.
    ///
    /// The label is owned rather than `&'static str` because some labels are DATA — a
    /// lockfile lists one row per package name — and a report that could only label rows
    /// with literals would push those callers back to hand-rolled `println!`.
    Field { label: String, value: String, note: Option<String> },
    /// Breathing room in the rich form; nothing at all in the plain one, where a blank
    /// line in a log is noise rather than structure.
    Gap,
}

pub struct Report {
    title: &'static str,
    headline: String,
    rows: Vec<Row>,
}

impl Report {
    /// `title` heads the rich block; `headline` is the sentence a pipe sees first, and is
    /// repeated (undecorated) at the top of the rich block so the two never disagree.
    pub fn new(title: &'static str, headline: impl Into<String>) -> Self {
        Report { title, headline: headline.into(), rows: Vec::new() }
    }

    pub fn field(mut self, label: &'static str, value: impl Into<String>) -> Self {
        self.rows.push(Row::Field { label: label.to_string(), value: value.into(), note: None });
        self
    }

    /// [`Report::note`] for a label computed at run time.
    pub fn note_owned(
        mut self,
        label: String,
        value: impl Into<String>,
        note: impl Into<String>,
    ) -> Self {
        self.rows.push(Row::Field { label, value: value.into(), note: Some(note.into()) });
        self
    }

    /// [`Report::field`] for a label computed at run time.
    pub fn field_owned(mut self, label: String, value: impl Into<String>) -> Self {
        self.rows.push(Row::Field { label, value: value.into(), note: None });
        self
    }

    pub fn note(mut self, label: &'static str, value: impl Into<String>, note: impl Into<String>) -> Self {
        self.rows.push(Row::Field {
            label: label.to_string(),
            value: value.into(),
            note: Some(note.into()),
        });
        self
    }

    pub fn gap(mut self) -> Self {
        self.rows.push(Row::Gap);
        self
    }

    /// Join values for a list field. `·` reads as a separator rather than punctuation
    /// belonging to a value, which matters when the values are themselves comma-free
    /// identifiers; ASCII mode falls back to a comma because a middle dot is exactly the
    /// sort of character a limited font renders as a box.
    /// The separator this output should use between items.
    ///
    /// Exposed because every caller that hand-wrote `\u{b7}` got it wrong: plain output is
    /// PARSED, and a multi-byte separator in a line a script splits is a defect, not a
    /// style. Ask once, here.
    pub fn sep(opts: &RenderOpts) -> &'static str {
        if !opts.rich || opts.ascii_only() { ", " } else { " \u{b7} " }
    }

    pub fn list(opts: &RenderOpts, items: &[&str]) -> String {
        // PLAIN OUTPUT IS PARSED, so it gets a comma. `·` is a multi-byte character in a
        // line a script may split, and the middle dot earns its place only in the rich
        // form, where it reads as a separator rather than as punctuation belonging to a
        // value. `HELIX_BOX=ascii` opts out for the same reason its tables do.
        let sep = if !opts.rich || opts.ascii_only() { ", " } else { " · " };
        items.join(sep)
    }

    fn wrap_into(text: &str, width: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            if !line.is_empty() && dw(&line) + 1 + dw(word) > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push(line);
        }
        out
    }

    pub fn print(&self, opts: &RenderOpts) {
        if !opts.rich {
            // The plain form is what a script parses and a test asserts, so it stays
            // flat: one fact per line, no alignment padding to strip, no colour.
            println!("{}", self.headline);
            for r in &self.rows {
                if let Row::Field { label, value, note } = r {
                    println!("{label}: {value}");
                    if let Some(n) = note {
                        println!("  {n}");
                    }
                }
            }
            return;
        }
        println!();
        println!("  {}", opts.paint(Role::Header, self.title));
        println!("  {}", opts.paint(Role::Dim, &opts.rule(48)));
        println!("  {}", self.headline);
        // Labels align on their widest, computed from the PLAIN text: painting wraps a
        // label in escapes that occupy no columns, and padding by the painted width
        // misaligns exactly when colour is on.
        let w = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Field { label, .. } => Some(dw(label)),
                Row::Gap => None,
            })
            .max()
            .unwrap_or(0);
        for r in &self.rows {
            match r {
                Row::Gap => println!(),
                Row::Field { label, value, note } => {
                    let pad = " ".repeat(w - dw(label));
                    println!("  {}{}  {}", opts.paint(Role::Key, label), pad, value);
                    if let Some(n) = note {
                        // A note is prose, and prose that runs past the terminal wraps
                        // wherever the terminal decides — mid-word, under the value
                        // column, destroying the alignment the block exists for.
                        let indent = w + 4;
                        for line in Self::wrap_into(n, opts.width.saturating_sub(indent).max(24)) {
                            println!("  {}  {}", " ".repeat(w), opts.paint(Role::Dim, &line));
                        }
                    }
                }
            }
        }
        println!();
    }
}

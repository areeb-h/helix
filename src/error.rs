//! Friendly, educational error reporting.
//!
//! Principle #5: errors should teach, not scold. Every error carries a source
//! position so we can point a caret at the exact spot, plus an optional `hint`
//! that suggests a fix.

#[derive(Debug, Clone)]
pub struct HelixError {
    pub message: String,
    pub line: usize,
    pub col: usize,
    pub hint: Option<String>,
}

impl HelixError {
    pub fn new(message: impl Into<String>, line: usize, col: usize) -> Self {
        HelixError {
            message: message.into(),
            line,
            col,
            hint: None,
        }
    }

    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }

    /// Render the error against the original source, with a caret and a help line.
    pub fn render(&self, src: &str, filename: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!("error: {}\n", self.message));
        out.push_str(&format!("  --> {}:{}:{}\n", filename, self.line, self.col));

        if let Some(line_str) = src.lines().nth(self.line.saturating_sub(1)) {
            let num = self.line.to_string();
            let gutter = " ".repeat(num.len());
            out.push_str(&format!("{} |\n", gutter));
            out.push_str(&format!("{} | {}\n", num, line_str));
            let caret_pad = " ".repeat(self.col.saturating_sub(1));
            out.push_str(&format!("{} | {}^\n", gutter, caret_pad));
        }

        if let Some(h) = &self.hint {
            out.push_str(&format!("help: {}\n", h));
        }
        out
    }
}

/// Push onto a `Vec` with **fallible** allocation. The bio/tabular readers load a
/// whole file into a `Vec<record>` (no streaming yet), so a multi-GB input would
/// otherwise grow the vector until the allocator aborts the process. Reserving room
/// for one more element first turns "out of memory" into a clean, catchable Helix
/// error. `try_reserve(1)` is amortized O(1) — when capacity already exists it does
/// nothing, and when it grows it still grows geometrically — so this is as cheap as
/// a plain `push` on the happy path.
pub fn try_push<T>(
    v: &mut Vec<T>,
    item: T,
    what: &str,
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    v.try_reserve(1).map_err(|_| {
        HelixError::new(format!("ran out of memory loading {what}"), line, col)
            .hint("the file is too large to hold in memory; filter or subsample it first.")
    })?;
    v.push(item);
    Ok(())
}

/// Allocate a `Vec` with capacity for `n` elements using **fallible** allocation,
/// turning an out-of-memory abort into a clean Helix error. Use where `n` comes
/// from runtime data (e.g. a DataFrame column sized to an input array) rather than
/// a small constant.
pub fn try_with_capacity<T>(
    n: usize,
    what: &str,
    line: usize,
    col: usize,
) -> Result<Vec<T>, HelixError> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| {
        HelixError::new(format!("ran out of memory building {what}"), line, col)
            .hint("the column is too large to materialize; filter or subsample first.")
    })?;
    Ok(v)
}

/// Reserve one slot in each listed column `Vec`, failing cleanly on OOM. The
/// tabular readers (VCF/SAM/GFF/BED) store columns as parallel vectors that grow
/// in lockstep, one push each per record. Reserving a slot in *every* column
/// before any of them pushes means a huge file surfaces as a catchable Helix error
/// rather than an allocator abort, and leaves all columns the same length on
/// failure (no half-written row). Cheap on the happy path — `try_reserve(1)` is a
/// no-op when capacity already exists.
macro_rules! reserve_rows {
    ($what:expr, $line:expr, $col:expr, $($v:expr),+ $(,)?) => {{
        $(
            $v.try_reserve(1).map_err(|_| {
                $crate::error::HelixError::new(
                    format!("ran out of memory loading {}", $what),
                    $line,
                    $col,
                )
                .hint("the file is too large to hold in memory; filter or subsample it first.")
            })?;
        )+
    }};
}
pub(crate) use reserve_rows;

/// Levenshtein edit distance — used to suggest "did you mean ...?".
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Pick the closest candidate name, if it's close enough to be a plausible typo.
pub fn suggest(name: &str, candidates: &[&str]) -> Option<String> {
    let mut best: Option<&str> = None;
    let mut best_d = usize::MAX;
    for c in candidates {
        let d = edit_distance(name, c);
        if d < best_d {
            best_d = d;
            best = Some(c);
        }
    }
    match best {
        // Accept if within 2 edits, or within ~1/3 of the word length for longer names.
        Some(c) if best_d <= 2 || best_d * 3 <= name.len() => Some(c.to_string()),
        _ => None,
    }
}

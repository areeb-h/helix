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

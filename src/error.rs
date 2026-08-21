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

        // An *empty* hint renders as no `help:` line — the bytecode `Op::Raise` carries
        // its hint as a plain string, so "no advice" reaches here as "" rather than
        // `None`, and `help: ` on its own would be worse than silence.
        if let Some(h) = self.hint.as_deref().filter(|h| !h.is_empty()) {
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

/// Optimal string alignment distance — Levenshtein, plus **transposing an adjacent
/// pair counts as one edit**. A swapped pair is the single most common typing error
/// (`maen` for `mean`), and plain Levenshtein charges it two, which would put it out
/// of reach of the one-edit rule below.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // Three rows: the transposition case reaches back two rows and two columns.
    let mut prev2 = vec![0usize; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut d = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            cur[j] = d;
        }
        // Rotate: prev2 := prev (row i-1), prev := cur (row i).
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// How far `name` is from `cand` **if that gap is small enough to be a typo** —
/// `None` when the two are simply different words.
///
/// The rule, and it is deliberately strict: case is ignored (so `Inf` finds `inf`),
/// and beyond that **one edit, and only in a name of four characters or more**.
/// One edit inside a three-letter word rewrites a third of it, which is how `nil`
/// became `pi`, `NA` became `e`, `sum` became `sin`, `cat` became `cbrt`, `disp`
/// became `dict` and `odd` became `ord`. A wrong suggestion is worse than silence:
/// a scientist told that `sum` means `sin` may believe it and ship the number.
///
/// Foreign spellings that this rule (correctly) refuses to guess at are answered by
/// name from the alias table in [`crate::suggest`], not by edit distance.
pub fn typo_distance(name: &str, cand: &str) -> Option<usize> {
    let a = name.to_lowercase();
    let b = cand.to_lowercase();
    let d = edit_distance(&a, &b);
    if d == 0 {
        return Some(0); // differs by case alone
    }
    if d > 1 {
        return None;
    }
    let longest = a.chars().count().max(b.chars().count());
    (longest >= 4).then_some(d)
}

/// Pick the closest candidate name, if it's close enough to be a plausible typo.
/// Ties break on the smallest name so the answer never depends on the order the
/// caller collected its candidates (several arrive from hash maps) — the three
/// engines must render byte-identical errors.
pub fn suggest(name: &str, candidates: &[&str]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        let Some(d) = typo_distance(name, c) else { continue };
        if best.is_none_or(|(bd, bc)| d < bd || (d == bd && *c < bc)) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c.to_string())
}

/// The message + hint for reassigning an immutable binding, shared by the walker's env
/// check and the compiler's raise ops so all three engines stay byte-identical.
///
/// The seeded math constants get their own wording because the generic hint was
/// actively harmful there: "declare it as mutable up front" WORKS for them — it
/// silently shadows Euler's number (or pi, or inf) for the whole file, which is the
/// trap, not the fix. An agent-written physics library hit exactly this: the natural
/// variable name for elementary charge is `e`.
pub fn immutable_reassign(name: &str) -> (String, String) {
    let known = match name {
        "e" => Some("Euler's number, 2.71828..."),
        "pi" => Some("3.14159..."),
        "inf" => Some("positive infinity"),
        _ => None,
    };
    match known {
        Some(what) => (
            format!("`{name}` is a built-in constant ({what}) and cannot be reassigned"),
            format!(
                "pick another name (`{name}_`, `E_CHARGE`, ...) — `mut {name} = ...` \
                 would shadow the constant for the whole file."
            ),
        ),
        None => (
            format!("`{name}` is immutable and cannot be reassigned"),
            format!(
                "declare it as mutable up front with `mut {name} = ...` if it needs to change."
            ),
        ),
    }
}

/// The recursion cap, worded so it teaches the rule it is reporting the edge of.
///
/// Helix optimises TAIL calls — when the recursive call is the whole result, with
/// nothing left to do after it, the frame is reused and a mutually tail-recursive pair
/// runs to millions of levels. So reaching this cap means the call is not in tail
/// position, and that is the single most useful thing to say: a reader told only
/// "maximum recursion depth exceeded" goes looking for a loop the language does not
/// have, when rewriting the call into tail position would remove the limit entirely.
///
/// One constructor for all four sites (the tree-walker's, and the VM's three) so the
/// engines cannot word the same condition differently.
pub fn recursion_depth_err(max: usize, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("maximum recursion depth ({max}) exceeded"), line, col).hint(
        "this call is not in TAIL position, so every level keeps a frame. A tail call \
         — where the recursive call is the whole result, with nothing left to do after \
         it — reuses its frame and runs to millions of levels. Otherwise: is a base \
         case missing, or would a comprehension (`map`/`filter`/`reduce`) fit better?",
    )
}

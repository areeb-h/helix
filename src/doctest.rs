//! Extraction of `>>>` examples from `##` doc comments.
//!
//! Deliberately dependency-free — no `crate::` imports, only `std`. That is what lets
//! `tests/cli.rs` pull this exact file in with `#[path = "../src/doctest.rs"]` (the crate
//! has no library target, so an integration test cannot `use helix::…`). The gate and
//! `helix test` therefore share ONE extractor rather than two that can drift, which
//! matters here more than usual: a doc-example runner that quietly stops finding examples
//! reports success forever.
//!
//! The format is the smallest thing that cannot be ambiguous: inside a `##` block, a line
//! whose first token is `>>>` is code; consecutive `>>>` lines are one program; the plain
//! `##` lines that follow are its expected output, ending at a blank doc line, the next
//! `>>>`, or the end of the block. See `docs/comments-and-docs.md`.

/// One documented example, lifted out of a `##` doc comment.
pub struct DocExample {
    pub line: usize,
    /// The `>>>` lines, in order. All but the last are setup.
    pub code: Vec<String>,
    /// The plain lines beneath, compared exactly against stdout (or stderr, for an error).
    pub expect: Vec<String>,
    /// Columns of indentation the `>>>` sat at, stripped from the expected lines so the
    /// block can be indented under the prose without that indent becoming part of the
    /// expected output. Deeper indentation inside the output itself is preserved.
    pub indent: usize,
}

/// Pull every `>>>` example out of the `##` doc comments in one source file.
pub fn doc_examples_in(src: &str) -> Vec<DocExample> {
    let mut out: Vec<DocExample> = Vec::new();
    let mut cur: Option<DocExample> = None;
    for (i, raw) in src.lines().enumerate() {
        let t = raw.trim_start();
        // Only `##` lines participate. A plain `#` comment can sit inside an example
        // block without terminating it being ambiguous, because it is not a doc line.
        let Some(body) = t.strip_prefix("##") else {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            continue;
        };
        let body = body.strip_prefix(' ').unwrap_or(body);
        let trimmed = body.trim_start();
        if let Some(code) = trimmed.strip_prefix(">>>") {
            let indent = body.len() - trimmed.len();
            let code = code.trim().to_string();
            match cur.as_mut() {
                // A `>>>` directly after previous code (no expected output yet) continues
                // the same program; one after expected output starts a new example.
                Some(e) if e.expect.is_empty() => e.code.push(code),
                _ => {
                    if let Some(e) = cur.take() {
                        out.push(e);
                    }
                    cur = Some(DocExample {
                        line: i + 1,
                        code: vec![code],
                        expect: Vec::new(),
                        indent,
                    });
                }
            }
        } else if let Some(e) = cur.as_mut() {
            if trimmed.is_empty() {
                out.push(cur.take().unwrap());
            } else {
                // Strip exactly the `>>>` line's indentation, no more — so output that is
                // itself indented keeps that shape.
                let mut line = body.trim_end();
                for _ in 0..e.indent {
                    line = line.strip_prefix(' ').unwrap_or(line);
                }
                e.expect.push(line.to_string());
            }
        }
    }
    if let Some(e) = cur.take() {
        out.push(e);
    }
    out
}

/// Build the program that runs one example: the file's own source, then the example's
/// `>>>` lines, with the LAST line wrapped in `print(...)` when the example documents an
/// output. Shared so the gate and `helix test` synthesize identically.
pub fn example_program(file_src: &str, ex: &DocExample) -> String {
    let mut prog = String::with_capacity(file_src.len() + 64);
    prog.push_str(file_src);
    prog.push('\n');
    for (i, line) in ex.code.iter().enumerate() {
        if i + 1 == ex.code.len() && !ex.expect.is_empty() && !is_bare_print(line) {
            prog.push_str(&format!("print({line})\n"));
        } else {
            prog.push_str(line);
            prog.push('\n');
        }
    }
    prog
}

/// Is this code line already a single bare `print(...)` call? Wrapping one in
/// another `print` emits the value and then `()` (print's Unit return), so
/// `## >>> print(1 + 2)` expecting `3` could never pass — and the diagnostic showed
/// `expected: 3` / `got: 3`, the real difference being an invisible trailing `()`.
///
/// Std-only by the module-header rule, so no parser: a string-literal-aware bracket
/// scan. The line qualifies iff it starts with `print(` and the close that returns
/// the depth to zero is a `)` sitting at the very end — nothing before the call,
/// nothing after it, so the whole line IS the call.
fn is_bare_print(line: &str) -> bool {
    let Some(rest) = line.trim().strip_prefix("print(") else {
        return false;
    };
    let (mut depth, mut in_str, mut escaped) = (1i32, false, false);
    for (i, c) in rest.char_indices() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return c == ')' && i + c.len_utf8() == rest.len();
                }
            }
            _ => {}
        }
    }
    false
}

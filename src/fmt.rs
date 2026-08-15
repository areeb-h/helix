//! `helix fmt` — the formatter.
//!
//! # It cannot change your program, and that is provable
//!
//! Every mainstream formatter is a *printer*: it parses to a tree and renders the tree back
//! out. That is how they get to re-wrap long lines, and it is also how they get to lose
//! comments, rewrite string delimiters, and — rarely but really — change what a program
//! does. Helix cannot take that design even if it wanted to, for a reason specific to this
//! parser: `src/parser.rs` DESUGARS fourteen constructs before anything reaches the AST, four
//! of them synthesizing `$`-prefixed names. Printing the AST for a user's `xs.sort_by(k)`
//! would emit `let $s = xs in $s.map(k).argsort().map($si => $s[$si])`, and `$` does not lex,
//! so the "formatted" file would not even tokenize.
//!
//! So this formatter never runs the parser. It reads the token stream from
//! [`crate::lexer::lex_trivia`] and re-emits each token's SOURCE BYTES verbatim
//! (`src[tok.start..tok.end]`), deciding only what goes between them. From that one
//! constraint everything else follows:
//!
//! * It cannot rewrite a literal. `'a'`, `"a"` and `'''a'''` all lex to `Tok::Str("a")` and
//!   `1e3` lexes to `1000.0` — a printer working from `Tok` would silently change all four.
//! * It cannot delete or reorder a token, so **`lex(fmt(src))` equals `lex(src)`
//!   token-for-token**. That is a static identity, checked over every file in the repository
//!   and asserted as a property test — not "we ran the tests and it looked fine".
//! * It formats a file that does not PARSE, because it only needs the file to LEX. That is
//!   the moment you most want a formatter and the moment prettier, rustfmt, black and gofmt
//!   all refuse: mid-edit.
//!
//! # The author owns the vertical; fmt owns the horizontal
//!
//! **fmt never joins two lines and never splits one.** It normalizes indentation and the
//! spacing between tokens on a line, and takes every line-break decision from the source.
//!
//! This is not a limitation dressed up as a principle; it is the specific failure this repo
//! already refuses to accept from its own toolchain. `cargo fmt --all --check` reports 1280
//! diffs here and is deliberately non-blocking in CI, because rustfmt re-indents
//! hand-wrapped comment prose it will not re-wrap. A formatter that never reflows can never
//! do that. It also means there is nothing to escape from, which is why there is no
//! `# fmt: off` — every other tool's escape hatch exists to get away from reflowing.
//!
//! Newlines are load-bearing in Helix and that is the second reason. `cook_newlines` decides
//! whether a break is significant from the tokens around it: `a` ⏎ `- b` is two statements
//! while `a -` ⏎ `b` is one expression. A formatter that moved breaks would change parses.
//! Blank lines are the one exception — runs of them provably collapse in `cook_newlines`, so
//! normalizing them is free.
//!
//! # Zero configuration
//!
//! No config file, no width flag, no options. Prettier calls four of its own options
//! "historical artifacts" and has frozen the set; rustfmt has ~90, most nightly-only, so the
//! option you need is routinely the one you cannot have. Style changes here go through the
//! ADR process the language already uses.
//!
//! Two decisions were measured against the repository rather than chosen by taste:
//! **two-space indent** (18 of 28 files that indent at all already use it) and **the gap
//! before a trailing comment is left exactly as written** (141 lines align theirs, and R1
//! says prose placement is the author's).

use crate::error::HelixError;
use crate::lexer::lex_trivia;
use crate::token::{Tok, Token};

/// Two spaces. Measured, not chosen: of the 28 tracked `.helix` files that indent at all, 18
/// use two and 10 use four.
const INDENT: &str = "  ";

/// Format one source string. `Err` only if it does not LEX — a parse error is fine, and
/// formatting it is the point.
pub fn format_source(src: &str) -> Result<String, HelixError> {
    let toks = lex_trivia(src)?;
    // Split into physical lines. Newline tokens are separators, not content.
    let mut lines: Vec<Vec<&Token>> = vec![Vec::new()];
    for t in &toks {
        match t.tok {
            Tok::Newline => lines.push(Vec::new()),
            Tok::Eof => {}
            _ => lines.last_mut().expect("one line always exists").push(t),
        }
    }

    let mut out = String::with_capacity(src.len());
    let mut blank_run = 0usize;
    // Whether the PREVIOUS code line ended somewhere a statement cannot end, so this one is
    // its continuation and gets an extra step of indent.
    let mut continuing = false;
    // ONE STEP PER LINE THAT LEAVES SOMETHING OPEN — not one per bracket. Each entry is
    // `(bracket depth the line started from, the indent that line was printed at)`.
    //
    // Counting brackets instead is the obvious implementation and it is wrong twice over.
    // `print((range(0, n)).map(i =>` leaves two brackets open but is ONE visual step, so its
    // body lands six columns deep where the author wrote two; and a closing line puts the
    // bracket back at the indent of whatever opened it, which a depth count cannot know once
    // a continuation step is also in play (`else do {` is at indent 1, so its `}` is too).
    //
    // Storing the opening line's own indent makes both fall out: a body is one step past
    // its opener, and a closer returns to its opener exactly.
    let mut steps: Vec<(usize, usize)> = Vec::new();
    let mut depth: usize = 0;

    for line in lines.iter() {
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        // A run of blank lines is worth at most one, and never at the top of the file. This
        // is safe precisely because `cook_newlines` collapses runs, so no program can tell.
        if blank_run > 0 && !out.is_empty() {
            out.push('\n');
        }
        blank_run = 0;

        let code: Vec<&Token> =
            line.iter().copied().filter(|t| !matches!(t.tok, Tok::Comment(_))).collect();

        // Walk the line once to learn two things: the LOWEST bracket depth it reaches, and
        // the depth it ends at. The low-water mark is what decides the line's own indent —
        // `).sum())` starts at depth 2 and touches 0, and belongs at 0, which counting only
        // its leading closers would miss.
        let (mut d, mut low) = (depth, depth);
        // How far the LEADING run of closers unwinds. Only that decides the line's own
        // indent: `}` and `).sum())` belong beside whatever opened them, but
        // `(0..n).reduce(…)))` closes three levels at its END and still belongs at the
        // indent of the level it starts inside.
        let mut lead = depth;
        let mut still_leading = true;
        for t in &code {
            match t.tok {
                Tok::LParen | Tok::LBracket | Tok::LBrace => {
                    d += 1;
                    still_leading = false;
                }
                Tok::RParen | Tok::RBracket | Tok::RBrace => {
                    d = d.saturating_sub(1);
                    low = low.min(d);
                    if still_leading {
                        lead = d;
                    }
                }
                _ => still_leading = false,
            }
        }
        let (end_depth, low, lead) = (d, low, lead);

        // Unwind the steps the LEADING closers left. If any were closed, the line belongs at
        // the indent of the one it closed — a `}` lines up with whatever opened it.
        let mut closed_to: Option<usize> = None;
        while steps.last().is_some_and(|&(at, _)| at >= lead) {
            closed_to = steps.pop().map(|(_, ind)| ind);
        }
        let mut indent = closed_to.unwrap_or_else(|| steps.last().map_or(0, |&(_, i)| i + 1));
        // The continuation step. A line is a continuation if the previous one ended somewhere
        // a statement cannot end, OR if this one BEGINS somewhere a statement cannot begin
        // (`else`, `then`, `in`, a leading `.`, a leading infix operator) — the same two-sided
        // judgement `cook_newlines` makes about whether the break between them is real.
        // Only at the top level: inside brackets the bracket step is already the signal, and
        // both would step an argument list twice, since every line of a list ends in a comma.
        let starts_continuation = code.first().is_some_and(|t| continues_before_this_line(&t.tok));
        if (continuing || starts_continuation) && closed_to.is_none() {
            indent += 1;
        }
        // A whole-line comment takes its statement's indent, and nothing else about it is
        // touched — not one byte inside it, and not the blank `##` lines that separate doc
        // examples, which the extractor reads as example boundaries.
        for _ in 0..indent {
            out.push_str(INDENT);
        }
        out.push_str(&render_line(src, line));
        out.push('\n');

        // A step whose bracket this line's TRAILING closers ended is dead: a step's
        // bracket closes when depth falls back to its opening level (`low <= at`), but
        // only LEADING closers participate in the indent unwind above — so a body that
        // ends with its closers at the END of the line
        // (`…reduce(0.0, (acc, j) => acc + 1.0))`) left its step queued, and the NEXT
        // flush-left line popped it and inherited its indent. That is how a column-0
        // comment after a wrapped nested lambda came out at column 2, more indented
        // than the `export fn` it documented. Dead steps are discarded silently — they
        // must not influence anyone's indent; this line's own indent was already
        // decided above.
        while steps.last().is_some_and(|&(at, _)| at >= low) {
            steps.pop();
        }
        // If the line leaves something open, it is the opener of one step — one, however
        // many brackets it actually left open.
        let opened = end_depth > low;
        if opened {
            steps.push((low, indent));
        }
        depth = end_depth;
        continuing = match code.last() {
            // A line that is only a comment neither starts nor ends a continuation.
            None => continuing,
            // A line that opened a bracket has already been paid for. `c = (0..n).map(i =>`
            // both opens and ends in a continuer, and charging it twice steps the body two
            // levels where one is meant.
            Some(_) if opened => false,
            Some(t) => continues_after_this_line(&t.tok),
        };
    }

    // Exactly one trailing newline: no blank tail, and never a file that ends mid-line.
    while out.ends_with("\n\n") {
        out.pop();
    }
    if out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// One line's tokens, spaced. The token text is always `src[start..end]` — never rebuilt
/// from `Tok`, which has already lost every literal's spelling.
fn render_line(src: &str, line: &[&Token]) -> String {
    let mut s = String::new();
    for (i, t) in line.iter().enumerate() {
        if i > 0 {
            let prev = line[i - 1];
            if let Tok::Comment(_) = t.tok {
                // THE GAP BEFORE A TRAILING COMMENT IS THE AUTHOR'S. 141 lines in this
                // repository align theirs into a column; collapsing that to one space would
                // destroy a deliberate layout for no functional gain, which is exactly the
                // complaint this project has about `cargo fmt`. One space is the floor, not
                // the rule.
                let gap = src[prev.end..t.start].chars().filter(|c| *c == ' ').count().max(1);
                s.push_str(&" ".repeat(gap));
            } else if needs_space(&prev.tok, &t.tok, line, i) {
                // WHERE A SPACE IS REQUIRED, THE AUTHOR MAY USE MORE. Where none is allowed,
                // none is kept — the no-space rules above are absolute.
                //
                // That asymmetry is the whole alignment policy, and it is deliberate:
                // `a  = state` / `sx = b` in bench/kernels/k5_montecarlo.helix is a column of
                // `=` signs somebody lined up on purpose, and 141 lines in this repository do
                // the same with trailing comments. Collapsing those to one space is precisely
                // the complaint this project has about `cargo fmt`, so fmt does not do it.
                // It still fixes `x=1`, `f( a , b )` and `xs [ 0 ]`, because those are gaps
                // where the answer is NO space, or none at all.
                let gap = src[prev.end..t.start].chars().filter(|c| *c == ' ').count().max(1);
                s.push_str(&" ".repeat(gap));
            }
        }
        s.push_str(&src[t.start..t.end]);
    }
    // A line is never allowed to end in whitespace, whatever the rules above produced.
    s.trim_end().to_string()
}

/// Does a space go between these two adjacent tokens?
///
/// Written as "no space when …, otherwise one space", because the exceptions are the short
/// list. `line`/`i` are for the one rule that needs context: telling a unary `-` from a
/// binary one.
fn needs_space(prev: &Tok, next: &Tok, line: &[&Token], i: usize) -> bool {
    use Tok::*;
    // Nothing hugs a `.`, `..` or `...` on either side — `a.b`, `0..n`, `xs...`.
    if matches!(prev, Dot | DotDot | DotDotDot) || matches!(next, Dot | DotDot | DotDotDot) {
        return false;
    }
    // `@col` is one lexical unit to a reader even though it is two tokens.
    if matches!(prev, At) {
        return false;
    }
    // Never after an opener, never before a closer or a separator.
    if matches!(prev, LParen | LBracket) || matches!(next, RParen | RBracket | Comma | Colon) {
        return false;
    }
    // `{` and `}` are records here, and read as units: `{a: 1}`, not `{ a: 1 }`.
    if matches!(prev, LBrace) || matches!(next, RBrace) {
        return false;
    }
    // A call or an index binds tight to what it applies to: `f(x)`, `xs[0]`, `"s".len()`.
    // Stated as "what needs a space before a `(`" rather than "what can precede a call",
    // because the second list is unbounded: `python.import("numpy")` puts a KEYWORD token
    // immediately before the paren, and every keyword usable as a member name would have to
    // be enumerated. Operators and separators are a closed set.
    if matches!(next, LParen | LBracket)
        && !matches!(
            prev,
            Eq | EqEq
                | Ne
                | Lt
                | Gt
                | Le
                | Ge
                | Plus
                | Minus
                | Star
                | StarStar
                | Slash
                | SlashSlash
                | Percent
                | And
                | Or
                | Not
                | Comma
                | Coalesce
                | Amp
                | Caret
                | Shl
                | Shr
                | Pipe
                | Colon
                | Arrow
                | FatArrow
                | If
                | Then
                | Else
                | In
                | Let
                | Match
                | Do
                | Mut
        )
    {
        return false;
    }
    // UNARY minus binds to its operand (`-1`, `f(-x)`, `[-1, -2]`); binary minus does not
    // (`a - b`). It is unary exactly when nothing could have ended an operand before it.
    if matches!(prev, Minus) && is_unary_minus(line, i - 1) {
        return false;
    }
    true
}

/// Is the `Minus` at `at` a unary sign rather than a subtraction? It is unary when the
/// token before it cannot end an operand — an operator, an opener, a comma, or line start.
fn is_unary_minus(line: &[&Token], at: usize) -> bool {
    use Tok::*;
    if at == 0 {
        return true;
    }
    !matches!(
        line[at - 1].tok,
        Ident(_)
            | Int(_)
            | Float(_)
            | BigInt(..)
            | Str(_)
            | InterpStr(_)
            | True
            | False
            | Missing
            | RParen
            | RBracket
            | RBrace
    )
}

/// Does a line STARTING with this token continue the line above? The mirror of
/// [`continues_after_this_line`], and deliberately `cook_newlines`'s `continues_after` list:
/// none of these can begin a statement, so a line that starts with one is a continuation and
/// is indented to say so. This is what keeps a dangling `else` from landing in column 0.
fn continues_before_this_line(t: &Tok) -> bool {
    use Tok::*;
    matches!(
        t,
        Dot | Then
            | Else
            | In
            | Plus
            | Star
            | StarStar
            | Slash
            | SlashSlash
            | Percent
            | EqEq
            | Ne
            | Lt
            | Gt
            | Le
            | Ge
            | And
            | Or
            | Coalesce
            | Amp
            | Caret
            | Pipe
            | Shl
            | Shr
    )
}

/// Does a line ending with this token continue onto the next one? Deliberately the same
/// judgement `cook_newlines`'s `continues_before` makes, since that is what actually decides
/// whether the break is significant — a continuation is indented one step so the layout
/// tells the reader what the lexer already knows.
fn continues_after_this_line(t: &Tok) -> bool {
    use Tok::*;
    matches!(
        t,
        Eq | EqEq
            | Ne
            | Lt
            | Gt
            | Le
            | Ge
            | Plus
            | Minus
            | Star
            | StarStar
            | Slash
            | SlashSlash
            | Percent
            | And
            | Or
            | Not
            | Coalesce
            | Amp
            | Caret
            | Shl
            | Shr
            | Pipe
            | In
            | Then
            | Else
            | Arrow
            | FatArrow
            | Colon
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tracked `.helix` file, formatted, must lex to the SAME tokens — and formatting
    /// twice must equal formatting once.
    ///
    /// This is the whole correctness argument, and it is a static identity rather than a
    /// hope: the formatter only ever changes whitespace, so a difference in the cooked token
    /// stream means a token was created, destroyed, or altered.
    #[test]
    fn formatting_never_changes_the_program() {
        for (path, src) in crate::fmt::tests::tracked_sources() {
            let once = match format_source(&src) {
                Ok(s) => s,
                Err(e) => panic!("{path}: failed to lex: {}", e.message),
            };
            let twice = format_source(&once).expect("formatted output must lex");
            assert_eq!(once, twice, "{path}: fmt is not idempotent");

            let before = crate::lexer::lex(&src).expect("input lexes");
            let after = crate::lexer::lex(&once).expect("output lexes");
            assert_eq!(
                before.len(),
                after.len(),
                "{path}: token COUNT changed ({} -> {})",
                before.len(),
                after.len()
            );
            for (a, b) in before.iter().zip(after.iter()) {
                assert_eq!(
                    format!("{:?}", a.tok),
                    format!("{:?}", b.tok),
                    "{path}: token changed"
                );
            }
        }
    }

    /// Not one byte inside a comment may change, and not one comment may be lost. The token
    /// identity above is completely blind to this — `lex` drops comments — so without this
    /// assertion the formatter could quietly eat documentation and every other gate would
    /// stay green.
    #[test]
    fn formatting_preserves_every_comment_verbatim() {
        for (path, src) in crate::fmt::tests::tracked_sources() {
            let out = format_source(&src).expect("lexes");
            let comments = |s: &str| -> Vec<String> {
                lex_trivia(s)
                    .expect("lexes")
                    .into_iter()
                    .filter_map(|t| match t.tok {
                        Tok::Comment(c) => Some(c),
                        _ => None,
                    })
                    .collect()
            };
            assert_eq!(comments(&src), comments(&out), "{path}: comments changed");
        }
    }

    /// Formatting an already-formatted file is a no-op, and the spacing rules do what they
    /// say. Small, readable cases — the file sweep above proves safety, this proves taste.
    #[test]
    fn spacing_and_indent_rules() {
        let f = |s: &str| format_source(s).expect("lexes");
        assert_eq!(f("x=1"), "x = 1\n");
        // Extra padding where a space is REQUIRED is the author's alignment, and is kept —
        // this is what stops fmt from destroying a column of aligned `=` signs.
        assert_eq!(f("x   =    1"), "x   =    1\n");
        // …but a gap where NO space is allowed goes, however wide. (The gap AFTER the comma
        // survives, because that is a position where a space is required — the rule is
        // uniform rather than carrying a list of "alignable" tokens, and the cost of that
        // simplicity is that odd input stays slightly odd rather than being rewritten.)
        assert_eq!(f("f(  a  ,  b  )"), "f(a,  b)\n");
        assert_eq!(f("f( a , b )"), "f(a, b)\n");
        assert_eq!(f("xs [ 0 ]"), "xs[0]\n");
        assert_eq!(f("a . b . c"), "a.b.c\n");
        assert_eq!(f("{ a : 1 , b : 2 }"), "{a: 1, b: 2}\n");
        assert_eq!(f("print ( - 1 )"), "print(-1)\n");
        assert_eq!(f("a - 1"), "a - 1\n");
        assert_eq!(f("[ - 1 , - 2 ]"), "[-1, -2]\n");
        assert_eq!(f("print(@a > 1)"), "print(@a > 1)\n");
        assert_eq!(f("xs.map(it => it * 2)"), "xs.map(it => it * 2)\n");
        // Nesting indents; a closing bracket rejoins the enclosing level.
        assert_eq!(f("f(\n1,\n2\n)"), "f(\n  1,\n  2\n)\n");
        // A trailing operator marks a continuation, which gets one extra step.
        assert_eq!(f("x = 1 +\n2"), "x = 1 +\n  2\n");
        // Blank runs collapse to one; leading and trailing blanks go entirely.
        assert_eq!(f("\n\n\nx = 1\n\n\n\ny = 2\n\n\n"), "x = 1\n\ny = 2\n");
        // A file with no trailing newline gets one; an empty file is one newline.
        assert_eq!(f("x = 1"), "x = 1\n");
        assert_eq!(f(""), "\n");
        // The gap before a trailing comment is the author's, at least one space.
        assert_eq!(f("x = 1     # note"), "x = 1     # note\n");
        assert_eq!(f("x = 1#note"), "x = 1 #note\n");
        // A whole-line comment takes its statement's indent and is otherwise untouched.
        assert_eq!(f("f(\n# why\n1\n)"), "f(\n  # why\n  1\n)\n");
        // A body whose closers sit at the END of a wrapped line (a nested lambda wrapped
        // across lines) ends its indent step THERE: the following column-0 comment and
        // `fn` stay at column 0. The dead step used to survive — only leading closers
        // unwound — and the next flush-left line popped it and inherited its indent, so
        // the comment came out MORE indented than the function it documented (physics
        // field report, v0.2.2).
        assert_eq!(
            f("fn a(xs) =\n  xs.map(i =>\n    i + 1)\n\n# note\nfn b(x) = x\n"),
            "fn a(xs) =\n  xs.map(i =>\n    i + 1)\n\n# note\nfn b(x) = x\n"
        );
    }

    /// A file that does not PARSE still formats — the moment a formatter is most wanted.
    #[test]
    fn formats_what_lexes_even_if_it_does_not_parse() {
        // Unbalanced, and meaningless to the parser; it still lexes, so it still formats.
        let out = format_source("f( a ,").expect("lexes");
        assert_eq!(out, "f(a,\n");
        assert!(crate::parser::parse(crate::lexer::lex("f( a ,").unwrap()).is_err());
        // But a LEX error is still an error — there is nothing to be faithful to.
        assert!(format_source("x = 0x10").is_err());
    }

    /// Every tracked `.helix` outside `tests/corpus/` (whose fixtures deliberately do not
    /// compile, and several of which do not lex).
    pub(super) fn tracked_sources() -> Vec<(String, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if p.is_dir() {
                    // `.claude` holds other sessions' git worktrees — a copy of this repo,
                    // which would double every file and (worse) test whatever state that
                    // worktree happens to be in.
                    if !matches!(
                        name,
                        "target" | ".git" | "node_modules" | "corpus" | ".next" | ".claude"
                    ) {
                        stack.push(p);
                    }
                } else if p.extension().and_then(|s| s.to_str()) == Some("helix")
                    && let Ok(src) = std::fs::read_to_string(&p)
                {
                    out.push((p.display().to_string(), src));
                }
            }
        }
        assert!(out.len() > 40, "expected the repo's .helix corpus, found {}", out.len());
        out
    }
}

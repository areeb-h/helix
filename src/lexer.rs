//! Lexer: source text -> tokens.
//!
//! Newlines are significant (they end statements), but with one crucial twist
//! that makes Helix's dot-chains read cleanly across lines: a newline is
//! *suppressed* whenever the code clearly isn't finished yet — e.g. the next
//! non-blank token is a leading `.`, or the previous token was an operator,
//! comma, or open bracket. This is what lets
//!
//!     patients
//!         .where(age > 40)
//!         .select(name)
//!
//! parse as a single statement with no line-continuation characters.

use crate::error::HelixError;
use crate::token::{StrSeg, Tok, Token};

pub fn lex(src: &str) -> Result<Vec<Token>, HelixError> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut line = 1;
    let mut col = 1;
    let mut raw: Vec<Token> = Vec::new();

    macro_rules! push {
        ($t:expr, $c:expr) => {{
            raw.push(Token {
                tok: $t,
                line,
                col: $c,
            });
        }};
    }

    while i < n {
        let c = chars[i];
        let start_col = col;
        match c {
            ' ' | '\t' | '\r' => {
                i += 1;
                col += 1;
            }
            '\n' => {
                push!(Tok::Newline, start_col);
                i += 1;
                line += 1;
                col = 1;
            }
            '#' => {
                // comment to end of line
                while i < n && chars[i] != '\n' {
                    i += 1;
                    col += 1;
                }
            }
            '"' => {
                let (segs, used, newlines, end_col) = lex_string(&chars, i, line, col)?;
                // Plain string if there are no `{expr}` interpolations.
                let tok = if segs.iter().any(|s| matches!(s, StrSeg::Expr(_))) {
                    Tok::InterpStr(segs)
                } else {
                    let mut s = String::new();
                    for seg in &segs {
                        if let StrSeg::Lit(t) = seg {
                            s.push_str(t);
                        }
                    }
                    Tok::Str(s)
                };
                push!(tok, start_col);
                i += used;
                if newlines > 0 {
                    line += newlines;
                    col = end_col;
                } else {
                    col += used;
                }
            }
            c if c.is_ascii_digit() => {
                let (tok, used) = lex_number(&chars, i);
                push!(tok, start_col);
                i += used;
                col += used;
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut j = i;
                while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let word: String = chars[i..j].iter().collect();
                let tok = match word.as_str() {
                    "mut" => Tok::Mut,
                    "fn" => Tok::Fn,
                    "import" => Tok::Import,
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    "if" => Tok::If,
                    "then" => Tok::Then,
                    "else" => Tok::Else,
                    "let" => Tok::Let,
                    "in" => Tok::In,
                    "missing" => Tok::Missing,
                    "try" => Tok::Try,
                    "match" => Tok::Match,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    _ => Tok::Ident(word),
                };
                push!(tok, start_col);
                let used = j - i;
                i += used;
                col += used;
            }
            '=' => {
                if i + 1 < n && chars[i + 1] == '=' {
                    push!(Tok::EqEq, start_col);
                    i += 2;
                    col += 2;
                } else if i + 1 < n && chars[i + 1] == '>' {
                    push!(Tok::FatArrow, start_col);
                    i += 2;
                    col += 2;
                } else {
                    push!(Tok::Eq, start_col);
                    i += 1;
                    col += 1;
                }
            }
            '!' => {
                if i + 1 < n && chars[i + 1] == '=' {
                    push!(Tok::Ne, start_col);
                    i += 2;
                    col += 2;
                } else {
                    return Err(HelixError::new("unexpected `!`", line, col)
                        .hint("Helix uses the word `not` for negation, e.g. `not done`."));
                }
            }
            '<' => two_or_one(&chars, i, '=', Tok::Le, Tok::Lt, &mut raw, line, start_col, &mut i, &mut col),
            '>' => two_or_one(&chars, i, '=', Tok::Ge, Tok::Gt, &mut raw, line, start_col, &mut i, &mut col),
            '+' => single(Tok::Plus, &mut raw, line, start_col, &mut i, &mut col),
            '-' => {
                if i + 1 < n && chars[i + 1] == '>' {
                    push!(Tok::Arrow, start_col);
                    i += 2;
                    col += 2;
                } else {
                    push!(Tok::Minus, start_col);
                    i += 1;
                    col += 1;
                }
            }
            '*' => {
                if i + 1 < n && chars[i + 1] == '*' {
                    push!(Tok::StarStar, start_col);
                    i += 2;
                    col += 2;
                } else {
                    push!(Tok::Star, start_col);
                    i += 1;
                    col += 1;
                }
            }
            '/' => single(Tok::Slash, &mut raw, line, start_col, &mut i, &mut col),
            '%' => single(Tok::Percent, &mut raw, line, start_col, &mut i, &mut col),
            '(' => single(Tok::LParen, &mut raw, line, start_col, &mut i, &mut col),
            ')' => single(Tok::RParen, &mut raw, line, start_col, &mut i, &mut col),
            '[' => single(Tok::LBracket, &mut raw, line, start_col, &mut i, &mut col),
            ']' => single(Tok::RBracket, &mut raw, line, start_col, &mut i, &mut col),
            '{' => single(Tok::LBrace, &mut raw, line, start_col, &mut i, &mut col),
            '}' => single(Tok::RBrace, &mut raw, line, start_col, &mut i, &mut col),
            ',' => single(Tok::Comma, &mut raw, line, start_col, &mut i, &mut col),
            '.' => {
                if i + 1 < n && chars[i + 1] == '.' {
                    push!(Tok::DotDot, start_col);
                    i += 2;
                    col += 2;
                } else {
                    push!(Tok::Dot, start_col);
                    i += 1;
                    col += 1;
                }
            }
            ':' => single(Tok::Colon, &mut raw, line, start_col, &mut i, &mut col),
            '|' => single(Tok::Pipe, &mut raw, line, start_col, &mut i, &mut col),
            '@' => single(Tok::At, &mut raw, line, start_col, &mut i, &mut col),
            '?' => {
                if i + 1 < n && chars[i + 1] == '?' {
                    push!(Tok::Coalesce, start_col);
                    i += 2;
                    col += 2;
                } else {
                    return Err(HelixError::new("unexpected `?`", line, col)
                        .hint("did you mean `??` (use a default when a value is `missing`)?"));
                }
            }
            other => {
                let mut err =
                    HelixError::new(format!("unexpected character `{}`", other), line, col);
                if !other.is_ascii() {
                    // Non-ASCII is fine inside strings and comments — only identifiers
                    // and operators are ASCII. (A common cause is an editor/shell
                    // turning `-`/`+-` into a fancy `—`/`±`.)
                    err = err.hint(
                        "Helix identifiers and operators are ASCII; put non-ASCII text inside a string or comment.",
                    );
                }
                return Err(err);
            }
        }
    }

    raw.push(Token {
        tok: Tok::Eof,
        line,
        col,
    });

    Ok(cook_newlines(raw))
}

fn single(
    tok: Tok,
    raw: &mut Vec<Token>,
    line: usize,
    col: usize,
    i: &mut usize,
    c: &mut usize,
) {
    raw.push(Token { tok, line, col });
    *i += 1;
    *c += 1;
}

#[allow(clippy::too_many_arguments)]
fn two_or_one(
    chars: &[char],
    i: usize,
    second: char,
    two: Tok,
    one: Tok,
    raw: &mut Vec<Token>,
    line: usize,
    col: usize,
    ip: &mut usize,
    cp: &mut usize,
) {
    if i + 1 < chars.len() && chars[i + 1] == second {
        raw.push(Token { tok: two, line, col });
        *ip += 2;
        *cp += 2;
    } else {
        raw.push(Token { tok: one, line, col });
        *ip += 1;
        *cp += 1;
    }
}

fn lex_number(chars: &[char], start: usize) -> (Tok, usize) {
    let n = chars.len();
    let mut j = start;
    while j < n && chars[j].is_ascii_digit() {
        j += 1;
    }
    // A float needs a `.` followed by a digit, so `3.mean()` stays Int(3) `.` mean.
    let mut is_float = false;
    if j + 1 < n && chars[j] == '.' && chars[j + 1].is_ascii_digit() {
        is_float = true;
        j += 1;
        while j < n && chars[j].is_ascii_digit() {
            j += 1;
        }
    }
    // Optional scientific exponent: `e`/`E` with an optional sign and digits
    // (`1e9`, `2.5e-3`, `4E10`) — makes it a float. Requires a digit after, so a
    // bare `e` stays a separate identifier (e.g. `3.e` is `3` `.` `e`).
    if j < n && (chars[j] == 'e' || chars[j] == 'E') {
        let mut t = j + 1;
        if t < n && (chars[t] == '+' || chars[t] == '-') {
            t += 1;
        }
        if t < n && chars[t].is_ascii_digit() {
            is_float = true;
            j = t + 1;
            while j < n && chars[j].is_ascii_digit() {
                j += 1;
            }
        }
    }
    let text: String = chars[start..j].iter().collect();
    let tok = if is_float {
        Tok::Float(text.parse().unwrap_or(0.0))
    } else {
        // An integer literal too large for i64 degrades to its float value
        // (its actual magnitude) rather than silently becoming 0.
        match text.parse::<i64>() {
            Ok(i) => Tok::Int(i),
            Err(_) => Tok::Float(text.parse::<f64>().unwrap_or(0.0)),
        }
    };
    (tok, j - start)
}

/// Lex a `"..."` into segments, recognizing `{expr}` interpolations (`{{`/`}}`
/// escape literal braces). Returns (segments, chars_consumed_incl_quotes,
/// newlines_inside, end_col).
fn lex_string(
    chars: &[char],
    start: usize,
    line: usize,
    col: usize,
) -> Result<(Vec<StrSeg>, usize, usize, usize), HelixError> {
    let n = chars.len();
    let mut j = start + 1; // skip opening quote
    let mut newlines = 0;
    let mut end_col = col + 1;
    let mut segs: Vec<StrSeg> = Vec::new();
    let mut lit = String::new();
    macro_rules! flush_lit {
        () => {
            if !lit.is_empty() {
                segs.push(StrSeg::Lit(std::mem::take(&mut lit)));
            }
        };
    }
    while j < n {
        let c = chars[j];
        match c {
            '"' => {
                flush_lit!();
                if segs.is_empty() {
                    segs.push(StrSeg::Lit(String::new()));
                }
                return Ok((segs, j - start + 1, newlines, end_col + 1));
            }
            '\\' => {
                j += 1;
                end_col += 1;
                if j >= n {
                    break;
                }
                lit.push(match chars[j] {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    '"' => '"',
                    '0' => '\0',
                    '{' => '{',
                    '}' => '}',
                    other => other,
                });
                j += 1;
                end_col += 1;
            }
            '{' if j + 1 < n && chars[j + 1] == '{' => {
                lit.push('{');
                j += 2;
                end_col += 2;
            }
            '}' if j + 1 < n && chars[j + 1] == '}' => {
                lit.push('}');
                j += 2;
                end_col += 2;
            }
            '{' => {
                // Start of an interpolation — scan to the matching top-level `}`,
                // building the embedded expression source as we go. Because the
                // expression lives inside the outer `"..."`, any string literal in
                // it is written with escaped quotes (`\"`). We un-escape on the
                // fly so the sub-expression re-lexes as valid Helix, and we track
                // nested-string + bracket state so a `}` (or `)`/`]`) *inside* a
                // string literal or sub-expression doesn't end the interpolation.
                flush_lit!();
                j += 1;
                end_col += 1;
                let mut expr = String::new();
                let mut depth = 0i32;
                let mut in_str = false;
                loop {
                    if j >= n {
                        return Err(HelixError::new("unterminated `{` interpolation", line, col)
                            .hint("close the `{...}` with a `}`."));
                    }
                    let e = chars[j];
                    // An outer-string escape: un-escape into the expression. `\"`
                    // is both an un-escaped quote and a nested-string delimiter.
                    if e == '\\' && j + 1 < n {
                        let nx = chars[j + 1];
                        let un = match nx {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '\\' => '\\',
                            '"' => '"',
                            '0' => '\0',
                            '{' => '{',
                            '}' => '}',
                            other => other,
                        };
                        if nx == '"' {
                            in_str = !in_str;
                        }
                        expr.push(un);
                        j += 2;
                        end_col += 2;
                        continue;
                    }
                    if !in_str {
                        match e {
                            '"' => in_str = true,
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' => depth -= 1,
                            '}' if depth == 0 => break,
                            '}' => depth -= 1,
                            _ => {}
                        }
                    } else if e == '"' {
                        in_str = false;
                    }
                    expr.push(e);
                    if e == '\n' {
                        newlines += 1;
                        end_col = 1;
                    } else {
                        end_col += 1;
                    }
                    j += 1;
                }
                if expr.trim().is_empty() {
                    return Err(HelixError::new("empty `{}` interpolation", line, col)
                        .hint("put an expression inside, e.g. `\"hi {name}\"`."));
                }
                segs.push(StrSeg::Expr(expr));
                j += 1; // skip closing `}`
                end_col += 1;
            }
            '\n' => {
                lit.push('\n');
                j += 1;
                newlines += 1;
                end_col = 1;
            }
            _ => {
                lit.push(c);
                j += 1;
                end_col += 1;
            }
        }
    }
    Err(HelixError::new("unterminated string literal", line, col)
        .hint("add a closing `\"` to end the string."))
}

/// Decide which newline tokens are real statement terminators and which are
/// mid-expression continuations to be dropped.
fn cook_newlines(raw: Vec<Token>) -> Vec<Token> {
    fn continues_before(t: &Tok) -> bool {
        matches!(
            t,
            Tok::Eq | Tok::EqEq | Tok::Ne | Tok::Lt | Tok::Gt | Tok::Le | Tok::Ge
                | Tok::Plus | Tok::Minus | Tok::Star | Tok::StarStar | Tok::Slash | Tok::Percent
                | Tok::And | Tok::Or | Tok::Not | Tok::Comma | Tok::Dot
                | Tok::LParen | Tok::LBracket | Tok::LBrace | Tok::Mut | Tok::FatArrow
                | Tok::Colon | Tok::Arrow | Tok::Coalesce
                // A line ending in `in` (the `let … in` separator) or a branch keyword
                // is unfinished — let its body/branch start on the next line.
                | Tok::In | Tok::Then | Tok::Else
        )
    }
    fn continues_after(t: &Tok) -> bool {
        // A line that's followed by `.`/`)`/`]`, or a `then`/`else` branch, is
        // clearly unfinished — drop the intervening newline.
        matches!(
            t,
            Tok::Dot | Tok::RParen | Tok::RBracket | Tok::RBrace | Tok::Then | Tok::Else | Tok::In
        )
    }

    let mut out: Vec<Token> = Vec::with_capacity(raw.len());
    for (idx, t) in raw.iter().enumerate() {
        if t.tok != Tok::Newline {
            out.push(t.clone());
            continue;
        }
        // previous kept token
        let prev = out.last().map(|x| &x.tok);
        // next non-newline token
        let next = raw[idx + 1..]
            .iter()
            .map(|x| &x.tok)
            .find(|x| **x != Tok::Newline);

        let drop = match (prev, next) {
            (None, _) => true,                                   // leading blank lines
            (Some(p), _) if continues_before(p) => true,         // line ends mid-expression
            (_, Some(nx)) if continues_after(nx) => true,        // next line starts with `.`/`)`/`]`
            (_, Some(Tok::Eof)) => true,
            (Some(Tok::Newline), _) => true,                     // collapse runs
            _ => false,
        };
        if !drop {
            out.push(t.clone());
        }
    }
    out
}

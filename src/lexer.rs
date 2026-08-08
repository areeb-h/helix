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
                // A triple-quoted RAW string `"""…"""` is literal — no `{…}` interpolation
                // and no `\` escapes — so braces (CSS/JSON), backslashes (regex/paths) and
                // quotes go in verbatim. This is the fix for the brace-doubling wart.
                if i + 2 < n && chars[i + 1] == '"' && chars[i + 2] == '"' {
                    let (s, used, newlines, end_col) = lex_raw_string(&chars, i, line, col)?;
                    push!(Tok::Str(s), start_col);
                    i += used;
                    if newlines > 0 {
                        line += newlines;
                        col = end_col;
                    } else {
                        col += used;
                    }
                } else {
                    let (segs, used, newlines, end_col) =
                        lex_string(&chars, i, line, col, '"', false)?;
                    // Plain string if there are no `{expr}` interpolations.
                    let tok = if segs.iter().any(|s| matches!(s, StrSeg::Expr(..))) {
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
            }
            // `'…'` is the same string as `"…"`, interpolation and all — an alternate
            // delimiter so the other quote can appear inside without escaping, which is
            // what kills the `\"` pile-up in a conditional inside a hole. `'''…'''` is the
            // INTERPOLATING multi-line form; `"""…"""` above stays RAW, so the two triples
            // divide the work rather than competing.
            '\'' => {
                let triple = i + 2 < n && chars[i + 1] == '\'' && chars[i + 2] == '\'';
                let (segs, used, newlines, end_col) =
                    lex_string(&chars, i, line, col, '\'', triple)?;
                let tok = if segs.iter().any(|s| matches!(s, StrSeg::Expr(..))) {
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
                // Two classic literal traps that used to split into two tokens
                // and die later with a baffling statement-boundary error:
                // `0x10` (no hex/binary/octal literals) and `1_000` (no digit
                // separators). Catch them here with a targeted message.
                let after = i + used;
                if after < n {
                    let nx = chars[after];
                    if used == 1 && c == '0' && matches!(nx, 'x' | 'X' | 'b' | 'B' | 'o' | 'O')
                        && after + 1 < n
                        && chars[after + 1].is_ascii_alphanumeric()
                    {
                        return Err(HelixError::new(
                            format!("Helix has no `0{}…` literals", nx),
                            line,
                            start_col,
                        )
                        .hint("write the plain decimal value (e.g. `16` instead of `0x10`)."));
                    }
                    if nx == '_' && after + 1 < n && chars[after + 1].is_ascii_digit() {
                        return Err(HelixError::new(
                            "digits cannot contain `_` separators",
                            line,
                            start_col,
                        )
                        .hint("write the digits without separators (e.g. `1000`)."));
                    }
                }
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
                    "do" => Tok::Do,
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
            '<' => {
                if i + 1 < n && chars[i + 1] == '<' {
                    push!(Tok::Shl, start_col);
                    i += 2;
                    col += 2;
                } else {
                    two_or_one(&chars, i, '=', Tok::Le, Tok::Lt, &mut raw, line, start_col, &mut i, &mut col);
                }
            }
            '>' => {
                if i + 1 < n && chars[i + 1] == '>' {
                    push!(Tok::Shr, start_col);
                    i += 2;
                    col += 2;
                } else {
                    two_or_one(&chars, i, '=', Tok::Ge, Tok::Gt, &mut raw, line, start_col, &mut i, &mut col);
                }
            }
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
            '/' => {
                if i + 1 < n && chars[i + 1] == '/' {
                    push!(Tok::SlashSlash, start_col);
                    i += 2;
                    col += 2;
                } else {
                    single(Tok::Slash, &mut raw, line, start_col, &mut i, &mut col);
                }
            }
            '%' => single(Tok::Percent, &mut raw, line, start_col, &mut i, &mut col),
            '(' => single(Tok::LParen, &mut raw, line, start_col, &mut i, &mut col),
            ')' => single(Tok::RParen, &mut raw, line, start_col, &mut i, &mut col),
            '[' => single(Tok::LBracket, &mut raw, line, start_col, &mut i, &mut col),
            ']' => single(Tok::RBracket, &mut raw, line, start_col, &mut i, &mut col),
            '{' => single(Tok::LBrace, &mut raw, line, start_col, &mut i, &mut col),
            '}' => single(Tok::RBrace, &mut raw, line, start_col, &mut i, &mut col),
            ',' => single(Tok::Comma, &mut raw, line, start_col, &mut i, &mut col),
            '.' => {
                if i + 2 < n && chars[i + 1] == '.' && chars[i + 2] == '.' {
                    push!(Tok::DotDotDot, start_col);
                    i += 3;
                    col += 3;
                } else if i + 1 < n && chars[i + 1] == '.' {
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
            '&' => single(Tok::Amp, &mut raw, line, start_col, &mut i, &mut col),
            '^' => single(Tok::Caret, &mut raw, line, start_col, &mut i, &mut col),
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
            // A UTF-8 byte-order mark (what Windows Notepad / PowerShell `Out-File`
            // prepend) is invisible — skip a leading one instead of failing every
            // line-1 program with an "unexpected character" whose caret renders
            // nothing.
            // Invisible, so the column does not advance: the first real token
            // still reports column 1.
            '\u{FEFF}' if i == 0 => {
                i += 1;
            }
            ';' => {
                // The parser's statement-boundary error carries this hint, but a
                // typed `;` dies here at lex time and never reaches it.
                return Err(HelixError::new("unexpected character `;`", line, col)
                    .hint("each statement goes on its own line; Helix has no `;`."));
            }
            other => {
                let mut err =
                    HelixError::new(format!("unexpected character `{}`", other), line, col);
                if !other.is_ascii() {
                    // Non-ASCII is fine inside strings, comments, and identifiers
                    // (identifiers are Unicode-alphabetic: `π = 3.14` lexes) — only
                    // OPERATORS are ASCII. (A common cause is an editor/shell
                    // turning `-`/`+-` into a fancy `—`/`±`.)
                    err = err.hint(
                        "Helix operators are ASCII; identifiers may use letters, but symbols belong inside a string or comment.",
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
        // (its actual magnitude) rather than silently becoming 0. It keeps a DISTINCT
        // token so the parser can still recognise `-9223372036854775808` as `i64::MIN`,
        // whose magnitude is one larger than `i64::MAX` and so cannot be written
        // positively. See `Tok::BigInt`.
        match text.parse::<i64>() {
            Ok(i) => Tok::Int(i),
            Err(_) => {
                // Parsed as i128 so the comparison is on the DIGITS: an f64 test would
                // accept 9223372036854775809, which rounds to the same 2^63.
                let is_min_magnitude =
                    text.parse::<i128>().is_ok_and(|v| v == i64::MAX as i128 + 1);
                Tok::BigInt(text.parse::<f64>().unwrap_or(0.0), is_min_magnitude)
            }
        }
    };
    (tok, j - start)
}

/// Lex a triple-quoted RAW string `"""…"""`: every character up to the closing `"""`
/// is literal — no `{…}` interpolation, no `\` escape processing — so CSS braces, JSON,
/// regex backslashes, Windows paths, and prose all go in verbatim. May span lines and
/// contain single or double quotes (anything but three in a row). Returns
/// `(content, chars_consumed, newlines_inside, end_col)` to match [`lex_string`].
fn lex_raw_string(
    chars: &[char],
    start: usize,
    line: usize,
    col: usize,
) -> Result<(String, usize, usize, usize), HelixError> {
    let n = chars.len();
    let body = start + 3; // past the opening `"""`
    let mut j = body;
    let mut newlines = 0usize;
    // Index of column 1 on `start`'s line, for computing the end column after newlines.
    let mut line_start = (start + 1).saturating_sub(col);
    let mut close = None;
    while j < n {
        if chars[j] == '"' && j + 2 < n && chars[j + 1] == '"' && chars[j + 2] == '"' {
            close = Some(j);
            break;
        }
        if chars[j] == '\n' {
            newlines += 1;
            line_start = j + 1;
        }
        j += 1;
    }
    let close = close.ok_or_else(|| {
        HelixError::new("unterminated `\"\"\"` raw string", line, col).hint("close it with `\"\"\"`.")
    })?;
    let content: String = chars[body..close].iter().collect();
    let used = (close + 3) - start;
    let end_col = (close + 3) - line_start + 1;
    Ok((content, used, newlines, end_col))
}

/// Lex an interpolating string into segments, recognizing `{expr}` interpolations
/// (`{{`/`}}` escape literal braces). Returns (segments, chars_consumed_incl_quotes,
/// newlines_inside, end_col).
///
/// `quote` is the delimiter — `"` or `'`, which behave IDENTICALLY, so that whichever one
/// is not being used as the delimiter can appear inside without escaping. That is the whole
/// point of having two: a conditional inside a hole reads
/// `"-> {if ok then 'YES' else 'NO'}"` instead of drowning in `\"`.
///
/// `triple` selects the three-character delimiter (`'''…'''`), which additionally spans
/// lines. Note the asymmetry with `"""…"""`, which is RAW and lexed by [`lex_raw_string`]:
/// raw is the right default for a triple-quoted block holding CSS, JSON, regexes or
/// Windows paths, so `"""` keeps it, and `'''` is the interpolating multi-line form. Two
/// delimiters, two jobs, and neither had to change meaning.
fn lex_string(
    chars: &[char],
    start: usize,
    line: usize,
    col: usize,
    quote: char,
    triple: bool,
) -> Result<(Vec<StrSeg>, usize, usize, usize), HelixError> {
    let n = chars.len();
    let delim = if triple { 3 } else { 1 };
    // True when `j` sits on a closing delimiter (all three characters, for a triple).
    let closes = |chars: &[char], j: usize| -> bool {
        chars[j] == quote
            && (!triple || (j + 2 < chars.len() && chars[j + 1] == quote && chars[j + 2] == quote))
    };
    let mut j = start + delim; // skip opening quote(s)
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
            // A lone `'` inside a `'''` block is literal — only three in a row close it —
            // so this guard, not a bare `quote` arm, is what makes the triple form work.
            _ if closes(chars, j) => {
                flush_lit!();
                if segs.is_empty() {
                    segs.push(StrSeg::Lit(String::new()));
                }
                return Ok((segs, j - start + delim, newlines, end_col + delim));
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
                    // BOTH delimiters are escapable in EITHER kind of string. Escaping the
                    // one you are not delimited by is redundant, not wrong, and accepting
                    // it means `\"` keeps working in every string it works in today.
                    '"' => '"',
                    '\'' => '\'',
                    '0' => '\0',
                    '{' => '{',
                    '}' => '}',
                    // An unknown escape used to be silently swallowed (`\q` → `q`;
                    // `\u{0041}` degraded to a literal `u` plus an interpolated
                    // `{0041}`) — a silent-mangling trap. Reject it instead.
                    other => {
                        return Err(HelixError::new(
                            format!("unknown string escape `\\{}`", other),
                            line,
                            col,
                        )
                        .hint("supported escapes: \\n \\t \\r \\\\ \\\" \\0 \\{ \\} — strings are UTF-8, paste unicode directly; for literal backslashes use a raw \"\"\"...\"\"\" string."));
                    }
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
                // The optional format spec after a *top-level* `:` (e.g. `.2f` in
                // `{x:.2f}`). `None` until/unless such a `:` is seen.
                let mut spec: Option<String> = None;
                let mut depth = 0i32;
                // WHICH quote opened the nested string, not merely whether one is open:
                // with two delimiters a `bool` would treat `'` as ordinary inside a hole,
                // so `"{f('}')}"` would end the hole at the `}` inside the sub-string.
                let mut in_str: Option<char> = None;
                // Push a char into the spec buffer if we're past the `:`, else the expr.
                macro_rules! push_char {
                    ($ch:expr) => {
                        match &mut spec {
                            Some(sp) => sp.push($ch),
                            None => expr.push($ch),
                        }
                    };
                }
                loop {
                    if j >= n {
                        return Err(HelixError::new("unterminated `{` interpolation", line, col)
                            .hint("close the hole with `}` (e.g. `\"hi {name}\"`), or write `{{` for a literal `{` (e.g. `\"{{\"`)."));
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
                            '\'' => '\'',
                            '0' => '\0',
                            '{' => '{',
                            '}' => '}',
                            // Same rejection as the outer-string scanner above.
                            other => {
                                return Err(HelixError::new(
                                    format!("unknown string escape `\\{}`", other),
                                    line,
                                    col,
                                )
                                .hint("supported escapes: \\n \\t \\r \\\\ \\\" \\0 \\{ \\} — strings are UTF-8, paste unicode directly; for literal backslashes use a raw \"\"\"...\"\"\" string."));
                            }
                        };
                        if nx == '"' || nx == '\'' {
                            in_str = match in_str {
                                None => Some(nx),
                                Some(q) if q == nx => None,
                                open => open,
                            };
                        }
                        push_char!(un);
                        j += 2;
                        end_col += 2;
                        continue;
                    }
                    if in_str.is_none() {
                        match e {
                            '"' | '\'' => in_str = Some(e),
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' => depth -= 1,
                            '}' if depth == 0 => break,
                            '}' => depth -= 1,
                            // A top-level `:` separates the expression from its format
                            // spec. Nested `:` (record literal `{a: 1}`, slice `xs[1:3]`)
                            // sit at depth > 0, and a `:` inside a string is `in_str`, so
                            // neither is mistaken for a spec.
                            ':' if depth == 0 && spec.is_none() => {
                                spec = Some(String::new());
                                j += 1;
                                end_col += 1;
                                continue;
                            }
                            _ => {}
                        }
                    } else if Some(e) == in_str {
                        in_str = None;
                    }
                    push_char!(e);
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
                        .hint("put an expression inside (e.g. `\"hi {name}\"`), or write `{{}}` for literal braces."));
                }
                segs.push(StrSeg::Expr(expr, spec));
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
    // Name the delimiter the string actually opened with — reporting `"` for an
    // unterminated `'…'` sends the reader hunting for the wrong character.
    let d: String = std::iter::repeat_n(quote, delim).collect();
    Err(HelixError::new("unterminated string literal", line, col)
        .hint(format!("add a closing `{d}` to end the string.")))
}

/// Decide which newline tokens are real statement terminators and which are
/// mid-expression continuations to be dropped.
fn cook_newlines(raw: Vec<Token>) -> Vec<Token> {
    fn continues_before(t: &Tok) -> bool {
        matches!(
            t,
            Tok::Eq | Tok::EqEq | Tok::Ne | Tok::Lt | Tok::Gt | Tok::Le | Tok::Ge
                | Tok::Plus | Tok::Minus | Tok::Star | Tok::StarStar | Tok::Slash | Tok::SlashSlash | Tok::Percent
                | Tok::And | Tok::Or | Tok::Not | Tok::Comma | Tok::Dot
                | Tok::LParen | Tok::LBracket | Tok::LBrace | Tok::Mut | Tok::FatArrow
                | Tok::Colon | Tok::Arrow | Tok::Coalesce
                | Tok::Amp | Tok::Caret | Tok::Shl | Tok::Shr | Tok::Pipe
                // A line ending in `in` (the `let … in` separator) or a branch keyword
                // is unfinished — let its body/branch start on the next line.
                | Tok::In | Tok::Then | Tok::Else
        )
    }
    fn continues_after(t: &Tok) -> bool {
        // A line that's followed by `.`/`)`/`]`, or a `then`/`else` branch, is
        // clearly unfinished — drop the intervening newline.
        //
        // The infix operators below extend this to leading-operator continuation
        // (`x = a\n  + b\n  + c`). This is purely additive: none of them can legally
        // *begin* an expression, so a line that starts with one is otherwise a parse
        // error — no currently-valid program changes meaning. `-` and `not` are
        // deliberately excluded: they are valid unary prefixes, so a line starting
        // with one is a real new statement, not a continuation.
        matches!(
            t,
            Tok::Dot | Tok::RParen | Tok::RBracket | Tok::RBrace | Tok::Then | Tok::Else | Tok::In
                | Tok::Plus | Tok::Star | Tok::StarStar | Tok::Slash | Tok::SlashSlash | Tok::Percent
                | Tok::EqEq | Tok::Ne | Tok::Lt | Tok::Gt | Tok::Le | Tok::Ge
                | Tok::And | Tok::Or | Tok::Coalesce
                | Tok::Amp | Tok::Caret | Tok::Pipe | Tok::Shl | Tok::Shr
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

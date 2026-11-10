//! Markup in Helix source (ADR 0061): `<li class="row">{item.name}</li>`.
//!
//! The lexer hands this module the source at a `<` that starts an element, and it reads the
//! whole element into a [`MNode`] tree: tags, attributes, text and `{…}` holes, each with its
//! line and column. The parser then turns the tree into the same view data `ui.el` builds,
//! so no engine knows markup exists.
//!
//! A hole's source is only FOUND here, never parsed: the parser parses it as an ordinary
//! expression, from its own position, so an error inside a hole points at the hole. Finding
//! where a hole ends means skipping what can hold a `}` that does not close it: strings,
//! comments, nested brackets and nested markup.

use crate::error::HelixError;

/// One node of markup, as written.
#[derive(Debug, Clone, PartialEq)]
pub enum MNode {
    /// `<tag …>children</tag>` or `<tag … />`.
    Element { tag: String, attrs: Vec<MAttr>, children: Vec<MNode>, line: usize, col: usize },
    /// `<>children</>`.
    Fragment { children: Vec<MNode>, line: usize, col: usize },
    /// Text between tags, with JSX's whitespace rules applied and entities decoded.
    Text(String),
    /// `{expr}` among the children: the expression's source and where it starts.
    Hole(Src),
}

/// A piece of Helix source inside markup, and where its first character is.
#[derive(Debug, Clone, PartialEq)]
pub struct Src {
    pub text: String,
    pub line: usize,
    pub col: usize,
}

/// One attribute of an element.
#[derive(Debug, Clone, PartialEq)]
pub struct MAttr {
    /// The name as written; empty for a spread.
    pub name: String,
    pub value: MValue,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MValue {
    /// A bare attribute: `disabled`.
    Flag,
    /// `name="…"`: a Helix string literal (so `"{n} items"` interpolates), as source.
    Str(Src),
    /// `name={expr}`.
    Expr(Src),
    /// `{...expr}`: a record whose fields are props.
    Spread(Src),
}

/// Whether a `<` at `i` starts markup rather than comparing: it must be followed by a letter
/// (a tag) or `>` (a fragment). The caller decides from the token before it whether a value
/// ended there, in which case `<` compares.
pub fn opens(chars: &[char], i: usize) -> bool {
    chars.get(i) == Some(&'<') && chars.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == '>')
}

/// Words after which a `<` starts markup even though a word ended there.
const KEYWORDS: [&str; 13] = ["mut", "fn", "import", "and", "or", "not", "if", "then", "else", "let", "in", "match", "do"];

struct Scan<'a> {
    chars: &'a [char],
    i: usize,
    line: usize,
    col: usize,
}

/// Read the element (or fragment) at `i`. Returns it and the position just past it.
pub fn scan(chars: &[char], i: usize, line: usize, col: usize) -> Result<(MNode, usize, usize, usize), HelixError> {
    let mut s = Scan { chars, i, line, col };
    let node = s.element(0)?;
    Ok((node, s.i, s.line, s.col))
}

/// How deeply markup may nest before it is refused, so a pathological file cannot overflow
/// the stack (the parser bounds its own recursion the same way).
const MAX_DEPTH: usize = 400;

impl Scan<'_> {
    fn peek(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).copied()
    }

    fn bump(&mut self) {
        if let Some(c) = self.peek(0) {
            self.i += 1;
            if c == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
        }
    }

    fn bump_n(&mut self, n: usize) {
        for _ in 0..n {
            self.bump();
        }
    }

    fn err(&self, msg: impl Into<String>) -> HelixError {
        HelixError::new(msg, self.line, self.col)
    }

    fn skip_space(&mut self) {
        while let Some(c) = self.peek(0) {
            if c.is_whitespace() {
                self.bump();
            } else if c == '#' {
                // A comment between attributes, to the end of its line.
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.bump();
                }
            } else {
                break;
            }
        }
    }

    fn name(&mut self, what: &str) -> Result<String, HelixError> {
        let mut out = String::new();
        if !self.peek(0).is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
            let found = match self.peek(0) {
                Some(c) => format!("`{c}`"),
                None => "the end of the file".to_string(),
            };
            return Err(self.err(format!("expected {what}, found {found}")));
        }
        let mut brackets = 0usize;
        while let Some(c) = self.peek(0) {
            // After `class:` comes a class name, which may be any utility: `class:bg-accent/10`,
            // `class:w-[42%]`, `class:data-[state=open]:block`. It ends at a space, `=`, `>` or `/>`
            // (a `=` inside brackets is part of it).
            let in_class = out.starts_with("class:");
            let ok = c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | ':' | '.')
                || (in_class && (matches!(c, '[' | ']' | '(' | ')' | '%' | '#' | ',' | '!' | '*' | '@' | '&' | '+')
                    || (c == '/' && self.peek(1) != Some('>'))
                    || (c == '=' && brackets > 0)));
            if !ok {
                break;
            }
            match c {
                '[' => brackets += 1,
                ']' => brackets = brackets.saturating_sub(1),
                _ => {}
            }
            out.push(c);
            self.bump();
        }
        Ok(out)
    }

    fn element(&mut self, depth: usize) -> Result<MNode, HelixError> {
        if depth > MAX_DEPTH {
            return Err(self.err("markup is nested too deeply"));
        }
        let (line, col) = (self.line, self.col);
        self.bump(); // `<`
        if self.peek(0) == Some('>') {
            self.bump();
            let children = self.children(depth, "", line, col)?;
            return Ok(MNode::Fragment { children, line, col });
        }
        let tag = self.name("a tag name")?;
        let mut attrs = Vec::new();
        loop {
            self.skip_space();
            match self.peek(0) {
                Some('/') if self.peek(1) == Some('>') => {
                    self.bump_n(2);
                    return Ok(MNode::Element { tag, attrs, children: Vec::new(), line, col });
                }
                Some('>') => {
                    self.bump();
                    let children = self.children(depth, &tag, line, col)?;
                    return Ok(MNode::Element { tag, attrs, children, line, col });
                }
                Some('{') => {
                    let (al, ac) = (self.line, self.col);
                    let src = self.hole(depth)?;
                    let t = src.text.trim_start();
                    let Some(rest) = t.strip_prefix("...") else {
                        return Err(HelixError::new("a `{…}` among the attributes must spread a record: `{...props}`", al, ac)
                            .hint("an attribute's value goes after its name: `class={c}`."));
                    };
                    let skipped = src.text.len() - rest.len();
                    // Where the spread's expression starts: past the `...` (and any space).
                    let lead: String = src.text[..skipped].to_string();
                    let (mut l, mut c) = (src.line, src.col);
                    for ch in lead.chars() {
                        if ch == '\n' {
                            l += 1;
                            c = 1;
                        } else {
                            c += 1;
                        }
                    }
                    attrs.push(MAttr {
                        name: String::new(),
                        value: MValue::Spread(Src { text: rest.to_string(), line: l, col: c }),
                        line: al,
                        col: ac,
                    });
                }
                Some(_) => {
                    let (al, ac) = (self.line, self.col);
                    let name = self.name("an attribute name, `>` or `/>`")?;
                    let value = if self.peek(0) == Some('=') {
                        self.bump();
                        match self.peek(0) {
                            Some('"') | Some('\'') => MValue::Str(self.string()?),
                            Some('{') => MValue::Expr(self.hole(depth)?),
                            _ => {
                                return Err(self
                                    .err(format!("`{name}=` needs a value: a string or a `{{expression}}`"))
                                    .hint(format!("write `{name}=\"…\"` or `{name}={{value}}`; a number goes in braces: `{name}={{4}}`.")));
                            }
                        }
                    } else {
                        MValue::Flag
                    };
                    attrs.push(MAttr { name, value, line: al, col: ac });
                }
                None => {
                    return Err(HelixError::new(format!("`<{tag}` is never closed"), line, col)
                        .hint("end the opening tag with `>` (and close it with `</…>`) or with `/>`."));
                }
            }
        }
    }

    /// The children of an element, up to and including its closing tag.
    fn children(&mut self, depth: usize, tag: &str, line: usize, col: usize) -> Result<Vec<MNode>, HelixError> {
        let mut out = Vec::new();
        let mut text = String::new();
        loop {
            match self.peek(0) {
                None => {
                    let shown = if tag.is_empty() { "<>".to_string() } else { format!("<{tag}>") };
                    let close = if tag.is_empty() { "</>".to_string() } else { format!("</{tag}>") };
                    return Err(HelixError::new(format!("`{shown}` is never closed"), line, col)
                        .hint(format!("close it with `{close}`, or write a childless tag as `<{tag} />`.")));
                }
                Some('<') if self.peek(1) == Some('/') => {
                    flush(&mut text, &mut out);
                    let (cl, cc) = (self.line, self.col);
                    self.bump_n(2);
                    let closing = if self.peek(0) == Some('>') { String::new() } else { self.name("a closing tag name")? };
                    self.skip_space();
                    if self.peek(0) != Some('>') {
                        return Err(self.err(format!("expected `>` to end `</{closing}`")));
                    }
                    self.bump();
                    if closing != tag {
                        let opened = if tag.is_empty() { "<>".to_string() } else { format!("<{tag}>") };
                        let want = if tag.is_empty() { "</>".to_string() } else { format!("</{tag}>") };
                        let got = if closing.is_empty() { "</>".to_string() } else { format!("</{closing}>") };
                        return Err(HelixError::new(format!("`{got}` closes `{opened}`, which opened at line {line}"), cl, cc)
                            .hint(format!("close it with `{want}`.")));
                    }
                    return Ok(out);
                }
                Some('<') if opens(self.chars, self.i) => {
                    flush(&mut text, &mut out);
                    out.push(self.element(depth + 1)?);
                }
                Some('<') => {
                    return Err(self.err("a `<` in text starts a tag").hint("write a literal one as `&lt;` or `{\"<\"}`."));
                }
                Some('{') => {
                    flush(&mut text, &mut out);
                    let src = self.hole(depth)?;
                    if !is_blank(&src.text) {
                        out.push(MNode::Hole(src));
                    }
                }
                Some('}') => {
                    return Err(self.err("a `}` in text closes nothing").hint("write a literal one as `{\"}\"}`."));
                }
                Some('&') => {
                    let (c, used) = entity(&self.chars[self.i..]);
                    text.push_str(&c);
                    self.bump_n(used);
                }
                Some(c) => {
                    text.push(c);
                    self.bump();
                }
            }
        }
    }

    /// An attribute's string value, as Helix source (quotes included).
    fn string(&mut self) -> Result<Src, HelixError> {
        let (line, col) = (self.line, self.col);
        let used = self.string_len()?;
        let text: String = self.chars[self.i..self.i + used].iter().collect();
        self.bump_n(used);
        Ok(Src { text, line, col })
    }

    /// How many characters the string literal at the cursor spans.
    fn string_len(&self) -> Result<usize, HelixError> {
        let c = self.chars;
        let i = self.i;
        let q = c[i];
        let triple = c.get(i + 1) == Some(&q) && c.get(i + 2) == Some(&q);
        let used = if q == '"' && triple {
            crate::lexer::lex_raw_string(c, i, self.line, self.col)?.1
        } else {
            crate::lexer::lex_string(c, i, self.line, self.col, q, triple)?.1
        };
        Ok(used)
    }

    /// A `{…}` hole: its source (between the braces) and where that starts.
    fn hole(&mut self, depth: usize) -> Result<Src, HelixError> {
        let (open_line, open_col) = (self.line, self.col);
        self.bump(); // `{`
        let (line, col) = (self.line, self.col);
        let from = self.i;
        let mut nest = 0usize;
        // Whether a value just ended, so a `<` here compares rather than opening markup —
        // the rule the lexer applies with tokens, applied to characters.
        let mut after_value = false;
        loop {
            let Some(c) = self.peek(0) else {
                return Err(HelixError::new("this `{` is never closed", open_line, open_col)
                    .hint("a hole in markup is `{expression}`; close it with `}`."));
            };
            match c {
                '"' | '\'' => {
                    let used = self.string_len()?;
                    self.bump_n(used);
                    after_value = true;
                }
                '#' if self.peek(1) == Some('[') => {
                    let mut d = 0usize;
                    loop {
                        match (self.peek(0), self.peek(1)) {
                            (None, _) => return Err(self.err("unterminated block comment")),
                            (Some('#'), Some('[')) => {
                                d += 1;
                                self.bump_n(2);
                            }
                            (Some(']'), Some('#')) => {
                                d -= 1;
                                self.bump_n(2);
                                if d == 0 {
                                    break;
                                }
                            }
                            _ => self.bump(),
                        }
                    }
                }
                // A line comment ends at its line's end, or at the `}` that closes the hole,
                // so `{# a note}` is a comment and nothing else.
                '#' => {
                    while self.peek(0).is_some_and(|c| c != '\n' && !(c == '}' && nest == 0)) {
                        self.bump();
                    }
                }
                '{' | '(' | '[' => {
                    nest += 1;
                    self.bump();
                    after_value = false;
                }
                '}' if nest == 0 => {
                    let text: String = self.chars[from..self.i].iter().collect();
                    self.bump();
                    return Ok(Src { text, line, col });
                }
                '}' | ')' | ']' => {
                    nest = nest.saturating_sub(1);
                    self.bump();
                    after_value = true;
                }
                '<' if !after_value && opens(self.chars, self.i) => {
                    self.element(depth + 1)?;
                    after_value = true;
                }
                '\n' => {
                    self.bump();
                    after_value = false;
                }
                c if c.is_whitespace() => self.bump(),
                c if c.is_alphanumeric() || c == '_' => {
                    let mut word = String::new();
                    while let Some(c) = self.peek(0) {
                        if c.is_alphanumeric() || c == '_' {
                            word.push(c);
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    after_value = !KEYWORDS.contains(&word.as_str());
                }
                _ => {
                    self.bump();
                    after_value = false;
                }
            }
        }
    }
}

/// Whether a hole holds nothing but space and comments: `{# a note }`.
fn is_blank(src: &str) -> bool {
    crate::lexer::lex(src).is_ok_and(|ts| ts.iter().all(|t| matches!(t.tok, crate::token::Tok::Newline | crate::token::Tok::Eof)))
}

/// Add the text gathered so far, cleaned as JSX cleans it: lines are trimmed where they meet
/// a line break, blank lines vanish, and the lines left are joined with one space. So markup
/// can be indented freely, and `Hello, <b>you</b>` keeps its space.
fn flush(text: &mut String, out: &mut Vec<MNode>) {
    if text.is_empty() {
        return;
    }
    let raw = std::mem::take(text);
    let lines: Vec<&str> = raw.split('\n').collect();
    let last_full = lines.iter().rposition(|l| !l.trim().is_empty());
    let mut s = String::new();
    for (i, line) in lines.iter().enumerate() {
        let mut t: &str = line.trim_end_matches('\r');
        if i > 0 {
            t = t.trim_start();
        }
        if i + 1 < lines.len() {
            t = t.trim_end();
        }
        if !t.is_empty() {
            s.push_str(&t.replace('\t', " "));
            if Some(i) != last_full {
                s.push(' ');
            }
        }
    }
    if !s.is_empty() {
        out.push(MNode::Text(s));
    }
}

/// An HTML character reference at the start of `s` (which begins with `&`): the text it
/// stands for and how many characters it spans. Anything that is not one is a literal `&`.
fn entity(s: &[char]) -> (String, usize) {
    let end = s.iter().take(12).position(|c| *c == ';');
    let Some(end) = end else { return ("&".into(), 1) };
    let body: String = s[1..end].iter().collect();
    let named = match body.as_str() {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        "lbrace" => Some('{'),
        "rbrace" => Some('}'),
        "middot" => Some('·'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "times" => Some('×'),
        "copy" => Some('©'),
        "rarr" => Some('→'),
        "larr" => Some('←'),
        _ => None,
    };
    let numeric = || {
        let n = body.strip_prefix('#')?;
        let v = match n.strip_prefix(['x', 'X']) {
            Some(h) => u32::from_str_radix(h, 16).ok()?,
            None => n.parse::<u32>().ok()?,
        };
        char::from_u32(v).filter(|c| !c.is_control() || c.is_whitespace())
    };
    match named.or_else(numeric) {
        Some(c) => (c.to_string(), end + 1),
        None => ("&".into(), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(src: &str) -> MNode {
        let chars: Vec<char> = src.chars().collect();
        scan(&chars, 0, 1, 1).unwrap().0
    }

    #[test]
    fn text_is_cleaned_as_jsx_cleans_it() {
        let n = read("<p>\n    Hello,\n    <b>you</b> and\n\n    me\n  </p>");
        let MNode::Element { children, .. } = n else { panic!() };
        assert_eq!(children[0], MNode::Text("Hello,".into()));
        assert_eq!(children[2], MNode::Text(" and me".into()));
    }

    #[test]
    fn holes_skip_strings_brackets_and_nested_markup() {
        let n = read(r#"<ul>{xs.map((x) => <li title="}">{x}</li>)}</ul>"#);
        let MNode::Element { children, .. } = n else { panic!() };
        let MNode::Hole(src) = &children[0] else { panic!() };
        assert_eq!(src.text, r#"xs.map((x) => <li title="}">{x}</li>)"#);
        assert_eq!((src.line, src.col), (1, 6));
    }

    #[test]
    fn a_comparison_in_a_hole_is_not_markup() {
        let n = read("<p>{if a <b then 1 else 2}</p>");
        let MNode::Element { children, .. } = n else { panic!() };
        assert!(matches!(&children[0], MNode::Hole(s) if s.text == "if a <b then 1 else 2"));
    }

    fn parse_err(src: &str) -> HelixError {
        crate::lexer::lex(src).and_then(crate::parser::parse).expect_err("an error")
    }

    #[test]
    fn errors_point_into_the_markup() {
        // A parse error inside a hole is where the hole is, not at the start of the element.
        let e = parse_err("x = 1\ny = <div>\n  <p>{1 +}</p>\n</div>\n");
        assert_eq!((e.line, e.col), (3, 10), "{}", e.message);
        let e = parse_err("x = <div onClick=\"go\" />\n");
        assert!(e.message.contains("script handler") && e.col == 10, "{}", e.message);
        let e = parse_err("x = <p class=\"a\" class=\"b\" />\n");
        assert!(e.message.contains("given twice"), "{}", e.message);
        let e = parse_err("x = <Nowhere />\n");
        assert!(e.message.contains("needs a function `nowhere`"), "{}", e.message);
        let e = parse_err("x = <p>a < b</p>\n");
        assert!(e.message.contains("starts a tag"), "{}", e.message);
    }

    #[test]
    fn a_value_before_the_angle_bracket_keeps_it_a_comparison() {
        for src in ["print(a <b)\n", "print(xs[0]<n)\n", "print(f(x)<y)\n", "print(1 <<x)\n"] {
            let toks = crate::lexer::lex(src).unwrap();
            assert!(!toks.iter().any(|t| matches!(t.tok, crate::token::Tok::Markup(_))), "{src}");
        }
        for src in ["x = <b/>\n", "f(<b/>)\n", "y = if c then <b/> else <i/>\n", "[<b/>, <i/>]\n"] {
            let toks = crate::lexer::lex(src).unwrap();
            assert!(toks.iter().any(|t| matches!(t.tok, crate::token::Tok::Markup(_))), "{src}");
        }
    }

    #[test]
    fn entities_and_mismatched_tags() {
        let n = read("<p>a &amp; b &#x2192; &nope c</p>");
        let MNode::Element { children, .. } = n else { panic!() };
        assert_eq!(children[0], MNode::Text("a & b → &nope c".into()));
        let chars: Vec<char> = "<p><b></p>".chars().collect();
        let e = scan(&chars, 0, 1, 1).unwrap_err();
        assert!(e.message.contains("closes `<b>`"), "{}", e.message);
    }
}

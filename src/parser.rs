//! Recursive-descent parser with precedence climbing.
//!
//! Grammar (informal):
//!   program    := { NL } [ stmt { NL+ stmt } ] { NL }
//!   stmt       := [ "mut" ] ident "=" expr        -- assignment
//!               | expr                             -- expression statement
//!   expr       := or
//!   or         := and   { "or"  and }
//!   and        := eq    { "and" eq }
//!   eq         := cmp   { ("==" | "!=") cmp }
//!   cmp        := term  { ("<" | ">" | "<=" | ">=") term }
//!   term       := factor{ ("+" | "-") factor }
//!   factor     := unary { ("*" | "/" | "%") unary }
//!   unary      := ("-" | "not") unary | postfix
//!   postfix    := primary { "." ident "(" args ")" | "[" expr "]" | "(" args ")" }
//!   primary    := int | float | str | "true" | "false" | ident
//!               | "(" expr ")" | "[" [ expr { "," expr } ] "]"

use std::collections::HashMap;

use crate::ast::{BinOp, Expr, InterpPart, Stmt, TypeAnn, UnOp};
use crate::error::{suggest, HelixError};
use crate::token::{StrSeg, Tok, Token};

/// A user function's signature, captured at its definition so calls to it can be
/// desugared: named arguments reordered into position and omitted parameters filled
/// with their (literal) defaults. `defaults` is parallel to `params`.
struct FnSig {
    params: Vec<String>,
    defaults: Vec<Option<Expr>>,
}

/// A parsed call's arguments: positional expressions and `name: value` named pairs.
type CallArgs = (Vec<Expr>, Vec<(String, Expr)>);

/// True for the literal-constant expressions allowed as a parameter default. The
/// default is inserted at each call site, so it must not reference anything (no
/// params, no globals) — a literal (optionally negated/notted) guarantees that.
fn is_const_default(e: &Expr) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => true,
        Expr::Unary { expr, .. } => is_const_default(expr),
        _ => false,
    }
}

/// Lex + parse a single expression (used for `{expr}` interpolation fragments).
pub fn parse_expression(src: &str) -> Result<Expr, HelixError> {
    let tokens = crate::lexer::lex(src)?;
    let mut p = Parser { toks: tokens, pos: 0, depth: 0, fn_sigs: HashMap::new() };
    p.skip_newlines();
    let e = p.expr()?;
    p.skip_newlines();
    if !p.at_end() {
        let (l, c) = p.pos();
        return Err(HelixError::new(
            format!("unexpected {} in interpolation `{{...}}`", p.peek().describe()),
            l,
            c,
        ));
    }
    Ok(e)
}

/// Desugar `recv.min_by(key)` / `max_by(key)` / `argmin()` / `argmax()` into
/// existing constructs so both engines handle them with no new ops:
///
///   recv.min_by(p => K)  →  let $o = recv.map(p => (K, p))
///                           in $o.reduce($o[0], ($a, $b) => if $b[0] < $a[0] then $b else $a)[1]
///   recv.argmin()        →  let $o = recv.enumerate()
///                           in $o.reduce($o[0], ($a, $b) => if $b[1] < $a[1] then $b else $a)[0]
///
/// `min_by`/`max_by` reduce over `(key, element)` pairs and return the element;
/// `argmin`/`argmax` reduce over `(index, value)` pairs and return the index.
fn desugar_order_by(
    recv: Expr,
    name: &str,
    mut args: Vec<Expr>,
    line: usize,
    col: usize,
) -> Result<Expr, HelixError> {
    use crate::ast::BinOp;
    let by = name == "min_by" || name == "max_by";
    let op = if name == "min_by" || name == "argmin" { BinOp::Lt } else { BinOp::Gt };

    let ident = |n: &str| Expr::Ident { name: n.to_string(), line, col };
    let index = |e: Expr, i: i64| Expr::Index {
        recv: Box::new(e),
        index: Box::new(Expr::Int(i)),
        line,
        col,
    };

    // The source array of comparison pairs, and which slot is the key vs. the result.
    let (src, key_idx, ret_idx) = if by {
        // `min_by`/`max_by(key)` — extract the key body (`p => K` or implicit `it`).
        let (param, key) = match args.len() {
            1 => match args.pop().unwrap() {
                Expr::Lambda { params, body } if params.len() == 1 => {
                    (params[0].clone(), *body)
                }
                Expr::Lambda { .. } => {
                    return Err(HelixError::new(
                        format!("`{name}` takes a one-argument key function"),
                        line,
                        col,
                    )
                    .hint("e.g. `rows.min_by(r => r.score)` or `xs.min_by(it * it)`."))
                }
                other => ("it".to_string(), other),
            },
            _ => {
                return Err(HelixError::new(
                    format!("`{name}` takes exactly one key function"),
                    line,
                    col,
                )
                .hint("e.g. `rows.min_by(r => r.score)`."))
            }
        };
        let pair = Expr::Tuple(vec![key, ident(&param)]);
        let map = Expr::Method {
            recv: Box::new(recv),
            name: "map".to_string(),
            args: vec![Expr::Lambda { params: vec![param], body: Box::new(pair) }],
            line,
            col,
        };
        (map, 0, 1)
    } else {
        // `argmin`/`argmax()` — pairs are `(index, value)` from `enumerate`.
        if !args.is_empty() {
            return Err(HelixError::new(format!("`{name}` takes no arguments"), line, col)
                .hint("use `min_by`/`max_by` to order by a key."));
        }
        let enumd = Expr::Method {
            recv: Box::new(recv),
            name: "enumerate".to_string(),
            args: vec![],
            line,
            col,
        };
        (enumd, 1, 0)
    };

    // ($a, $b) => if $b[key] OP $a[key] then $b else $a
    let cmp = Expr::Lambda {
        params: vec!["$ob_a".to_string(), "$ob_b".to_string()],
        body: Box::new(Expr::If {
            cond: Box::new(Expr::Binary {
                op,
                left: Box::new(index(ident("$ob_b"), key_idx)),
                right: Box::new(index(ident("$ob_a"), key_idx)),
                line,
                col,
            }),
            then_branch: Box::new(ident("$ob_b")),
            else_branch: Box::new(ident("$ob_a")),
            line,
            col,
        }),
    };
    let reduced = Expr::Method {
        recv: Box::new(ident("$ob")),
        name: "reduce".to_string(),
        args: vec![index(ident("$ob"), 0), cmp],
        line,
        col,
    };
    Ok(Expr::Let {
        bindings: vec![("$ob".to_string(), src)],
        body: Box::new(index(reduced, ret_idx)),
    })
}

const TYPE_NAMES: &[&str] = &[
    "Int", "Float", "Num", "String", "Bool", "Array", "Tensor", "DataFrame", "Dna",
];

pub fn parse(tokens: Vec<Token>) -> Result<Vec<Stmt>, HelixError> {
    let mut p = Parser { toks: tokens, pos: 0, depth: 0, fn_sigs: HashMap::new() };
    p.program()
}

/// Caps how deeply expressions may nest. Every recursive descent path (groups,
/// prefixes, calls, arrays, indexing) funnels through `unary`, so a counter
/// there bounds parser recursion — and, since it bounds AST nesting depth, it
/// also protects the type checker / compiler / tree-walker, which recurse over
/// the same shape. Deep enough nested input would otherwise overflow the stack
/// at parse time rather than producing a clean error.
const MAX_PARSE_DEPTH: usize = 1000;

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    depth: usize,
    /// Signatures of user functions seen so far, keyed by name — for resolving
    /// named arguments and defaults at call sites (a function is defined before it
    /// is called, so its signature is known by the time its calls are parsed).
    fn_sigs: HashMap<String, FnSig>,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_tok(&self) -> &Token {
        &self.toks[self.pos]
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let idx = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[idx].tok
    }

    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at_end(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn pos(&self) -> (usize, usize) {
        let t = self.peek_tok();
        (t.line, t.col)
    }

    fn eat(&mut self, want: &Tok, ctx: &str) -> Result<Token, HelixError> {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(want) {
            Ok(self.advance())
        } else {
            let (l, c) = self.pos();
            Err(HelixError::new(
                format!("expected {} {}, found {}", want.describe(), ctx, self.peek().describe()),
                l,
                c,
            ))
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) {
            self.advance();
        }
    }

    fn program(&mut self) -> Result<Vec<Stmt>, HelixError> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !self.at_end() {
            let s = self.statement()?;
            stmts.push(s);
            if self.at_end() {
                break;
            }
            // statements are separated by at least one newline
            if matches!(self.peek(), Tok::Newline) {
                self.skip_newlines();
            } else {
                let (l, c) = self.pos();
                return Err(HelixError::new(
                    format!(
                        "expected end of line after statement, found {}",
                        self.peek().describe()
                    ),
                    l,
                    c,
                )
                .hint("each statement goes on its own line; Helix has no `;`."));
            }
        }
        Ok(stmts)
    }

    fn statement(&mut self) -> Result<Stmt, HelixError> {
        // `import a.b.c [as alias]` — load the module at `a/b/c.helix`.
        if matches!(self.peek(), Tok::Import) {
            let (l, c) = self.pos();
            self.advance();
            let first = self
                .ident_name("after `import`")
                .map_err(|e| e.hint("import a module by name, e.g. `import stats` or `import lib.stats`."))?;
            let mut segments = vec![first];
            let mut selected: Option<Vec<String>> = None;
            // Dotted path: `import lib.stats` → `lib/stats.helix`. A `.{a, b}` tail
            // instead selects names to bring into scope unqualified.
            while matches!(self.peek(), Tok::Dot) {
                self.advance();
                if matches!(self.peek(), Tok::LBrace) {
                    self.advance(); // consume `{`
                    let mut names = Vec::new();
                    loop {
                        names.push(self.ident_name("to import inside `{ }`")?);
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBrace) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                    self.eat(&Tok::RBrace, "to close the import list").map_err(|e| {
                        e.hint("a selective import looks like `import lib.stats.{mean, std}`.")
                    })?;
                    selected = Some(names);
                    break; // nothing follows a selective list
                }
                segments.push(self.ident_name("after `.` in the module path")?);
            }
            // Optional `as alias` — `as` is contextual (only special here), so it
            // stays usable as an ordinary identifier everywhere else. A selective
            // import binds its names directly, so it takes no alias.
            let alias = if selected.is_none() && matches!(self.peek(), Tok::Ident(n) if n == "as") {
                self.advance();
                self.ident_name("after `as`")
                    .map_err(|e| e.hint("give the module an alias, e.g. `import lib.stats as stats`."))?
            } else {
                // Default the namespace to the last path segment.
                segments.last().unwrap().clone()
            };
            return Ok(Stmt::Import { segments, alias, selected, line: l, col: c });
        }
        // `fn name(a, b) = expr`
        if matches!(self.peek(), Tok::Fn) {
            let (l, c) = self.pos();
            self.advance();
            let name = self.ident_name("after `fn`")?;
            self.eat(&Tok::LParen, "to start the parameter list")
                .map_err(|e| e.hint("functions look like `fn area(w, h) = w * h`."))?;
            let mut params: Vec<(String, Option<TypeAnn>)> = Vec::new();
            let mut defaults: Vec<Option<Expr>> = Vec::new();
            if !matches!(self.peek(), Tok::RParen) {
                loop {
                    let (pl, pc) = self.pos();
                    let pname = self.ident_name("as a parameter")?;
                    // optional `: Type` annotation
                    let ann = if matches!(self.peek(), Tok::Colon) {
                        self.advance();
                        Some(self.parse_type_ann()?)
                    } else {
                        None
                    };
                    // optional `= literal` default value
                    let default = if matches!(self.peek(), Tok::Eq) {
                        self.advance();
                        let d = self.expr()?;
                        if !is_const_default(&d) {
                            return Err(HelixError::new(
                                format!("the default for parameter `{pname}` must be a literal constant"),
                                pl,
                                pc,
                            )
                            .hint("defaults like `= 0`, `= -5`, `= \"\"`, `= true`, or `= missing` are allowed."));
                        }
                        Some(d)
                    } else {
                        None
                    };
                    // Parameters with defaults must come last, so positional binding is
                    // unambiguous.
                    if default.is_none() && defaults.iter().any(|d| d.is_some()) {
                        return Err(HelixError::new(
                            format!("parameter `{pname}` has no default but follows one that does"),
                            pl,
                            pc,
                        )
                        .hint("put parameters with defaults after those without."));
                    }
                    params.push((pname, ann));
                    defaults.push(default);
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            self.eat(&Tok::RParen, "to close the parameter list")?;
            // Record the signature BEFORE parsing the body, so a recursive call inside
            // it resolves named arguments and defaults against this function.
            self.fn_sigs.insert(
                name.clone(),
                FnSig { params: params.iter().map(|(n, _)| n.clone()).collect(), defaults },
            );
            // optional `-> Type` return annotation
            let ret = if matches!(self.peek(), Tok::Arrow) {
                self.advance();
                Some(self.parse_type_ann()?)
            } else {
                None
            };
            self.eat(&Tok::Eq, "before the function body")
                .map_err(|e| e.hint("a function body is an expression: `fn f(x) = x + 1`."))?;
            let body = self.expr()?;
            return Ok(Stmt::Func {
                name,
                params,
                ret,
                body,
                line: l,
                col: c,
            });
        }
        // `mut x = ...` or `mut a, b = ...`
        if matches!(self.peek(), Tok::Mut) {
            let (l, c) = self.pos();
            self.advance();
            let first = self.ident_name("after `mut`")?;
            if matches!(self.peek(), Tok::Comma) {
                let names = self.finish_target_list(first)?;
                self.eat(&Tok::Eq, "in destructuring assignment")?;
                let value = self.expr()?;
                return Ok(Stmt::Destructure { names, mutable: true, value, line: l, col: c });
            }
            self.eat(&Tok::Eq, "in assignment")?;
            let value = self.expr()?;
            return Ok(Stmt::Assign {
                name: first,
                mutable: true,
                value,
                line: l,
                col: c,
            });
        }
        // `a, b = ...` — destructuring (2+ names ending in `=`)
        if self.at_destructure() {
            let (l, c) = self.pos();
            let first = self.ident_name("")?;
            let names = self.finish_target_list(first)?;
            self.eat(&Tok::Eq, "in destructuring assignment")?;
            let value = self.expr()?;
            return Ok(Stmt::Destructure { names, mutable: false, value, line: l, col: c });
        }
        // `x = ...`  (only when an identifier is immediately followed by a single `=`)
        if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Eq) {
            let (l, c) = self.pos();
            let name = self.ident_name("")?;
            self.eat(&Tok::Eq, "in assignment")?;
            let value = self.expr()?;
            return Ok(Stmt::Assign {
                name,
                mutable: false,
                value,
                line: l,
                col: c,
            });
        }
        Ok(Stmt::Expr(self.expr()?))
    }

    /// True if the upcoming tokens are `ident (, ident)+ =` — a destructuring
    /// target list (2+ names). A single `ident =` is a normal assignment.
    fn at_destructure(&self) -> bool {
        if !matches!(self.peek(), Tok::Ident(_)) {
            return false;
        }
        let mut k = self.pos + 1;
        let mut commas = 0;
        loop {
            match &self.toks[k].tok {
                Tok::Comma => {
                    commas += 1;
                    k += 1;
                    if !matches!(self.toks[k].tok, Tok::Ident(_)) {
                        return false;
                    }
                    k += 1;
                }
                Tok::Eq => return commas >= 1,
                _ => return false,
            }
        }
    }

    /// Given the first target name (already consumed), consume `, ident…`.
    fn finish_target_list(&mut self, first: String) -> Result<Vec<String>, HelixError> {
        let mut names = vec![first];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            names.push(self.ident_name("as a destructuring target")?);
        }
        Ok(names)
    }

    fn ident_name(&mut self, ctx: &str) -> Result<String, HelixError> {
        match self.peek().clone() {
            Tok::Ident(n) => {
                self.advance();
                Ok(n)
            }
            other => {
                let (l, c) = self.pos();
                Err(HelixError::new(
                    format!("expected a name {}, found {}", ctx, other.describe()),
                    l,
                    c,
                ))
            }
        }
    }

    /// Like `ident_name`, but also accepts a reserved keyword as the name — used
    /// for member access after `.`, where a keyword can only be a member (so
    /// `python.import(...)`, `x.in`, etc. parse). Returns the keyword's source text.
    fn member_name(&mut self, ctx: &str) -> Result<String, HelixError> {
        let kw = match self.peek() {
            Tok::Ident(n) => Some(n.clone()),
            Tok::Mut => Some("mut".to_string()),
            Tok::Fn => Some("fn".to_string()),
            Tok::Import => Some("import".to_string()),
            Tok::And => Some("and".to_string()),
            Tok::Or => Some("or".to_string()),
            Tok::Not => Some("not".to_string()),
            Tok::If => Some("if".to_string()),
            Tok::Then => Some("then".to_string()),
            Tok::Else => Some("else".to_string()),
            Tok::Let => Some("let".to_string()),
            Tok::In => Some("in".to_string()),
            Tok::Match => Some("match".to_string()),
            Tok::Try => Some("try".to_string()),
            Tok::Do => Some("do".to_string()),
            Tok::Missing => Some("missing".to_string()),
            Tok::True => Some("true".to_string()),
            Tok::False => Some("false".to_string()),
            _ => None,
        };
        match kw {
            Some(n) => {
                self.advance();
                Ok(n)
            }
            None => {
                let (l, c) = self.pos();
                Err(HelixError::new(
                    format!("expected a name {}, found {}", ctx, self.peek().describe()),
                    l,
                    c,
                ))
            }
        }
    }

    /// Parse a function-signature type annotation: one capitalized type word.
    fn parse_type_ann(&mut self) -> Result<TypeAnn, HelixError> {
        let (l, c) = self.pos();
        let word = match self.peek().clone() {
            Tok::Ident(n) => n,
            other => {
                return Err(HelixError::new(
                    format!("expected a type name, found {}", other.describe()),
                    l,
                    c,
                )
                .hint("e.g. `fn area(w: Int, h: Int) -> Int = ...`."))
            }
        };
        let ann = match word.as_str() {
            "Int" => TypeAnn::Int,
            "Float" => TypeAnn::Float,
            "Num" => TypeAnn::Num,
            "String" => TypeAnn::String,
            "Bool" => TypeAnn::Bool,
            "Array" => TypeAnn::Array,
            "Tensor" => TypeAnn::Tensor,
            "DataFrame" => TypeAnn::DataFrame,
            "Dna" => TypeAnn::Dna,
            _ => {
                let mut err =
                    HelixError::new(format!("unknown type `{}`", word), l, c);
                if let Some(s) = suggest(&word, TYPE_NAMES) {
                    err = err.hint(format!("did you mean `{}`?", s));
                } else {
                    err = err.hint(format!("known types: {}", TYPE_NAMES.join(", ")));
                }
                return Err(err);
            }
        };
        self.advance();
        Ok(ann)
    }

    fn expr(&mut self) -> Result<Expr, HelixError> {
        if let Some(lam) = self.try_lambda()? {
            return Ok(lam);
        }
        self.coalesce_expr()
    }

    /// `a ?? b` — lowest precedence, left-associative.
    fn coalesce_expr(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.or_expr()?;
        while matches!(self.peek(), Tok::Coalesce) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.or_expr()?;
            left = Expr::Binary {
                op: BinOp::Coalesce,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    /// Recognize `x => body` and `(a, b) => body` without consuming input unless
    /// it really is a lambda. Returns None (and leaves the cursor put) otherwise.
    fn try_lambda(&mut self) -> Result<Option<Expr>, HelixError> {
        // single parameter: IDENT =>
        if let Tok::Ident(name) = self.peek().clone() {
            if matches!(self.peek_at(1), Tok::FatArrow) {
                self.advance(); // IDENT
                self.advance(); // =>
                let body = self.expr()?;
                return Ok(Some(Expr::Lambda {
                    params: vec![name],
                    body: Box::new(body),
                }));
            }
            return Ok(None);
        }

        // multiple parameters: ( IDENT (, IDENT)* ) =>
        if matches!(self.peek(), Tok::LParen) {
            let mut k = self.pos + 1;
            let mut params = Vec::new();
            loop {
                match &self.toks[k].tok {
                    Tok::Ident(nm) => {
                        params.push(nm.clone());
                        k += 1;
                        match &self.toks[k].tok {
                            Tok::Comma => {
                                k += 1;
                                continue;
                            }
                            Tok::RParen => {
                                k += 1;
                                break;
                            }
                            _ => return Ok(None),
                        }
                    }
                    _ => return Ok(None), // empty `()` or non-ident — not a lambda
                }
            }
            if !matches!(self.toks[k].tok, Tok::FatArrow) {
                return Ok(None);
            }
            // Commit: consume `( params ) =>`, then parse the body.
            while self.pos <= k {
                self.advance();
            }
            let body = self.expr()?;
            return Ok(Some(Expr::Lambda {
                params,
                body: Box::new(body),
            }));
        }

        Ok(None)
    }

    fn or_expr(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.and_expr()?;
        while matches!(self.peek(), Tok::Or) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.and_expr()?;
            left = Expr::Binary {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.equality()?;
        while matches!(self.peek(), Tok::And) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.equality()?;
            left = Expr::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    fn equality(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.comparison()?;
        loop {
            let op = match self.peek() {
                Tok::EqEq => BinOp::Eq,
                Tok::Ne => BinOp::Ne,
                _ => break,
            };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.comparison()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.bit_or()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => BinOp::Lt,
                Tok::Gt => BinOp::Gt,
                Tok::Le => BinOp::Le,
                Tok::Ge => BinOp::Ge,
                _ => break,
            };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.bit_or()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    // Bitwise operators (int-only), all left-associative. Mirroring Rust's ordering,
    // they bind tighter than comparison and looser than `+`/`-`, with precedence
    // `|` < `^` < `&` < `<<`/`>>`. So `mask >> j & 1 == 1` is `((mask>>j)&1)==1`.
    fn bit_or(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.bit_xor()?;
        while matches!(self.peek(), Tok::Pipe) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.bit_xor()?;
            left = Expr::Binary { op: BinOp::BitOr, left: Box::new(left), right: Box::new(right), line: l, col: c };
        }
        self.depth = saved;
        Ok(left)
    }

    fn bit_xor(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.bit_and()?;
        while matches!(self.peek(), Tok::Caret) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.bit_and()?;
            left = Expr::Binary { op: BinOp::BitXor, left: Box::new(left), right: Box::new(right), line: l, col: c };
        }
        self.depth = saved;
        Ok(left)
    }

    fn bit_and(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.shift()?;
        while matches!(self.peek(), Tok::Amp) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.shift()?;
            left = Expr::Binary { op: BinOp::BitAnd, left: Box::new(left), right: Box::new(right), line: l, col: c };
        }
        self.depth = saved;
        Ok(left)
    }

    fn shift(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.range_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Shl => BinOp::Shl,
                Tok::Shr => BinOp::Shr,
                _ => break,
            };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.range_expr()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right), line: l, col: c };
        }
        self.depth = saved;
        Ok(left)
    }

    /// A range literal `a..b` desugars to the existing `range(a, b)` builtin call,
    /// so every back-end (tree-walker, VM, JIT fusion via `as_range_call`) handles it
    /// with no further changes. `..` binds looser than `+`/`-` (so `0..n+1` is
    /// `0..(n+1)`) but tighter than comparisons; it does not chain (`a..b..c` is a
    /// parse error). Exclusive of the upper bound, matching `range`.
    fn range_expr(&mut self) -> Result<Expr, HelixError> {
        let left = self.term()?;
        if matches!(self.peek(), Tok::DotDot) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.term()?;
            if matches!(self.peek(), Tok::DotDot) {
                return Err(HelixError::new("a range `a..b` cannot be chained", l, c)
                    .hint("write a single `start..end`, e.g. `0..n`."));
            }
            return Ok(Expr::Call {
                name: "range".into(),
                args: vec![left, right],
                line: l,
                col: c,
            });
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.factor()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.factor()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    fn factor(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.unary()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    /// Account for one more structural level — recursive *nesting* (`((…))`) or
    /// left-spine growth from a *chain* (`a+b+c…`, `x.f().g()…`, built iteratively
    /// in the precedence loops below). Errors past `MAX_PARSE_DEPTH` so a later
    /// recursive pass over the AST — type-check, compile, or even `Box<Expr>`'s
    /// `Drop` — can't overflow the native stack on pathological input. The parser
    /// is the single place that bounds AST depth, so every depth-increasing
    /// construct must call this.
    fn deepen(&mut self) -> Result<(), HelixError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            let (l, c) = self.pos();
            return Err(
                HelixError::new("expression nested or chained too deeply", l, c)
                    .hint("split very large or deeply-nested expressions."),
            );
        }
        Ok(())
    }

    fn unary(&mut self) -> Result<Expr, HelixError> {
        // Every nesting path passes through here, so this bounds recursive-descent
        // depth; the precedence loops add `deepen()` to bound left-spine chains too.
        self.deepen()?;
        let r = self.unary_inner();
        self.depth -= 1;
        r
    }

    fn unary_inner(&mut self) -> Result<Expr, HelixError> {
        let (l, c) = self.pos();
        match self.peek() {
            Tok::Minus => {
                self.advance();
                let e = self.unary()?;
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(e),
                    line: l,
                    col: c,
                })
            }
            Tok::Not => {
                self.advance();
                let e = self.unary()?;
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(e),
                    line: l,
                    col: c,
                })
            }
            Tok::Try => {
                self.advance();
                let e = self.unary()?;
                Ok(Expr::Try { expr: Box::new(e), line: l, col: c })
            }
            _ => self.power(),
        }
    }

    /// Exponentiation binds tighter than unary minus and is right-associative,
    /// so `-2 ** 2` is `-(2 ** 2)` and `2 ** 3 ** 2` is `2 ** (3 ** 2)`.
    fn power(&mut self) -> Result<Expr, HelixError> {
        let base = self.postfix()?;
        if matches!(self.peek(), Tok::StarStar) {
            let (l, c) = self.pos();
            self.advance();
            let exp = self.unary()?;
            Ok(Expr::Binary {
                op: BinOp::Pow,
                left: Box::new(base),
                right: Box::new(exp),
                line: l,
                col: c,
            })
        } else {
            Ok(base)
        }
    }

    fn postfix(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.deepen()?;
                    let (l, c) = self.pos();
                    self.advance();
                    let name = self.member_name("after `.`")?;
                    // `.name(...)` is a method call; `.name` (no parens) is record
                    // field access — one obvious way: parens mean a call.
                    if matches!(self.peek(), Tok::LParen) {
                        self.advance();
                        let (args, named) = self.call_args()?;
                        if !named.is_empty() {
                            return Err(HelixError::new(
                                "named arguments are not supported on method calls",
                                l,
                                c,
                            )
                            .hint("pass method arguments positionally."));
                        }
                        self.eat(&Tok::RParen, "to close the argument list")?;
                        // `min_by`/`max_by`/`argmin`/`argmax` are sugar — desugared here
                        // into `map`/`enumerate` + `reduce` + index, so both engines get
                        // them for free (no new ops, parity by construction).
                        e = match name.as_str() {
                            "min_by" | "max_by" | "argmin" | "argmax" => {
                                desugar_order_by(e, &name, args, l, c)?
                            }
                            // `a.zipmap(b, f)` == `a.zip(b).map(f)` — a paired
                            // elementwise map, desugared so both engines reuse the
                            // tested zip+map (parity by construction). For plain
                            // arithmetic, prefer broadcast (`a * b`, `sin(a) + b`).
                            "zipmap" => {
                                if args.len() != 2 {
                                    return Err(HelixError::new(
                                        format!("`zipmap` takes (other, fn), got {} arguments", args.len()),
                                        l,
                                        c,
                                    )
                                    .hint("e.g. `xs.zipmap(ys, (x, y) => x + y)`."));
                                }
                                let mut it = args.into_iter();
                                let other = it.next().unwrap();
                                let f = it.next().unwrap();
                                let zipped = Expr::Method {
                                    recv: Box::new(e),
                                    name: "zip".into(),
                                    args: vec![other],
                                    line: l,
                                    col: c,
                                };
                                Expr::Method {
                                    recv: Box::new(zipped),
                                    name: "map".into(),
                                    args: vec![f],
                                    line: l,
                                    col: c,
                                }
                            }
                            _ => Expr::Method {
                                recv: Box::new(e),
                                name,
                                args,
                                line: l,
                                col: c,
                            },
                        };
                    } else {
                        e = Expr::Field {
                            recv: Box::new(e),
                            name,
                            line: l,
                            col: c,
                        };
                    }
                }
                Tok::LBracket => {
                    self.deepen()?;
                    let (l, c) = self.pos();
                    self.advance();
                    // `start? : stop? (: step?)?` is a slice; a bare expr is an index.
                    let start = if matches!(self.peek(), Tok::Colon | Tok::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    if matches!(self.peek(), Tok::Colon) {
                        self.advance();
                        let stop = if matches!(self.peek(), Tok::Colon | Tok::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.expr()?))
                        };
                        let step = if matches!(self.peek(), Tok::Colon) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBracket) {
                                None
                            } else {
                                Some(Box::new(self.expr()?))
                            }
                        } else {
                            None
                        };
                        self.eat(&Tok::RBracket, "to close the slice")?;
                        e = Expr::Slice {
                            recv: Box::new(e),
                            start,
                            stop,
                            step,
                            line: l,
                            col: c,
                        };
                    } else {
                        let index = match start {
                            Some(ix) => ix,
                            None => {
                                return Err(HelixError::new(
                                    "expected an index or slice inside `[...]`",
                                    l,
                                    c,
                                )
                                .hint("e.g. `xs[0]`, `xs[1:3]`, or `xs[::2]`."))
                            }
                        };
                        self.eat(&Tok::RBracket, "to close the index")?;
                        e = Expr::Index {
                            recv: Box::new(e),
                            index,
                            line: l,
                            col: c,
                        };
                    }
                }
                Tok::LParen => {
                    // A call is only valid directly on a bare name: `print(...)`.
                    if let Expr::Ident { name, line, col } = e {
                        self.deepen()?;
                        self.advance();
                        let (pos, named) = self.call_args()?;
                        self.eat(&Tok::RParen, "to close the argument list")?;
                        let args = self.resolve_call_args(&name, pos, named, line, col)?;
                        e = Expr::Call {
                            name,
                            args,
                            line,
                            col,
                        };
                    } else {
                        let (l, c) = self.pos();
                        return Err(HelixError::new("this value cannot be called", l, c)
                            .hint("only named functions can be called, e.g. `print(...)`."));
                    }
                }
                _ => break,
            }
        }
        self.depth = saved;
        Ok(e)
    }

    /// Parse a call's argument list into positional args and named args
    /// (`name: value`). Positional arguments must precede named ones.
    fn call_args(&mut self) -> Result<CallArgs, HelixError> {
        let mut pos = Vec::new();
        let mut named: Vec<(String, Expr)> = Vec::new();
        if matches!(self.peek(), Tok::RParen) {
            return Ok((pos, named));
        }
        loop {
            // `ident:` introduces a named argument (distinct from a bare expression).
            if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Colon) {
                let name = self.ident_name("as an argument name")?;
                self.advance(); // the `:`
                named.push((name, self.expr()?));
            } else {
                if !named.is_empty() {
                    let (l, c) = self.pos();
                    return Err(HelixError::new(
                        "a positional argument cannot follow a named one",
                        l,
                        c,
                    )
                    .hint("put positional arguments first, then named ones."));
                }
                pos.push(self.expr()?);
            }
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
                if matches!(self.peek(), Tok::RParen) {
                    break; // trailing comma
                }
            } else {
                break;
            }
        }
        Ok((pos, named))
    }

    /// Desugar a call's positional + named arguments into a single positional vector,
    /// using the called function's recorded signature: named arguments are placed by
    /// name and omitted parameters filled with their (literal) defaults. Calls to
    /// non-user functions (builtins) accept only positional arguments; their arity is
    /// checked later by the type checker, so positional args pass through unchanged.
    fn resolve_call_args(
        &self,
        name: &str,
        pos: Vec<Expr>,
        named: Vec<(String, Expr)>,
        line: usize,
        col: usize,
    ) -> Result<Vec<Expr>, HelixError> {
        let Some(sig) = self.fn_sigs.get(name) else {
            if !named.is_empty() {
                return Err(HelixError::new(
                    format!("named arguments are only supported for user-defined functions, not `{name}`"),
                    line,
                    col,
                )
                .hint("pass arguments to builtins positionally."));
            }
            return Ok(pos);
        };
        let n = sig.params.len();
        // Fast path: plain positional call that the arity check can handle directly.
        if named.is_empty() && (pos.len() == n || sig.defaults.iter().all(|d| d.is_none())) {
            return Ok(pos);
        }
        if pos.len() > n {
            return Err(HelixError::new(
                format!(
                    "`{name}` takes {n} parameter{}, but {} positional arguments were given",
                    if n == 1 { "" } else { "s" },
                    pos.len()
                ),
                line,
                col,
            ));
        }
        let mut slots: Vec<Option<Expr>> = (0..n).map(|_| None).collect();
        for (i, p) in pos.into_iter().enumerate() {
            slots[i] = Some(p);
        }
        for (pname, value) in named {
            let Some(idx) = sig.params.iter().position(|p| *p == pname) else {
                return Err(HelixError::new(
                    format!("`{name}` has no parameter named `{pname}`"),
                    line,
                    col,
                )
                .hint(format!("its parameters are: {}", sig.params.join(", "))));
            };
            if slots[idx].is_some() {
                return Err(HelixError::new(
                    format!("parameter `{pname}` of `{name}` was given more than once"),
                    line,
                    col,
                ));
            }
            slots[idx] = Some(value);
        }
        let mut out = Vec::with_capacity(n);
        for (i, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(e) => out.push(e),
                None => match &sig.defaults[i] {
                    Some(d) => out.push(d.clone()),
                    None => {
                        return Err(HelixError::new(
                            format!("`{name}` is missing an argument for parameter `{}`", sig.params[i]),
                            line,
                            col,
                        )
                        .hint("pass it positionally or by name, or give the parameter a default."))
                    }
                },
            }
        }
        Ok(out)
    }

    /// Parse a pattern, collecting `a | b | c` alternatives into an `Or`.
    fn parse_pattern(&mut self) -> Result<crate::ast::Pattern, HelixError> {
        let (l, c) = self.pos();
        let first = self.parse_single_pattern()?;
        if !matches!(self.peek(), Tok::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while matches!(self.peek(), Tok::Pipe) {
            self.advance();
            alts.push(self.parse_single_pattern()?);
        }
        // v1: or-pattern alternatives must not bind variables, so the arm's bindings
        // are unambiguous regardless of which alternative matched.
        if alts.iter().any(|a| !crate::interp::pattern_binding_names(a).is_empty()) {
            return Err(HelixError::new("or-patterns (`a | b`) cannot bind variables yet", l, c)
                .hint("use literal alternatives, e.g. `1 | 2 | 3 => ...`."));
        }
        Ok(crate::ast::Pattern::Or(alts))
    }

    /// Parse one pattern alternative: a literal, `_`, a name to bind, a tuple, or a
    /// record. Tuple elements and record field values recurse through `parse_pattern`,
    /// so `|` nests inside them.
    fn parse_single_pattern(&mut self) -> Result<crate::ast::Pattern, HelixError> {
        use crate::ast::Pattern;
        let (l, c) = self.pos();
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.advance();
                Ok(if name == "_" { Pattern::Wildcard } else { Pattern::Bind(name) })
            }
            Tok::Int(v) => {
                self.advance();
                Ok(Pattern::Int(v))
            }
            Tok::Float(v) => {
                self.advance();
                Ok(Pattern::Float(v))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Pattern::Str(s))
            }
            Tok::True => {
                self.advance();
                Ok(Pattern::Bool(true))
            }
            Tok::False => {
                self.advance();
                Ok(Pattern::Bool(false))
            }
            Tok::Missing => {
                self.advance();
                Ok(Pattern::Missing)
            }
            Tok::Minus => {
                self.advance();
                match self.peek().clone() {
                    Tok::Int(v) => {
                        self.advance();
                        Ok(Pattern::Int(-v))
                    }
                    Tok::Float(v) => {
                        self.advance();
                        Ok(Pattern::Float(-v))
                    }
                    other => Err(HelixError::new(
                        format!("expected a number after `-` in a pattern, found {}", other.describe()),
                        l,
                        c,
                    )),
                }
            }
            Tok::LParen => {
                self.advance();
                if matches!(self.peek(), Tok::RParen) {
                    self.advance();
                    return Ok(Pattern::Tuple(Vec::new()));
                }
                let first = self.parse_pattern()?;
                if matches!(self.peek(), Tok::Comma) {
                    let mut pats = vec![first];
                    while matches!(self.peek(), Tok::Comma) {
                        self.advance();
                        if matches!(self.peek(), Tok::RParen) {
                            break; // trailing comma / `(p,)`
                        }
                        pats.push(self.parse_pattern()?);
                    }
                    self.eat(&Tok::RParen, "to close a tuple pattern")?;
                    Ok(Pattern::Tuple(pats))
                } else {
                    self.eat(&Tok::RParen, "to close a grouped pattern")?;
                    Ok(first) // `(p)` just groups
                }
            }
            Tok::LBrace => {
                self.advance();
                let mut fields = Vec::new();
                while !matches!(self.peek(), Tok::RBrace) {
                    let key = self.ident_name("as a record-pattern field")?;
                    let subpat = if matches!(self.peek(), Tok::Colon) {
                        self.advance();
                        self.parse_pattern()?
                    } else {
                        Pattern::Bind(key.clone()) // `{field}` shorthand binds the field
                    };
                    fields.push((key, subpat));
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBrace, "to close a record pattern")?;
                Ok(Pattern::Record(fields))
            }
            other => Err(HelixError::new(
                format!("expected a pattern, found {}", other.describe()),
                l,
                c,
            )
            .hint("a pattern is a literal, `_`, a name, a tuple `(a, b)`, or a record `{ok: true, value: v}`.")),
        }
    }

    /// `do { name = expr  <newline>  ...  final_expr }` — a block of sequential
    /// bindings ending in a result expression. Desugars to `let name = expr, … in
    /// final_expr`, so every back-end and the checker handle it with no changes;
    /// with no bindings it is just the body. Bindings are newline-separated (Helix
    /// has no `;`), and the last non-binding line is the block's value. The bindings
    /// are immutable `let`s, like `let … in`, so the language model is unchanged —
    /// `do` only flattens deep `let … in` chains so they read top-to-bottom.
    fn do_block(&mut self) -> Result<Expr, HelixError> {
        let (l, c) = self.pos();
        self.advance(); // `do`
        self.eat(&Tok::LBrace, "after `do`")
            .map_err(|e| e.hint("a `do` block looks like `do { x = 1\\n  y = 2\\n  x + y }`."))?;
        self.skip_newlines();
        let mut bindings = Vec::new();
        loop {
            // A binding is `IDENT = expr` — a single `=`, never `==`. Anything else
            // is the block's final result expression.
            let binding_name = match self.peek().clone() {
                Tok::Ident(name) if matches!(self.peek_at(1), Tok::Eq) => Some(name),
                _ => None,
            };
            if let Some(name) = binding_name {
                self.advance(); // IDENT
                self.advance(); // =
                let value = self.expr()?;
                bindings.push((name, value));
                self.skip_newlines();
                continue;
            }
            if matches!(self.peek(), Tok::RBrace) {
                return Err(HelixError::new("a `do` block must end with a result expression", l, c)
                    .hint("add a final line that produces the block's value, e.g. `do { x = 1\\n  x + 1 }`."));
            }
            let body = self.expr()?;
            self.skip_newlines();
            self.eat(&Tok::RBrace, "to close the `do` block")
                .map_err(|e| e.hint("a `do` block ends with `}` after its result expression."))?;
            return Ok(if bindings.is_empty() {
                body
            } else {
                Expr::Let { bindings, body: Box::new(body) }
            });
        }
    }

    fn primary(&mut self) -> Result<Expr, HelixError> {
        let (l, c) = self.pos();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.advance();
                Ok(Expr::Int(v))
            }
            Tok::Float(v) => {
                self.advance();
                Ok(Expr::Float(v))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::InterpStr(segs) => {
                self.advance();
                let mut parts = Vec::with_capacity(segs.len());
                for seg in segs {
                    match seg {
                        StrSeg::Lit(t) => parts.push(InterpPart::Lit(t)),
                        StrSeg::Expr(src, spec_src) => {
                            // The fragment is lexed+parsed as its own snippet, so any
                            // error inside it carries snippet-relative positions (line 1).
                            // Relocate it to the interpolated string's real position so
                            // the caret points at the user's actual source, not line 1.
                            let e = parse_expression(&src)
                                .map_err(|err| HelixError { line: l, col: c, ..err })?;
                            // Parse the format spec now, so a malformed spec is a parse
                            // error pointing at the string (never a runtime surprise).
                            let spec = match spec_src {
                                Some(sp) => Some(
                                    crate::strfmt::parse_spec(&sp)
                                        .map_err(|m| HelixError::new(m, l, c))?,
                                ),
                                None => None,
                            };
                            parts.push(InterpPart::Expr(Box::new(e), spec));
                        }
                    }
                }
                Ok(Expr::Interp(parts))
            }
            Tok::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::Missing => {
                self.advance();
                Ok(Expr::Missing)
            }
            Tok::Ident(name) => {
                self.advance();
                Ok(Expr::Ident { name, line: l, col: c })
            }
            Tok::At => {
                // `@name` — a DataFrame column reference. The name that follows must
                // be a plain identifier (`@age`), not a keyword or expression.
                self.advance();
                let name = self.ident_name("after `@` (a column name)").map_err(|e| {
                    e.hint("`@` marks a DataFrame column, e.g. `df.where(@age > 40)`.")
                })?;
                Ok(Expr::Column { name, line: l, col: c })
            }
            Tok::Let => {
                self.advance();
                let mut bindings = Vec::new();
                loop {
                    let name = self.ident_name("as a `let` binding")?;
                    self.eat(&Tok::Eq, "in a `let` binding")
                        .map_err(|e| e.hint("`let` looks like `let a = 1, b = 2 in a + b`."))?;
                    let value = self.expr()?;
                    bindings.push((name, value));
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::In, "after `let` bindings")
                    .map_err(|e| e.hint("a `let` needs an `in` body: `let x = 1 in x + 1`."))?;
                let body = self.expr()?;
                Ok(Expr::Let {
                    bindings,
                    body: Box::new(body),
                })
            }
            Tok::Do => self.do_block(),
            Tok::If => {
                self.advance();
                let cond = self.expr()?;
                self.eat(&Tok::Then, "after the condition").map_err(|e| {
                    e.hint("an `if` expression looks like `if cond then a else b`.")
                })?;
                let then_branch = self.expr()?;
                self.eat(&Tok::Else, "after the `then` branch").map_err(|e| {
                    e.hint("`if` is an expression, so it always needs an `else` branch that yields a value.")
                })?;
                let else_branch = self.expr()?;
                Ok(Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(then_branch),
                    else_branch: Box::new(else_branch),
                    line: l,
                    col: c,
                })
            }
            Tok::Match => {
                self.advance();
                let scrutinee = self.expr()?;
                self.eat(&Tok::LBrace, "after the `match` value").map_err(|e| {
                    e.hint("a `match` looks like `match x { 0 => \"zero\", _ => \"other\" }`.")
                })?;
                let mut arms = Vec::new();
                while !matches!(self.peek(), Tok::RBrace) {
                    let pattern = self.parse_pattern()?;
                    // Optional guard: `pat if cond => ...`.
                    let guard = if matches!(self.peek(), Tok::If) {
                        self.advance();
                        Some(self.expr()?)
                    } else {
                        None
                    };
                    self.eat(&Tok::FatArrow, "after a match pattern").map_err(|e| {
                        e.hint("each arm is `pattern [if guard] => result`, e.g. `0 => \"zero\"`.")
                    })?;
                    let body = self.expr()?;
                    arms.push(crate::ast::MatchArm { pattern, guard, body });
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBrace, "to close the `match`")?;
                if arms.is_empty() {
                    return Err(HelixError::new("a `match` needs at least one arm", l, c)
                        .hint("e.g. `match x { 0 => \"zero\", _ => \"other\" }`."));
                }
                Ok(Expr::Match { scrutinee: Box::new(scrutinee), arms, line: l, col: c })
            }
            Tok::LParen => {
                // `(e)` groups; `()` / `(a, b)` / `(x,)` build a tuple. (A lambda
                // `(a, b) =>` was already handled by `try_lambda`.)
                self.advance();
                if matches!(self.peek(), Tok::RParen) {
                    self.advance();
                    return Ok(Expr::Tuple(Vec::new()));
                }
                let first = self.expr()?;
                if matches!(self.peek(), Tok::Comma) {
                    let mut elems = vec![first];
                    while matches!(self.peek(), Tok::Comma) {
                        self.advance();
                        if matches!(self.peek(), Tok::RParen) {
                            break; // trailing comma / `(x,)`
                        }
                        elems.push(self.expr()?);
                    }
                    self.eat(&Tok::RParen, "to close the tuple")?;
                    Ok(Expr::Tuple(elems))
                } else {
                    self.eat(&Tok::RParen, "to close the group")?;
                    Ok(first)
                }
            }
            Tok::LBracket => {
                self.advance();
                let mut elems = Vec::new();
                if !matches!(self.peek(), Tok::RBracket) {
                    loop {
                        elems.push(self.expr()?);
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBracket) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.eat(&Tok::RBracket, "to close the array")?;
                Ok(Expr::Array(elems))
            }
            Tok::LBrace => {
                // record literal: `{ name: expr, age: expr }`
                self.advance();
                let mut fields: Vec<(String, Expr)> = Vec::new();
                if !matches!(self.peek(), Tok::RBrace) {
                    loop {
                        // Field names may be keywords (`match`, `in`, `if`, …) — they
                        // are contextual here, never ambiguous before a `:`.
                        let key = self.member_name("as a record field name")?;
                        self.eat(&Tok::Colon, &format!("after field `{}`", key))
                            .map_err(|e| {
                                e.hint("records look like `{name: \"Ada\", age: 41}`.")
                            })?;
                        let value = self.expr()?;
                        fields.push((key, value));
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBrace) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.eat(&Tok::RBrace, "to close the record")?;
                Ok(Expr::Record(fields))
            }
            other => Err(HelixError::new(
                format!("unexpected {}", other.describe()),
                l,
                c,
            )
            .hint("expected a value here — a number, string, name, or `[...]` array.")),
        }
    }
}

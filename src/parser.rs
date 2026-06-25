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

use crate::ast::{BinOp, Expr, InterpPart, Stmt, TypeAnn, UnOp};
use crate::error::{suggest, HelixError};
use crate::token::{StrSeg, Tok, Token};

/// Lex + parse a single expression (used for `{expr}` interpolation fragments).
pub fn parse_expression(src: &str) -> Result<Expr, HelixError> {
    let tokens = crate::lexer::lex(src)?;
    let mut p = Parser { toks: tokens, pos: 0, depth: 0 };
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

const TYPE_NAMES: &[&str] = &[
    "Int", "Float", "Num", "String", "Bool", "Array", "Tensor", "DataFrame", "Dna",
];

pub fn parse(tokens: Vec<Token>) -> Result<Vec<Stmt>, HelixError> {
    let mut p = Parser { toks: tokens, pos: 0, depth: 0 };
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
            if !matches!(self.peek(), Tok::RParen) {
                loop {
                    let pname = self.ident_name("as a parameter")?;
                    // optional `: Type` annotation
                    let ann = if matches!(self.peek(), Tok::Colon) {
                        self.advance();
                        Some(self.parse_type_ann()?)
                    } else {
                        None
                    };
                    params.push((pname, ann));
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            self.eat(&Tok::RParen, "to close the parameter list")?;
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
        let mut left = self.term()?;
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
            let right = self.term()?;
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
                        let args = self.args()?;
                        self.eat(&Tok::RParen, "to close the argument list")?;
                        e = Expr::Method {
                            recv: Box::new(e),
                            name,
                            args,
                            line: l,
                            col: c,
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
                        let args = self.args()?;
                        self.eat(&Tok::RParen, "to close the argument list")?;
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

    fn args(&mut self) -> Result<Vec<Expr>, HelixError> {
        let mut args = Vec::new();
        if matches!(self.peek(), Tok::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
                if matches!(self.peek(), Tok::RParen) {
                    break; // trailing comma
                }
            } else {
                break;
            }
        }
        Ok(args)
    }

    /// Parse one `match`-arm pattern (v1: a literal, a name to bind, or `_`).
    fn parse_pattern(&mut self) -> Result<crate::ast::Pattern, HelixError> {
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
            other => Err(HelixError::new(
                format!("expected a pattern, found {}", other.describe()),
                l,
                c,
            )
            .hint("a pattern is a literal (`0`, `\"x\"`, `true`, `missing`), a name to bind, or `_`.")),
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
                        StrSeg::Expr(src) => {
                            let e = parse_expression(&src)?;
                            parts.push(InterpPart::Expr(Box::new(e)));
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
                    let pat = self.parse_pattern()?;
                    self.eat(&Tok::FatArrow, "after a match pattern").map_err(|e| {
                        e.hint("each arm is `pattern => result`, e.g. `0 => \"zero\"`.")
                    })?;
                    let result = self.expr()?;
                    arms.push((pat, result));
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
                        let key = self.ident_name("as a record field name")?;
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

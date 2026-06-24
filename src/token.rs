//! Token definitions.

/// A segment of an interpolated string: literal text, or the raw source of an
/// embedded `{expr}` (parsed into an AST later, in the parser).
#[derive(Debug, Clone, PartialEq)]
pub enum StrSeg {
    Lit(String),
    Expr(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // Literals
    Int(i64),
    Float(f64),
    Str(String),
    /// A string containing one or more `{expr}` interpolations.
    InterpStr(Vec<StrSeg>),
    Ident(String),
    True,
    False,

    // Keywords
    Mut,
    Fn,
    Import,
    And,
    Or,
    Not,
    If,
    Then,
    Else,
    Let,
    In,
    Missing,
    Try,

    // Symbols
    Eq,      // =
    EqEq,    // ==
    Ne,      // !=
    Lt,      // <
    Gt,      // >
    Le,      // <=
    Ge,      // >=
    Plus,     // +
    Minus,    // -
    Star,     // *
    StarStar, // **
    Slash,    // /
    Percent,  // %
    Coalesce, // ??
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace, // {
    RBrace, // }
    Comma,
    Dot,
    Colon,    // :
    Arrow,    // ->
    FatArrow, // =>

    Newline,
    Eof,
}

impl Tok {
    /// A human-friendly name for use in error messages.
    pub fn describe(&self) -> String {
        match self {
            Tok::Int(_) => "a number".into(),
            Tok::Float(_) => "a number".into(),
            Tok::Str(_) => "a string".into(),
            Tok::InterpStr(_) => "an interpolated string".into(),
            Tok::Ident(n) => format!("`{}`", n),
            Tok::True => "`true`".into(),
            Tok::False => "`false`".into(),
            Tok::Mut => "`mut`".into(),
            Tok::Fn => "`fn`".into(),
            Tok::Import => "`import`".into(),
            Tok::And => "`and`".into(),
            Tok::Or => "`or`".into(),
            Tok::Not => "`not`".into(),
            Tok::If => "`if`".into(),
            Tok::Then => "`then`".into(),
            Tok::Else => "`else`".into(),
            Tok::Let => "`let`".into(),
            Tok::In => "`in`".into(),
            Tok::Missing => "`missing`".into(),
            Tok::Try => "`try`".into(),
            Tok::Eq => "`=`".into(),
            Tok::EqEq => "`==`".into(),
            Tok::Ne => "`!=`".into(),
            Tok::Lt => "`<`".into(),
            Tok::Gt => "`>`".into(),
            Tok::Le => "`<=`".into(),
            Tok::Ge => "`>=`".into(),
            Tok::Plus => "`+`".into(),
            Tok::Minus => "`-`".into(),
            Tok::Star => "`*`".into(),
            Tok::StarStar => "`**`".into(),
            Tok::Slash => "`/`".into(),
            Tok::Percent => "`%`".into(),
            Tok::Coalesce => "`??`".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBracket => "`[`".into(),
            Tok::RBracket => "`]`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Dot => "`.`".into(),
            Tok::Colon => "`:`".into(),
            Tok::Arrow => "`->`".into(),
            Tok::FatArrow => "`=>`".into(),
            Tok::Newline => "end of line".into(),
            Tok::Eof => "end of file".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
    pub col: usize,
}

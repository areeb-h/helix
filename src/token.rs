//! Token definitions.

/// A segment of an interpolated string: literal text, or the raw source of an
/// embedded `{expr}` (parsed into an AST later, in the parser).
#[derive(Debug, Clone, PartialEq)]
pub enum StrSeg {
    Lit(String),
    /// An interpolation hole: the embedded expression source and an optional format
    /// spec (the text after a top-level `:`, e.g. the `.2f` in `{x:.2f}`).
    Expr(String, Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // Literals
    Int(i64),
    Float(f64),
    /// An INTEGER literal whose magnitude exceeded `i64::MAX`, carrying the `f64` it
    /// degrades to. Distinct from [`Tok::Float`] for exactly one reason: `i64::MIN` has a
    /// magnitude one larger than `i64::MAX`, so `-9223372036854775808` can only be
    /// recognised as an integer if the parser knows the operand was written WITHOUT a
    /// decimal point. Everywhere else it behaves as a float.
    /// The `bool` is true when NEGATING this exact literal yields `i64::MIN` — decided
    /// from the digits, not from the `f64`, because 9223372036854775809 also rounds to
    /// 2^63 and must NOT be mistaken for a value the user can write exactly.
    BigInt(f64, bool),
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
    Match,
    Do,

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
    Star,       // *
    StarStar,   // **
    Slash,      // /
    SlashSlash, // // (integer/floor division)
    Percent,    // %
    Coalesce, // ??
    Amp,      // & (bitwise and)
    Caret,    // ^ (bitwise xor)
    Shl,      // << (shift left)
    Shr,      // >> (shift right)
    At,       // @ (DataFrame column sigil: `@age`)
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace, // {
    RBrace, // }
    Comma,
    Dot,
    DotDot,    // .. (range literal: `0..n`)
    DotDotDot, // ... (record spread: `{ ...base, k: v }`)
    Colon,     // :
    Arrow,    // ->
    FatArrow, // =>
    Pipe,     // | (match-pattern alternatives)

    Newline,
    Eof,
}

impl Tok {
    /// A human-friendly name for use in error messages.
    pub fn describe(&self) -> String {
        match self {
            Tok::Int(_) => "a number".into(),
            Tok::Float(_) | Tok::BigInt(..) => "a number".into(),
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
            Tok::Match => "`match`".into(),
            Tok::Do => "`do`".into(),
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
            Tok::SlashSlash => "`//`".into(),
            Tok::Percent => "`%`".into(),
            Tok::Coalesce => "`??`".into(),
            Tok::Amp => "`&`".into(),
            Tok::Caret => "`^`".into(),
            Tok::Shl => "`<<`".into(),
            Tok::Shr => "`>>`".into(),
            Tok::At => "`@`".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBracket => "`[`".into(),
            Tok::RBracket => "`]`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Dot => "`.`".into(),
            Tok::DotDot => "`..`".into(),
            Tok::DotDotDot => "`...`".into(),
            Tok::Colon => "`:`".into(),
            Tok::Arrow => "`->`".into(),
            Tok::FatArrow => "`=>`".into(),
            Tok::Pipe => "`|`".into(),
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

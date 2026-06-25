//! Abstract syntax tree.
//!
//! Nodes that can fail at runtime carry a (line, col) so the interpreter can
//! point errors back at the source.

#[derive(Debug, Clone)]
pub enum UnOp {
    Neg,
    Not,
}

/// A writable type annotation on a function signature (the surface grammar).
/// Kept separate from the checker's richer internal `Type` (which also has
/// `Num`/`Unknown`/`Missing`/`Function`/`GroupBy`, which users can't write).
#[derive(Debug, Clone)]
pub enum TypeAnn {
    Int,
    Float,
    Num,
    String,
    Bool,
    Array,
    Tensor,
    DataFrame,
    Dna,
}

#[derive(Debug, Clone)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    /// `a ?? b` — `b` when `a` is `missing`, else `a`.
    Coalesce,
}

impl BinOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Gt => ">",
            BinOp::Le => "<=",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::Coalesce => "??",
        }
    }
}

/// One piece of an interpolated string.
#[derive(Debug, Clone)]
pub enum InterpPart {
    Lit(String),
    Expr(Box<Expr>),
}

/// A `match`-arm pattern (v1: refutable literals, an irrefutable binding, and the
/// wildcard). A `Bind` or `Wildcard` matches anything (so it must come last); a
/// literal matches only an equal value.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// `_` — matches anything, binds nothing.
    Wildcard,
    /// `name` — matches anything, binds the value to `name`.
    Bind(String),
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    /// `missing` — matches only an absent value.
    Missing,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    /// An interpolated string: literal chunks interleaved with `{expr}` parts.
    Interp(Vec<InterpPart>),
    /// The `missing` literal — absent data.
    Missing,
    Ident {
        name: String,
        line: usize,
        col: usize,
    },
    Array(Vec<Expr>),
    /// A tuple literal: `(a, b)`, `(x,)`. Fixed-size, heterogeneous.
    Tuple(Vec<Expr>),
    /// A record literal: `{name: "Ada", age: 41}` (ordered, identifier keys).
    Record(Vec<(String, Expr)>),
    /// Field access on a record: `r.name` (no parens — `r.method()` is a `Method`).
    Field {
        recv: Box<Expr>,
        name: String,
        line: usize,
        col: usize,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
        line: usize,
        col: usize,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
        line: usize,
        col: usize,
    },
    /// Free function call, e.g. `print(x)`, `dna("ATGC")`.
    Call {
        name: String,
        args: Vec<Expr>,
        line: usize,
        col: usize,
    },
    /// Method call, e.g. `xs.mean()`, `seq.kmers(3)`.
    Method {
        recv: Box<Expr>,
        name: String,
        args: Vec<Expr>,
        line: usize,
        col: usize,
    },
    /// Indexing, e.g. `xs[0]`, `xs[-1]`.
    Index {
        recv: Box<Expr>,
        index: Box<Expr>,
        line: usize,
        col: usize,
    },
    /// Slicing, e.g. `xs[1:3]`, `xs[:n]`, `xs[::2]`, `xs[::-1]` (Python semantics).
    Slice {
        recv: Box<Expr>,
        start: Option<Box<Expr>>,
        stop: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
        line: usize,
        col: usize,
    },
    /// `x => body` or `(a, b) => body` — an anonymous function. Currently only
    /// meaningful as an argument to `map`/`filter`/`where`/`reduce`; `it` is the
    /// implicit one-parameter shorthand.
    Lambda {
        params: Vec<String>,
        body: Box<Expr>,
    },
    /// `let a = x, b = y in body` — local bindings scoped to `body` (an
    /// expression). Bindings are sequential: later ones can use earlier ones.
    Let {
        bindings: Vec<(String, Expr)>,
        body: Box<Expr>,
    },
    /// `if cond then a else b` — a value-producing expression, not a statement.
    If {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
        line: usize,
        col: usize,
    },
    /// `try EXPR` — evaluate `EXPR`, catching any runtime error. Yields a record
    /// `{ok, value, error}`: on success `{ok: true, value: <result>, error: missing}`,
    /// on a runtime error `{ok: false, value: missing, error: <message>}`.
    Try {
        expr: Box<Expr>,
        line: usize,
        col: usize,
    },
    /// `match e { pat => result, ... }` — try each arm's pattern against `e` in
    /// order; the first that matches binds its variables and yields its result. A
    /// value-producing expression (like `if`), not a statement.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<(Pattern, Expr)>,
        line: usize,
        col: usize,
    },
}

impl Expr {
    /// The source position of this expression, for error reporting. Variants that
    /// carry an explicit `(line, col)` return it; the few that don't (literals,
    /// `let`/lambda) return `(0, 0)` — those can't be the source of a positioned
    /// runtime render error in practice.
    pub fn position(&self) -> (usize, usize) {
        match self {
            Expr::Ident { line, col, .. }
            | Expr::Field { line, col, .. }
            | Expr::Unary { line, col, .. }
            | Expr::Binary { line, col, .. }
            | Expr::Call { line, col, .. }
            | Expr::Method { line, col, .. }
            | Expr::Index { line, col, .. }
            | Expr::Slice { line, col, .. }
            | Expr::If { line, col, .. }
            | Expr::Try { line, col, .. }
            | Expr::Match { line, col, .. } => (*line, *col),
            _ => (0, 0),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `x = expr` (mutable=false) or `mut x = expr` (mutable=true).
    /// Whether this defines a new binding or reassigns is resolved at runtime.
    Assign {
        name: String,
        mutable: bool,
        value: Expr,
        line: usize,
        col: usize,
    },
    /// `a, b = expr` — destructure a tuple/array into multiple bindings.
    Destructure {
        names: Vec<String>,
        mutable: bool,
        value: Expr,
        line: usize,
        col: usize,
    },
    /// `fn name(a: T, b) -> R = expr` — a named function definition.
    /// Parameter and return annotations are optional (inferred when absent).
    Func {
        name: String,
        params: Vec<(String, Option<TypeAnn>)>,
        ret: Option<TypeAnn>,
        body: Expr,
        line: usize,
        col: usize,
    },
    /// `import a.b.c [as alias]` — load the module at the relative path
    /// `a/b/c.helix` and make its public (top-level) definitions reachable as
    /// `alias.member` (the alias defaults to the last path segment, `c`). Resolved
    /// and stripped by the module loader before type-checking; the rest of the
    /// pipeline never sees it.
    Import {
        /// Path segments, e.g. `["math", "stats"]` for `import math.stats`.
        segments: Vec<String>,
        /// The namespace the module is reached through — the `as` name, or the
        /// last path segment when no `as` clause is given. Unused for a selective
        /// import, which binds the chosen names directly rather than a namespace.
        alias: String,
        /// A selective import (`import math.stats.{mean, std}`) brings these names
        /// into scope unqualified; `None` imports the whole module as `alias`.
        selected: Option<Vec<String>>,
        line: usize,
        col: usize,
    },
    Expr(Expr),
}

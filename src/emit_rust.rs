//! `helix emit rust` (ADR 0065): a rule written in Helix, emitted as plain Rust.
//!
//! A RULE is a set of pure functions over records, maps, lists, options, integers, strings and
//! booleans: `(state, inputs) -> (state, outputs)`. Helix is where it is written, checked
//! (`helix check`), proved free of effects (`helix effects`) and tested on three engines. What a
//! privileged program ships is the Rust this module writes: `std` only, no `unsafe`, no
//! dependencies, readable, one Rust function per Helix function.
//!
//! THE CONTRACT IS WRITTEN IN THE SOURCE. Rust needs a type for every value and Helix's records
//! are structural, so a rule declares its Rust shapes in `## rust:` lines, in Rust's own syntax:
//!
//! ```text
//! ## rust: pub enum Action { Freeze(String), Thaw(String) }
//! ## rust: pub struct Policy { after: i64, apps: BTreeMap<String, App> }
//! ## rust: impl Policy
//! ## rust: pub fn update(&mut self, now_ms: u64, open: &[String]) -> Vec<Action>
//! export fn update(self, now_ms, open) = …
//! ```
//!
//! A `## rust: fn` line sits above the Helix function it describes. `&mut self` means the Helix
//! function takes the state first and returns `(new_state, output)` — or just the new state
//! when the Rust method returns nothing; `&self` means it returns the output; `-> Self` with no
//! receiver builds the state. Inside, integers are `i64` (Helix's), and the public methods
//! convert at the boundary (`u64` times, `&[String]`, `Option<&str>`).
//!
//! WHAT IS TRANSLATED, AND HOW IT KEEPS HELIX'S MEANING:
//! - a record of a declared struct is that struct; `{Freeze: id}` is the enum variant
//!   `Action::Freeze(id)`; a record update `{...s, f: v}` is `S { f: v, ..s }`;
//! - a Dict is a `BTreeMap`, so iteration is in key order, as in Helix;
//! - `missing` is `None`, a present value where an `Option` is expected is `Some(v)`, `a ?? b`
//!   is `unwrap_or`, and `if x.is_missing() then … else …` unwraps `x` in the else branch;
//! - integer arithmetic wraps (`wrapping_add`), as Helix's does;
//! - where Helix would raise (`min` of an empty list), the Rust panics with the same message.
//!
//! Anything else is refused with the line and a reason, so an emitted file is never a guess.
//! And the Rust is CHECKED AGAINST THE HELIX: the emitter generates random call sequences,
//! runs them through the Helix functions, and writes each as a Rust `#[test]` that must give
//! the same outputs.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::ast::{BinOp, Expr, Pattern, RecordPart, Stmt, UnOp};

/// A type inside the emitted code.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Int,
    Bool,
    Str,
    Opt(Box<Ty>),
    Vec(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Tuple(Vec<Ty>),
    Named(String),
    Unit,
}

impl Ty {
    fn rust(&self) -> String {
        match self {
            Ty::Int => "i64".into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "String".into(),
            Ty::Opt(t) => format!("Option<{}>", t.rust()),
            Ty::Vec(t) => format!("Vec<{}>", t.rust()),
            Ty::Map(k, v) => format!("BTreeMap<{}, {}>", k.rust(), v.rust()),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(Ty::rust).collect::<Vec<_>>().join(", ")),
            Ty::Named(n) => n.clone(),
            Ty::Unit => "()".into(),
        }
    }
    fn copy(&self) -> bool {
        match self {
            Ty::Int | Ty::Bool | Ty::Unit => true,
            Ty::Opt(t) => t.copy(),
            Ty::Tuple(ts) => ts.iter().all(Ty::copy),
            _ => false,
        }
    }
}

/// An error, with the Helix line it is about (0 when it is about a declaration).
#[derive(Debug)]
pub struct EmitError {
    pub line: usize,
    pub message: String,
}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, EmitError> {
    Err(EmitError { line, message: message.into() })
}

// --- The `## rust:` declarations. ---------------------------------------------------------

#[derive(Debug, Clone)]
enum Decl {
    Struct { name: String, public: bool, fields: Vec<(String, Ty)> },
    Enum { name: String, public: bool, variants: Vec<(String, Option<Ty>)> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Recv {
    None,
    Ref,
    Mut,
}

/// A parameter or return type as the Rust signature writes it.
#[derive(Debug, Clone)]
struct Api {
    text: String,
    inner: Ty,
}

impl Api {
    /// `Save<T>`'s T.
    fn saved(&self) -> Option<&str> {
        self.text.strip_prefix("Save<")?.strip_suffix('>')
    }
    /// How the public method writes this type.
    fn public_text(&self, as_param: bool) -> String {
        match self.saved() {
            Some(_) if as_param => "Option<&str>".into(),
            Some(_) => "String".into(),
            None => self.text.clone(),
        }
    }
    /// The type the rule's function takes for it.
    fn param_ty(&self) -> Ty {
        if self.saved().is_some() { Ty::Opt(Box::new(self.inner.clone())) } else { self.inner.clone() }
    }
}

#[derive(Debug, Clone)]
struct Sig {
    public: bool,
    name: String,
    recv: Recv,
    params: Vec<(String, Api)>,
    ret: Option<Api>,
    /// The `##` comment above the signature: the method's documentation.
    doc: Vec<String>,
}

/// Tokens of the `## rust:` mini-syntax.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if c == '-' && chars.peek() == Some(&'>') {
            chars.next();
            out.push("->".into());
        } else if !c.is_whitespace() {
            out.push(c.to_string());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

struct TyParser<'a> {
    toks: &'a [String],
    at: usize,
    self_ty: Option<&'a str>,
}

impl TyParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.at).map(String::as_str)
    }
    fn eat(&mut self, t: &str) -> bool {
        if self.peek() == Some(t) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &str) -> Result<(), String> {
        if self.eat(t) { Ok(()) } else { Err(format!("expected `{t}`, found `{}`", self.peek().unwrap_or("the end"))) }
    }
    fn ident(&mut self) -> Result<String, String> {
        match self.peek().map(str::to_string) {
            Some(t) if t.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') => {
                self.at += 1;
                Ok(t)
            }
            other => Err(format!("expected a name, found `{}`", other.as_deref().unwrap_or("the end"))),
        }
    }
    /// A type, as written (`&[String]`), and the type it is inside (`Vec<String>`).
    fn ty(&mut self) -> Result<Api, String> {
        let start = self.at;
        let inner = self.ty_inner()?;
        let text = render_tokens(&self.toks[start..self.at]);
        Ok(Api { text, inner })
    }
    fn ty_inner(&mut self) -> Result<Ty, String> {
        if self.eat("&") {
            if self.eat("[") {
                let t = self.ty_inner()?;
                self.expect("]")?;
                return Ok(Ty::Vec(Box::new(t)));
            }
            if self.eat("str") {
                return Ok(Ty::Str);
            }
            return Err("a reference is `&str` or `&[T]` here".into());
        }
        if self.eat("(") {
            let mut ts = Vec::new();
            while !self.eat(")") {
                ts.push(self.ty_inner()?);
                self.eat(",");
            }
            return Ok(if ts.is_empty() { Ty::Unit } else { Ty::Tuple(ts) });
        }
        let name = self.ident()?;
        let mut args = Vec::new();
        if self.eat("<") {
            while !self.eat(">") {
                args.push(self.ty_inner()?);
                self.eat(",");
            }
        }
        let one = |args: &mut Vec<Ty>| args.pop().ok_or_else(|| format!("`{name}` needs a type argument"));
        Ok(match (name.as_str(), args.len()) {
            ("i64" | "u64" | "i32" | "u32" | "usize" | "isize", 0) => Ty::Int,
            ("bool", 0) => Ty::Bool,
            ("String" | "str", 0) => Ty::Str,
            ("Option", 1) => Ty::Opt(Box::new(one(&mut args)?)),
            ("Vec", 1) => Ty::Vec(Box::new(one(&mut args)?)),
            ("BTreeMap", 2) => {
                let v = one(&mut args)?;
                let k = one(&mut args)?;
                Ty::Map(Box::new(k), Box::new(v))
            }
            ("Self", 0) => Ty::Named(self.self_ty.ok_or("`Self` needs a `## rust: impl Name` line above")?.to_string()),
            // `Save<T>`: T as text that survives a restart. A result is the text; a parameter
            // is the text that was read back, if any — `None` when it was missing or damaged.
            ("Save", 1) => match one(&mut args)? {
                Ty::Named(n) => Ty::Named(n),
                _ => return Err("`Save<T>` saves a declared struct".into()),
            },
            (n, 0) if n.chars().next().is_some_and(char::is_uppercase) => Ty::Named(n.to_string()),
            (n, _) => return Err(format!("`{n}` is not a type a rule can use: i64/u64, bool, String/&str, Option, Vec/&[T], BTreeMap, tuples, and declared structs and enums")),
        })
    }
}

fn render_tokens(ts: &[String]) -> String {
    let mut s = String::new();
    for (i, t) in ts.iter().enumerate() {
        if t == "," {
            s.push_str(", ");
        } else {
            s.push_str(t);
        }
        let _ = i;
    }
    s
}

/// What a rule file declares.
#[derive(Debug, Default)]
pub struct Contract {
    decls: Vec<Decl>,
    self_ty: Option<String>,
    sigs: BTreeMap<String, Sig>,
    traces: usize,
    domains: BTreeMap<String, Vec<String>>,
    doc: Vec<String>,
}

fn parse_contract(src: &str) -> Result<Contract, EmitError> {
    let mut c = Contract { traces: 48, ..Default::default() };
    let lines: Vec<&str> = src.lines().collect();
    let mut in_head = true;
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let n = i + 1;
        if in_head {
            match line.strip_prefix("##") {
                Some(d) if !d.trim_start().starts_with("rust") => c.doc.push(d.strip_prefix(' ').unwrap_or(d).to_string()),
                Some(_) => {}
                None => in_head = false,
            }
        }
        if let Some(n_traces) = line.strip_prefix("## rust-traces:") {
            c.traces = n_traces.trim().parse().map_err(|_| EmitError { line: n, message: "`## rust-traces:` takes a number".into() })?;
            continue;
        }
        if let Some(d) = line.strip_prefix("## rust-domain:") {
            let Some((ty, vals)) = d.split_once('=') else { return err(n, "write `## rust-domain: String = a, b, c`") };
            c.domains.insert(ty.trim().to_string(), vals.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect());
            continue;
        }
        let Some(decl) = line.strip_prefix("## rust:") else { continue };
        let toks = tokens(decl);
        let fail = |m: String| EmitError { line: n, message: m };
        let mut p = TyParser { toks: &toks, at: 0, self_ty: c.self_ty.as_deref() };
        let public = p.eat("pub");
        match p.peek() {
            Some("impl") => {
                p.at += 1;
                c.self_ty = Some(p.ident().map_err(fail)?);
            }
            Some("struct") => {
                p.at += 1;
                let name = p.ident().map_err(fail)?;
                p.expect("{").map_err(fail)?;
                let mut fields = Vec::new();
                while !p.eat("}") {
                    let f = p.ident().map_err(fail)?;
                    p.expect(":").map_err(fail)?;
                    fields.push((f, p.ty().map_err(fail)?.inner));
                    p.eat(",");
                }
                c.decls.push(Decl::Struct { name, public, fields });
            }
            Some("enum") => {
                p.at += 1;
                let name = p.ident().map_err(fail)?;
                p.expect("{").map_err(fail)?;
                let mut variants = Vec::new();
                while !p.eat("}") {
                    let v = p.ident().map_err(fail)?;
                    let payload = if p.eat("(") {
                        let t = p.ty().map_err(fail)?.inner;
                        p.expect(")").map_err(fail)?;
                        Some(t)
                    } else {
                        None
                    };
                    variants.push((v, payload));
                    p.eat(",");
                }
                c.decls.push(Decl::Enum { name, public, variants });
            }
            Some("fn") => {
                p.at += 1;
                let name = p.ident().map_err(fail)?;
                p.expect("(").map_err(fail)?;
                let mut recv = Recv::None;
                let mut params = Vec::new();
                if p.eat("&") {
                    recv = if p.eat("mut") { Recv::Mut } else { Recv::Ref };
                    p.expect("self").map_err(fail)?;
                    p.eat(",");
                }
                while !p.eat(")") {
                    let pn = p.ident().map_err(fail)?;
                    p.expect(":").map_err(fail)?;
                    params.push((pn, p.ty().map_err(fail)?));
                    p.eat(",");
                }
                let ret = if p.eat("->") { Some(p.ty().map_err(fail)?) } else { None };
                if recv != Recv::None && c.self_ty.is_none() {
                    return err(n, "a method needs a `## rust: impl Name` line above it");
                }
                // The Helix function this describes is the next `fn` line.
                let helix = lines[i + 1..]
                    .iter()
                    .map(|l| l.trim())
                    .find(|l| !l.starts_with('#'))
                    .and_then(|l| l.strip_prefix("export ").unwrap_or(l).strip_prefix("fn "))
                    .and_then(|l| l.split('(').next())
                    .map(|s| s.trim().to_string());
                let Some(helix) = helix.filter(|h| *h == name) else {
                    return err(n, format!("`## rust: fn {name}` must sit directly above `fn {name}(…)`"));
                };
                // Its documentation: the `##` lines just above, other than `## rust` ones.
                let mut doc: Vec<String> = lines[..i]
                    .iter()
                    .rev()
                    .map(|l| l.trim())
                    .take_while(|l| l.starts_with("##"))
                    .filter(|l| !l.starts_with("## rust"))
                    .map(|l| l.trim_start_matches('#').trim().to_string())
                    .collect();
                doc.reverse();
                c.sigs.insert(helix, Sig { public, name, recv, params, ret, doc });
            }
            _ => return err(n, "a `## rust:` line declares a `struct`, an `enum`, an `impl`, or a `fn` signature"),
        }
    }
    Ok(c)
}

// --- Translating expressions. -------------------------------------------------------------

/// A variable in scope: its Rust name, type, and how many references stand between it and the
/// value (a lambda's parameter over `iter()` is a reference).
#[derive(Clone)]
struct Var {
    rust: String,
    ty: Ty,
    refs: usize,
}

#[derive(Clone, Default)]
struct Env {
    vars: Vec<(String, Var)>,
}

impl Env {
    fn get(&self, name: &str) -> Option<&Var> {
        self.vars.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v)
    }
    fn with(&self, name: &str, v: Var) -> Env {
        let mut e = self.clone();
        e.vars.push((name.to_string(), v));
        e
    }
}

const KEYWORDS: &[&str] = &[
    "self", "Self", "type", "fn", "match", "ref", "mod", "move", "mut", "impl", "trait", "struct", "enum", "use", "where", "loop", "while",
    "for", "in", "let", "const", "static", "crate", "super", "as", "box", "dyn", "async", "await", "pub", "return", "break", "continue",
];

fn rust_name(name: &str) -> String {
    if name == "self" {
        "state".into()
    } else if KEYWORDS.contains(&name) {
        format!("{name}_")
    } else {
        name.replace('$', "_")
    }
}

struct Tx<'a> {
    c: &'a Contract,
    /// Each Helix function's internal parameter and return types.
    fns: BTreeMap<String, (Vec<Ty>, Ty)>,
    /// A path that an `is_missing` test unwrapped, and the variable holding the value.
    narrowed: Vec<(String, Var)>,
}

/// A path the narrowing can name: `x` or `x.f.g`.
fn path_of(e: &Expr) -> Option<String> {
    match e {
        Expr::Ident { name, .. } => Some(name.clone()),
        Expr::Field { recv, name, .. } => Some(format!("{}.{name}", path_of(recv)?)),
        _ => None,
    }
}

/// Whether `e` refers to the path `p` anywhere.
fn mentions(e: &Expr, p: &str) -> bool {
    if path_of(e).as_deref() == Some(p) {
        return true;
    }
    let mut found = false;
    let mut visit = |x: &Expr| {
        if path_of(x).as_deref() == Some(p) {
            found = true;
        }
    };
    crate::visit::walk_expr(e, &mut visit);
    found
}

fn line_of(e: &Expr) -> usize {
    e.position().0
}

impl<'a> Tx<'a> {
    fn struct_fields(&self, name: &str) -> Option<&Vec<(String, Ty)>> {
        self.c.decls.iter().find_map(|d| match d {
            Decl::Struct { name: n, fields, .. } if n == name => Some(fields),
            _ => None,
        })
    }
    fn variant(&self, v: &str) -> Option<(String, Option<Ty>)> {
        self.c.decls.iter().find_map(|d| match d {
            Decl::Enum { name, variants, .. } => variants.iter().find(|(n, _)| n == v).map(|(_, p)| (name.clone(), p.clone())),
            _ => None,
        })
    }

    /// `code` of type `got`, where `want` is expected: the same, or wrapped in `Some`.
    fn coerce(&self, code: String, got: Ty, want: Option<&Ty>, line: usize) -> Result<(String, Ty), EmitError> {
        match want {
            None => Ok((code, got)),
            Some(w) if *w == got => Ok((code, got)),
            Some(Ty::Opt(inner)) if **inner == got => Ok((format!("Some({code})"), Ty::Opt(inner.clone()))),
            Some(w) => err(line, format!("this is {} here, where {} is expected", got.rust(), w.rust())),
        }
    }

    /// A place (a variable or a field path) without copying it, and its type.
    fn place(&self, e: &Expr, env: &Env) -> Option<(String, Ty, usize)> {
        if let Some(p) = path_of(e)
            && let Some((_, v)) = self.narrowed.iter().rev().find(|(n, _)| *n == p)
        {
            return Some((v.rust.clone(), v.ty.clone(), v.refs));
        }
        match e {
            Expr::Ident { name, .. } => env.get(name).map(|v| (v.rust.clone(), v.ty.clone(), v.refs)),
            Expr::Field { recv, name, .. } => {
                let (code, ty, _) = self.place(recv, env)?;
                let Ty::Named(s) = ty else { return None };
                let fty = self.struct_fields(&s)?.iter().find(|(f, _)| f == name)?.1.clone();
                Some((format!("{code}.{name}"), fty, 0))
            }
            Expr::Index { recv, index, .. } => {
                let (code, ty, _) = self.place(recv, env)?;
                match (ty, index.as_ref()) {
                    (Ty::Tuple(ts), Expr::Int(i)) if (*i as usize) < ts.len() => Some((format!("{code}.{i}"), ts[*i as usize].clone(), 0)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// A value of `e` (owned).
    fn value(&mut self, e: &Expr, env: &Env, want: Option<&Ty>) -> Result<(String, Ty), EmitError> {
        let line = line_of(e);
        if let Some((code, ty, refs)) = self.place(e, env) {
            let stars = "*".repeat(refs);
            let code = if ty.copy() {
                format!("{stars}{code}")
            } else if refs > 0 {
                format!("({stars}{code}).clone()")
            } else {
                format!("{code}.clone()")
            };
            return self.coerce(code, ty, want, line);
        }
        let (code, ty) = self.expr(e, env, want)?;
        self.coerce(code, ty, want, line)
    }

    /// A place to read from by reference (`&x` makes sense of it): a place, or a value.
    fn borrowable(&mut self, e: &Expr, env: &Env) -> Result<(String, Ty), EmitError> {
        if let Some((code, ty, refs)) = self.place(e, env) {
            return Ok((if refs > 0 { format!("{}{code}", "*".repeat(refs)) } else { code }, ty));
        }
        let (code, ty) = self.expr(e, env, None)?;
        Ok((paren(&code), ty))
    }

    /// `&x` for an argument read by reference (`contains`, `get`): the place itself, not a copy.
    fn by_ref(&mut self, e: &Expr, env: &Env, want: Option<&Ty>) -> Result<String, EmitError> {
        if let Some((code, _, refs)) = self.place(e, env) {
            return Ok(if refs > 1 { format!("{}{code}", "*".repeat(refs - 1)) } else if refs == 1 { code } else { format!("&{code}") });
        }
        let (code, _) = self.value(e, env, want)?;
        Ok(format!("&{}", paren(&code)))
    }

    fn expr(&mut self, e: &Expr, env: &Env, want: Option<&Ty>) -> Result<(String, Ty), EmitError> {
        let line = line_of(e);
        match e {
            Expr::Int(n) => Ok((if *n < 0 { format!("({n})") } else { n.to_string() }, Ty::Int)),
            Expr::Bool(b) => Ok((b.to_string(), Ty::Bool)),
            Expr::Str(s) => Ok((format!("String::from({s:?})"), Ty::Str)),
            Expr::Missing => match want {
                Some(Ty::Opt(t)) => Ok(("None".into(), Ty::Opt(t.clone()))),
                _ => err(line, "`missing` needs an Option here: declare the field or result as `Option<…>`"),
            },
            Expr::Ident { name, .. } => err(line, format!("`{name}` is not defined here")),
            Expr::Array(items) => {
                let elem = match want {
                    Some(Ty::Vec(t)) => Some((**t).clone()),
                    _ => None,
                };
                if items.is_empty() {
                    return match elem {
                        Some(t) => Ok(("Vec::new()".into(), Ty::Vec(Box::new(t)))),
                        None => err(line, "an empty list needs a type here (it is the other branch's, or the declared one)"),
                    };
                }
                let mut codes = Vec::new();
                let mut ety = elem;
                for it in items {
                    let (c, t) = self.value(it, env, ety.as_ref())?;
                    ety = Some(t);
                    codes.push(c);
                }
                Ok((format!("vec![{}]", codes.join(", ")), Ty::Vec(Box::new(ety.unwrap_or(Ty::Unit)))))
            }
            Expr::Tuple(items) => {
                let wants: Vec<Option<Ty>> = match want {
                    Some(Ty::Tuple(ts)) if ts.len() == items.len() => ts.iter().cloned().map(Some).collect(),
                    _ => vec![None; items.len()],
                };
                let mut codes = Vec::new();
                let mut tys = Vec::new();
                for (it, w) in items.iter().zip(wants) {
                    let (c, t) = self.value(it, env, w.as_ref())?;
                    codes.push(c);
                    tys.push(t);
                }
                Ok((format!("({})", codes.join(", ")), Ty::Tuple(tys)))
            }
            Expr::Record(fields) => self.record(fields, None, env, want, line),
            Expr::RecordUpdate { parts, line, .. } => {
                let mut base = None;
                let mut fields = Vec::new();
                for p in parts {
                    match p {
                        RecordPart::Spread(b) if base.is_none() && fields.is_empty() => base = Some(b),
                        RecordPart::Spread(_) => return err(*line, "a record update has one `...base`, first"),
                        RecordPart::Field(n, v) => fields.push((n.clone(), v.clone())),
                    }
                }
                self.record(&fields, base, env, want, *line)
            }
            Expr::Field { recv, name, .. } => {
                let (_, rty) = self.borrowable(recv, env)?;
                match rty {
                    Ty::Opt(_) => err(line, format!("`.{name}` of a value that may be missing: test `.is_missing()` first, or give a default with `??`")),
                    _ => err(line, format!("`.{name}` is not a field of {}", rty.rust())),
                }
            }
            Expr::Index { recv, index, .. } => {
                let (rc, rty) = self.borrowable(recv, env)?;
                match rty {
                    Ty::Vec(t) => {
                        let (ic, _) = self.value(index, env, Some(&Ty::Int))?;
                        let v = format!("{rc}[{ic} as usize]");
                        Ok((if t.copy() { v } else { format!("{v}.clone()") }, *t))
                    }
                    other => err(line, format!("indexing {} is not supported in a rule (a tuple takes a literal index)", other.rust())),
                }
            }
            Expr::Unary { op, expr, .. } => match op {
                UnOp::Not => {
                    let (c, _) = self.value(expr, env, Some(&Ty::Bool))?;
                    Ok((format!("!{}", paren(&c)), Ty::Bool))
                }
                UnOp::Neg => {
                    let (c, _) = self.value(expr, env, Some(&Ty::Int))?;
                    Ok((format!("{}.wrapping_neg()", paren(&c)), Ty::Int))
                }
            },
            Expr::Binary { op, left, right, .. } => self.binary(*op, left, right, env, want, line),
            Expr::Call { name, args, .. } => self.call(name, args, env, line),
            Expr::Method { recv, name, args, .. } => self.method(recv, name, args, env, want, line),
            Expr::Let { bindings, body, .. } => {
                let mut env = env.clone();
                let mut out = String::from("{\n");
                for (name, value) in bindings {
                    let (c, t) = self.value(value, &env, None)?;
                    if name.starts_with("$do") {
                        let _ = writeln!(out, "let _ = {c};");
                        continue;
                    }
                    let r = rust_name(name);
                    let _ = writeln!(out, "let {r} = {c};");
                    env = env.with(name, Var { rust: r, ty: t, refs: 0 });
                }
                let (c, t) = self.value(body, &env, want)?;
                let _ = write!(out, "{c}\n}}");
                Ok((out, t))
            }
            Expr::If { cond, then_branch, else_branch, line, .. } => self.if_(cond, then_branch, else_branch, env, want, *line),
            Expr::Match { scrutinee, arms, line, .. } => {
                let (sc, sty) = self.value(scrutinee, env, None)?;
                let scr = match sty {
                    Ty::Str => format!("{}.as_str()", paren(&sc)),
                    Ty::Int | Ty::Bool => sc,
                    other => return err(*line, format!("a rule matches on strings, integers or booleans, not {}", other.rust())),
                };
                let mut out = format!("match {scr} {{\n");
                let mut ty = want.cloned();
                for arm in arms {
                    if arm.guard.is_some() {
                        return err(*line, "a rule's `match` arms have no guards; use `if`");
                    }
                    let pat = match &arm.pattern {
                        Pattern::Wildcard => "_".to_string(),
                        Pattern::Int(n) => n.to_string(),
                        Pattern::Str(s) => format!("{s:?}"),
                        Pattern::Bool(b) => b.to_string(),
                        _ => return err(*line, "a rule's `match` arms are literals or `_`"),
                    };
                    let (c, t) = self.value(&arm.body, env, ty.as_ref())?;
                    ty = Some(t);
                    let _ = writeln!(out, "{pat} => {c},");
                }
                out.push('}');
                Ok((out, ty.unwrap_or(Ty::Unit)))
            }
            Expr::Lambda { .. } => err(line, "a function value is only used as the argument of `map`, `filter`, `any`, `all` or `reduce` in a rule"),
            _ => err(line, "this expression is outside what a rule can emit (records, maps, lists, options, integers, strings, booleans)"),
        }
    }

    fn record(
        &mut self,
        fields: &[(String, Expr)],
        base: Option<&Expr>,
        env: &Env,
        want: Option<&Ty>,
        line: usize,
    ) -> Result<(String, Ty), EmitError> {
        // `{Freeze: id}`: an enum variant.
        if base.is_none()
            && fields.len() == 1
            && fields[0].0.chars().next().is_some_and(char::is_uppercase)
            && let Some((enum_name, payload)) = self.variant(&fields[0].0)
        {
            let code = match payload {
                Some(pt) => {
                    let (c, _) = self.value(&fields[0].1, env, Some(&pt))?;
                    format!("{enum_name}::{}({c})", fields[0].0)
                }
                None => format!("{enum_name}::{}", fields[0].0),
            };
            return Ok((code, Ty::Named(enum_name)));
        }
        let (base_code, base_ty) = match base {
            Some(b) => {
                let (c, t) = self.value(b, env, None)?;
                (Some(c), Some(t))
            }
            None => (None, None),
        };
        let want_struct = match (base_ty, want) {
            (Some(Ty::Named(n)), _) => Some(n),
            (_, Some(Ty::Named(n))) => Some(n.clone()),
            (_, Some(Ty::Opt(t))) => match t.as_ref() {
                Ty::Named(n) => Some(n.clone()),
                _ => None,
            },
            _ => None,
        };
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        let sname = match want_struct {
            Some(n) => n,
            None => {
                let found: Vec<String> = self
                    .c
                    .decls
                    .iter()
                    .filter_map(|d| match d {
                        Decl::Struct { name, fields: fs, .. } if fs.len() == names.len() && fs.iter().all(|(f, _)| names.contains(&f.as_str())) => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
                match found.as_slice() {
                    [one] => one.clone(),
                    [] => return err(line, format!("no declared struct has exactly the fields {}", names.join(", "))),
                    _ => return err(line, "more than one declared struct has these fields; give it a type through a signature"),
                }
            }
        };
        let decl = self.struct_fields(&sname).cloned().ok_or_else(|| EmitError { line, message: format!("`{sname}` is not a declared struct") })?;
        let mut parts = Vec::new();
        for (f, v) in fields {
            let Some((_, fty)) = decl.iter().find(|(n, _)| n == f) else {
                return err(line, format!("`{sname}` has no field `{f}`"));
            };
            let (c, _) = self.value(v, env, Some(fty))?;
            parts.push(if c == format!("{f}.clone()") || c == *f { f.clone() } else { format!("{f}: {c}") });
        }
        match base_code {
            Some(b) => {
                if fields.len() == decl.len() {
                    return err(line, "this update sets every field; build the record instead");
                }
                parts.push(format!("..{b}"));
            }
            None => {
                if let Some((missing, _)) = decl.iter().find(|(n, _)| !names.contains(&n.as_str())) {
                    return err(line, format!("`{sname}` needs `{missing}` too"));
                }
            }
        }
        Ok((format!("{sname} {{ {} }}", parts.join(", ")), Ty::Named(sname)))
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, env: &Env, want: Option<&Ty>, line: usize) -> Result<(String, Ty), EmitError> {
        match op {
            BinOp::And | BinOp::Or => {
                let (a, _) = self.value(l, env, Some(&Ty::Bool))?;
                let (b, _) = self.value(r, env, Some(&Ty::Bool))?;
                let o = if op == BinOp::And { "&&" } else { "||" };
                Ok((format!("{} {o} {}", paren(&a), paren(&b)), Ty::Bool))
            }
            BinOp::Coalesce => {
                let (a, at) = self.value(l, env, None)?;
                let Ty::Opt(inner) = at else { return Ok((a, at)) };
                let (b, _) = self.value(r, env, Some(&inner))?;
                let lazy = !(is_simple(&b));
                let code = if lazy { format!("{}.unwrap_or_else(|| {b})", paren(&a)) } else { format!("{}.unwrap_or({b})", paren(&a)) };
                let _ = want;
                Ok((code, *inner))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::FloorDiv | BinOp::Mod => {
                let (a, at) = self.value(l, env, None)?;
                let (b, bt) = self.value(r, env, None)?;
                if matches!(at, Ty::Opt(_)) || matches!(bt, Ty::Opt(_)) {
                    return err(line, "arithmetic on a value that may be missing: give it a default with `??`, or test `.is_missing()` first");
                }
                if at != Ty::Int || bt != Ty::Int {
                    return err(line, "arithmetic in a rule is on integers");
                }
                let m = match op {
                    BinOp::Add => "wrapping_add",
                    BinOp::Sub => "wrapping_sub",
                    BinOp::Mul => "wrapping_mul",
                    BinOp::FloorDiv => "div_euclid",
                    _ => "rem_euclid",
                };
                Ok((format!("{}.{m}({b})", paren(&a)), Ty::Int))
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                let (a, at) = self.borrowable(l, env)?;
                let (b, bt) = self.borrowable(r, env)?;
                if matches!(at, Ty::Opt(_)) || matches!(bt, Ty::Opt(_)) {
                    return err(line, "comparing a value that may be missing: test `.is_missing()` first, or give a default with `??`");
                }
                if at != bt {
                    return err(line, format!("comparing {} with {}", at.rust(), bt.rust()));
                }
                let o = match op {
                    BinOp::Eq => "==",
                    BinOp::Ne => "!=",
                    BinOp::Lt => "<",
                    BinOp::Gt => ">",
                    BinOp::Le => "<=",
                    _ => ">=",
                };
                Ok((format!("{} {o} {}", paren(&a), paren(&b)), Ty::Bool))
            }
            other => err(line, format!("`{other:?}` is not used in rules")),
        }
    }

    fn call(&mut self, name: &str, args: &[Expr], env: &Env, line: usize) -> Result<(String, Ty), EmitError> {
        if matches!(name, "max" | "min") && args.len() == 2 {
            let (a, _) = self.value(&args[0], env, Some(&Ty::Int))?;
            let (b, _) = self.value(&args[1], env, Some(&Ty::Int))?;
            return Ok((format!("{}.{name}({b})", paren(&a)), Ty::Int));
        }
        if name == "abs" && args.len() == 1 {
            let (a, _) = self.value(&args[0], env, Some(&Ty::Int))?;
            return Ok((format!("{}.wrapping_abs()", paren(&a)), Ty::Int));
        }
        let Some((ptys, ret)) = self.fns.get(name).cloned() else {
            return err(line, format!("`{name}` is not one of this rule's functions (a rule calls only its own functions, `max`, `min` and `abs`)"));
        };
        if ptys.len() != args.len() {
            return err(line, format!("`{name}` takes {} arguments", ptys.len()));
        }
        let mut codes = Vec::new();
        for (a, t) in args.iter().zip(&ptys) {
            codes.push(self.value(a, env, Some(t))?.0);
        }
        Ok((format!("{}({})", rust_name(name), codes.join(", ")), ret))
    }

    /// A lambda over the elements of an iterator: `|x| body`, its parameter `refs` deep.
    fn lambda(&mut self, f: &Expr, env: &Env, params: &[(Ty, usize)], want: Option<&Ty>, line: usize) -> Result<(String, Ty), EmitError> {
        let Expr::Lambda { params: names, body, .. } = f else {
            return err(line, "this takes a function: `(x) => …`");
        };
        if names.len() != params.len() {
            return err(line, format!("this function takes {} parameter{}", params.len(), if params.len() == 1 { "" } else { "s" }));
        }
        let mut env = env.clone();
        let mut heads = Vec::new();
        for (n, (t, refs)) in names.iter().zip(params) {
            let r = rust_name(n);
            heads.push(r.clone());
            env = env.with(n, Var { rust: r, ty: t.clone(), refs: *refs });
        }
        let (c, t) = self.value(body, &env, want)?;
        Ok((format!("|{}| {c}", heads.join(", ")), t))
    }

    fn method(&mut self, recv: &Expr, name: &str, args: &[Expr], env: &Env, want: Option<&Ty>, line: usize) -> Result<(String, Ty), EmitError> {
        // `[].to_dict()`: an empty map, of the type the context declares.
        if name == "to_dict" && matches!(recv, Expr::Array(a) if a.is_empty()) {
            return match want {
                Some(Ty::Map(k, v)) => Ok(("BTreeMap::new()".into(), Ty::Map(k.clone(), v.clone()))),
                _ => err(line, "an empty map needs a type here (a struct field's, or a signature's)"),
            };
        }
        let (r, rty) = self.borrowable(recv, env)?;
        let arg = |this: &mut Self, i: usize, t: Option<&Ty>| -> Result<(String, Ty), EmitError> {
            match args.get(i) {
                Some(a) => this.value(a, env, t),
                None => err(line, format!("`{name}` needs an argument")),
            }
        };
        match (&rty, name) {
            (Ty::Opt(_), "is_missing") => Ok((format!("{r}.is_none()"), Ty::Bool)),
            (_, "is_missing") => Ok(("false".into(), Ty::Bool)),
            (Ty::Vec(t), "count" | "length") => {
                let _ = t;
                Ok((format!("({r}.len() as i64)"), Ty::Int))
            }
            (Ty::Vec(_), "is_empty") => Ok((format!("{r}.is_empty()"), Ty::Bool)),
            (Ty::Vec(t), "contains") => {
                let a = self.by_ref(args.first().ok_or_else(|| EmitError { line, message: "`contains` takes a value".into() })?, env, Some(t))?;
                Ok((format!("{r}.contains({a})"), Ty::Bool))
            }
            (Ty::Vec(t), "concat") => {
                let (a, _) = arg(self, 0, Some(&rty))?;
                let _ = t;
                Ok((format!("[{r}.clone(), {a}].concat()"), rty.clone()))
            }
            (Ty::Vec(t), "map") => {
                let f = args.first().ok_or_else(|| EmitError { line, message: "`map` takes a function".into() })?;
                let want_elem = match want {
                    Some(Ty::Vec(e)) => Some((**e).clone()),
                    _ => None,
                };
                let (l, out) = self.lambda(f, env, &[((**t).clone(), 1)], want_elem.as_ref(), line)?;
                Ok((format!("{r}.iter().map({l}).collect::<Vec<_>>()"), Ty::Vec(Box::new(out))))
            }
            (Ty::Vec(t), "filter") => {
                let f = args.first().ok_or_else(|| EmitError { line, message: "`filter` takes a function".into() })?;
                let (l, _) = self.lambda(f, env, &[((**t).clone(), 2)], Some(&Ty::Bool), line)?;
                Ok((format!("{r}.iter().filter({l}).cloned().collect::<Vec<_>>()"), rty.clone()))
            }
            (Ty::Vec(t), "any" | "all") => {
                let f = args.first().ok_or_else(|| EmitError { line, message: format!("`{name}` takes a function") })?;
                let (l, _) = self.lambda(f, env, &[((**t).clone(), 1)], Some(&Ty::Bool), line)?;
                Ok((format!("{r}.iter().{name}({l})"), Ty::Bool))
            }
            (Ty::Vec(t), "reduce") => {
                let (init, ity) = arg(self, 0, want)?;
                let f = args.get(1).ok_or_else(|| EmitError { line, message: "`reduce` takes an initial value and a function".into() })?;
                let (l, _) = self.lambda(f, env, &[(ity.clone(), 0), ((**t).clone(), 1)], Some(&ity), line)?;
                Ok((format!("{r}.iter().fold({init}, {l})"), ity))
            }
            (Ty::Vec(t), "min" | "max") if **t == Ty::Int => Ok((
                format!("{r}.iter().copied().{name}().expect(\"cannot compute `{name}` of an empty array\")"),
                Ty::Int,
            )),
            (Ty::Vec(t), "first") => Ok((format!("{r}.first().cloned()"), Ty::Opt(t.clone()))),
            (Ty::Vec(t), "sort") => Ok((format!("{{ let mut v = {r}.clone(); v.sort(); v }}"), Ty::Vec(t.clone()))),
            (Ty::Vec(t), "to_dict") => match (t.as_ref(), want) {
                (Ty::Tuple(kv), _) if kv.len() == 2 => Ok((
                    format!("{r}.iter().cloned().collect::<BTreeMap<_, _>>()"),
                    Ty::Map(Box::new(kv[0].clone()), Box::new(kv[1].clone())),
                )),
                _ => err(line, "`to_dict` takes a list of (key, value) pairs"),
            },
            (Ty::Map(k, v), "get") => {
                let a = self.by_ref(args.first().ok_or_else(|| EmitError { line, message: "`get` takes a key".into() })?, env, Some(k))?;
                let got = if v.copy() { format!("{r}.get({a}).copied()") } else { format!("{r}.get({a}).cloned()") };
                if args.len() == 2 {
                    let (d, _) = arg(self, 1, Some(v))?;
                    Ok((format!("{got}.unwrap_or({d})"), (**v).clone()))
                } else {
                    Ok((got, Ty::Opt(v.clone())))
                }
            }
            (Ty::Map(k, _), "has" | "contains") => {
                let a = self.by_ref(args.first().ok_or_else(|| EmitError { line, message: "`has` takes a key".into() })?, env, Some(k))?;
                Ok((format!("{r}.contains_key({a})"), Ty::Bool))
            }
            (Ty::Map(k, v), "insert") => {
                let (a, _) = arg(self, 0, Some(k))?;
                let (b, _) = arg(self, 1, Some(v))?;
                Ok((format!("{{\nlet mut m = {r}.clone();\nm.insert({a}, {b});\nm\n}}"), rty.clone()))
            }
            (Ty::Map(k, _), "remove") => {
                let a = self.by_ref(args.first().ok_or_else(|| EmitError { line, message: "`remove` takes a key".into() })?, env, Some(k))?;
                Ok((format!("{{\nlet mut m = {r}.clone();\nm.remove({a});\nm\n}}"), rty.clone()))
            }
            (Ty::Map(k, _), "keys") => Ok((format!("{r}.keys().cloned().collect::<Vec<_>>()"), Ty::Vec(k.clone()))),
            (Ty::Map(_, v), "values") => Ok((format!("{r}.values().cloned().collect::<Vec<_>>()"), Ty::Vec(v.clone()))),
            (Ty::Map(k, v), "items") => Ok((
                format!("{r}.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>()"),
                Ty::Vec(Box::new(Ty::Tuple(vec![(**k).clone(), (**v).clone()]))),
            )),
            (Ty::Map(..), "count" | "length") => Ok((format!("({r}.len() as i64)"), Ty::Int)),
            (Ty::Str, "starts_with" | "ends_with" | "contains") => {
                let (a, _) = arg(self, 0, Some(&Ty::Str))?;
                Ok((format!("{r}.{name}({}.as_str())", paren(&a)), Ty::Bool))
            }
            (t, m) => err(line, format!("`.{m}()` on {} is not something a rule can emit", t.rust())),
        }
    }

    fn if_(&mut self, cond: &Expr, then_b: &Expr, else_b: &Expr, env: &Env, want: Option<&Ty>, line: usize) -> Result<(String, Ty), EmitError> {
        // `if x.is_missing() then a else b` (or `not x.is_missing()`, flipped): `x` is
        // unwrapped where it is present.
        let test = match cond {
            Expr::Method { recv, name, args, .. } if name == "is_missing" && args.is_empty() => Some((recv.as_ref(), false)),
            Expr::Unary { op: UnOp::Not, expr, .. } => match expr.as_ref() {
                Expr::Method { recv, name, args, .. } if name == "is_missing" && args.is_empty() => Some((recv.as_ref(), true)),
                _ => None,
            },
            _ => None,
        };
        if let Some((subject, flipped)) = test
            && let Some(p) = path_of(subject)
            && let Some((code, Ty::Opt(inner), refs)) = self.place(subject, env)
        {
            let (absent, present) = if flipped { (else_b, then_b) } else { (then_b, else_b) };
            if mentions(present, &p) {
                let var = rust_name(p.rsplit('.').next().unwrap_or("v"));
                let place = if refs > 0 { format!("{}{code}", "*".repeat(refs)) } else { code };
                let (ac, at) = self.branch(absent, env, want)?;
                self.narrowed.push((p.clone(), Var { rust: var.clone(), ty: (*inner).clone(), refs: 0 }));
                let pr = self.value(present, env, Some(&at).or(want));
                self.narrowed.pop();
                let (pc, pt) = pr?;
                let place = if inner.copy() { place } else { format!("{place}.clone()") };
                return Ok((format!("match {place} {{\nNone => {ac},\nSome({var}) => {pc},\n}}"), pt));
            }
        }
        let (c, _) = self.value(cond, env, Some(&Ty::Bool))?;
        // An empty list or a `missing` takes its type from the other branch.
        let (tc, tt, ec, et) = match self.branch(then_b, env, want) {
            Ok((tc, tt)) => {
                let (ec, et) = self.value(else_b, env, Some(&tt))?;
                (tc, tt, ec, et)
            }
            Err(first) => {
                let Ok((ec, et)) = self.branch(else_b, env, want) else { return Err(first) };
                let (tc, tt) = self.value(then_b, env, Some(&et))?;
                (tc, tt, ec, et)
            }
        };
        let ty = if tt == et { tt } else { return err(line, format!("the branches give {} and {}", tt.rust(), et.rust())) };
        Ok((format!("if {c} {{\n{}\n}} else {{\n{}\n}}", unblock(&tc), unblock(&ec)), ty))
    }

    fn branch(&mut self, e: &Expr, env: &Env, want: Option<&Ty>) -> Result<(String, Ty), EmitError> {
        self.value(e, env, want)
    }
}

/// A block's statements, for a place that is already a block (a function or branch body).
fn unblock(c: &str) -> &str {
    match c.strip_prefix("{\n").and_then(|x| x.strip_suffix("\n}")) {
        Some(inner) if balanced_braces(inner) => inner,
        _ => c,
    }
}

fn balanced_braces(c: &str) -> bool {
    let mut d = 0i32;
    for ch in c.chars() {
        match ch {
            '{' => d += 1,
            '}' => {
                d -= 1;
                if d < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    d == 0
}

/// `c`, parenthesised unless it is already one postfix expression (a name, a call, a method
/// chain, a literal) — the only kind that is safe before `.method()` or beside an operator.
fn paren(c: &str) -> String {
    if postfix(c) { c.to_string() } else { format!("({c})") }
}

fn postfix(c: &str) -> bool {
    let b = c.as_bytes();
    if b.is_empty() || matches!(b[0], b'!' | b'*' | b'&' | b'-' | b'|') {
        return false;
    }
    let mut depth = 0i32;
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let ch = b[i];
        if in_str {
            if ch == b'\\' {
                i += 2;
                continue;
            }
            if ch == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match ch {
            b'"' => in_str = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            // A turbofish (`::<Vec<_>>`) is part of the path, not a comparison.
            b':' if depth == 0 && b.get(i + 1) == Some(&b':') && b.get(i + 2) == Some(&b'<') => {
                let mut angle = 0i32;
                i += 2;
                while i < b.len() {
                    match b[i] {
                        b'<' => angle += 1,
                        b'>' => {
                            angle -= 1;
                            if angle == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            // `vec![…]` is a macro call, not a negation.
            b'!' if depth == 0 && i > 0 && b[i - 1].is_ascii_alphanumeric() => {}
            _ if depth == 0 && (ch.is_ascii_whitespace() || b"+-*/%<>=!&|?".contains(&ch)) => return false,
            _ => {}
        }
        i += 1;
    }
    depth == 0
}

/// Cheap enough to build even when unused (no call in it): `unwrap_or`, not `unwrap_or_else`.
fn is_simple(c: &str) -> bool {
    !c.contains('(')
}

// --- The boundary: public methods in the contract's types. --------------------------------

/// `x` (a parameter as the API takes it) as the type the rule uses inside.
fn into_inner(name: &str, api: &Api) -> String {
    if let Some(t) = api.saved() {
        return format!("{name}.and_then({t}::load)");
    }
    let t = api.text.replace(' ', "");
    match t.as_str() {
        "u64" | "u32" | "usize" | "i32" | "isize" => format!("{name} as i64"),
        "&str" => format!("{name}.to_string()"),
        "Option<&str>" => format!("{name}.map(str::to_string)"),
        "Option<u64>" | "Option<u32>" | "Option<usize>" => format!("{name}.map(|v| v as i64)"),
        _ if t.starts_with("&[") => format!("{name}.to_vec()"),
        _ if api.inner.copy() => name.to_string(),
        _ => name.to_string(),
    }
}

/// A rule's result, as the API returns it.
fn from_inner(code: &str, api: &Api) -> String {
    if api.saved().is_some() {
        return format!("{code}.save()");
    }
    let t = api.text.replace(' ', "");
    match t.as_str() {
        "u64" | "u32" | "usize" => format!("{code} as {t}"),
        "Option<u64>" | "Option<u32>" | "Option<usize>" => format!("{code}.map(|v| v as {})", &t[7..t.len() - 1]),
        _ => code.to_string(),
    }
}

/// What the effects analysis says about the rule, for the header — and a refusal if it can
/// reach anything.
fn effects_line(stmts: &[Stmt]) -> Result<String, String> {
    for f in crate::effects::closure(stmts) {
        if let Some((e, path)) = f.does.effects.first() {
            return Err(format!("`{}` reaches {e:?} ({}); a rule must not reach files, the network or programs", f.name, path.join(" -> ")));
        }
        if let Some((what, _)) = &f.does.nondeterministic {
            return Err(format!("`{}` is not reproducible: it calls `{what}`; a rule's inputs are its arguments", f.name));
        }
    }
    Ok("no authority (files, network, programs: none); reproducible".into())
}

/// The whole emitted file.
pub fn emit(src: &str, source_name: &str, traces_json: Option<&str>) -> Result<String, EmitError> {
    let c = parse_contract(src)?;
    let toks = crate::lexer::lex(src).map_err(|e| EmitError { line: e.line, message: e.message })?;
    let stmts = crate::parser::parse(toks).map_err(|e| EmitError { line: e.line, message: e.message })?;
    crate::types::check(&stmts).map_err(|e| EmitError { line: e.line, message: format!("the rule does not type-check: {}", e.message) })?;
    let effects = effects_line(&stmts).map_err(|m| EmitError { line: 0, message: m })?;

    let mut tx = Tx { c: &c, fns: BTreeMap::new(), narrowed: Vec::new() };
    // Top-level integer and boolean constants (`WIPE_AT = 10`) are Rust `const`s.
    let mut consts: Vec<(String, String, Ty)> = Vec::new();
    let mut const_env = Env::default();
    for st in &stmts {
        let Stmt::Assign { name, value, line, .. } = st else { continue };
        let (v, ty) = match value {
            Expr::Int(n) => (n.to_string(), Ty::Int),
            Expr::Unary { op: UnOp::Neg, expr, .. } if matches!(expr.as_ref(), Expr::Int(_)) => {
                let Expr::Int(n) = expr.as_ref() else { continue };
                (format!("-{n}"), Ty::Int)
            }
            Expr::Bool(b) => (b.to_string(), Ty::Bool),
            Expr::Lambda { .. } => continue,
            _ => return err(*line, format!("`{name}`: a rule's top-level values are integer or boolean constants")),
        };
        if name.chars().any(|ch| ch.is_lowercase()) {
            return err(*line, format!("`{name}`: a rule's constants are written in capitals, as Rust's are (`{}`)", name.to_uppercase()));
        }
        const_env = const_env.with(name, Var { rust: name.clone(), ty: ty.clone(), refs: 0 });
        consts.push((name.clone(), v, ty));
    }
    let self_ty = c.self_ty.clone();
    let self_named = || Ty::Named(self_ty.clone().unwrap_or_default());
    // Every function's internal types, from its signature.
    for (helix, sig) in &c.sigs {
        let mut ps: Vec<Ty> = Vec::new();
        if sig.recv != Recv::None {
            ps.push(self_named());
        }
        ps.extend(sig.params.iter().map(|(_, a)| a.param_ty()));
        let out = sig.ret.as_ref().map(|r| r.inner.clone()).unwrap_or(Ty::Unit);
        let ret = match sig.recv {
            Recv::Mut if out == Ty::Unit => self_named(),
            Recv::Mut => Ty::Tuple(vec![self_named(), out]),
            _ => out,
        };
        tx.fns.insert(helix.clone(), (ps, ret));
    }

    let mut out = String::new();
    let digest = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(src.as_bytes()))
    };
    let _ = writeln!(out, "// @generated by Helix {} (`helix emit rust`) from {source_name}. Do not edit:", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(out, "// change the Helix rule and emit again.");
    let _ = writeln!(out, "// Source SHA-256: {digest}");
    let _ = writeln!(out, "// helix effects: {effects}");
    for d in &c.doc {
        let _ = writeln!(out, "//! {d}").map(|_| ());
    }
    out = out.replace("//! \n", "//!\n");
    // Filled in at the end, when it is known whether a map is used.
    out.push_str("\n__IMPORTS__\n");

    for (name, v, ty) in &consts {
        let _ = writeln!(out, "const {name}: {} = {v};", ty.rust());
    }
    if !consts.is_empty() {
        out.push('\n');
    }
    for d in &c.decls {
        match d {
            Decl::Struct { name, public, fields } => {
                let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]\n{}struct {name} {{", if *public { "pub " } else { "" });
                for (f, t) in fields {
                    let _ = writeln!(out, "    {f}: {},", t.rust());
                }
                let _ = writeln!(out, "}}\n");
            }
            Decl::Enum { name, public, variants } => {
                let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]\n{}enum {name} {{", if *public { "pub " } else { "" });
                for (v, p) in variants {
                    match p {
                        Some(t) => {
                            let _ = writeln!(out, "    {v}({}),", t.rust());
                        }
                        None => {
                            let _ = writeln!(out, "    {v},");
                        }
                    }
                }
                let _ = writeln!(out, "}}\n");
            }
        }
    }

    // The rule's own functions, one Rust function each, in source order.
    for s in &stmts {
        let Stmt::Func { name, params, body, line, .. } = s else { continue };
        let Some(sig) = c.sigs.get(name) else {
            return err(*line, format!("`{name}` has no `## rust: fn {name}(…)` signature above it"));
        };
        let (ptys, ret) = tx.fns[name].clone();
        if params.len() != ptys.len() {
            return err(*line, format!("`{name}` has {} parameters and its signature {}", params.len(), ptys.len()));
        }
        let mut env = const_env.clone();
        let mut heads = Vec::new();
        for ((p, _), t) in params.iter().zip(&ptys) {
            let r = rust_name(p);
            heads.push(format!("{r}: {}", t.rust()));
            env = env.with(p, Var { rust: r, ty: t.clone(), refs: 0 });
        }
        let (code, _) = tx.value(body, &env, Some(&ret)).map_err(|mut e| {
            if e.line == 0 {
                e.line = *line;
                e.message = format!("in `{name}`: {}", e.message);
            }
            e
        })?;
        let _ = sig;
        let _ = writeln!(out, "fn {}({}) -> {} {{\n{}\n}}\n", rust_name(name), heads.join(", "), ret.rust(), unblock(&code));
    }

    // The contract's methods, converting at the boundary.
    if let Some(st) = &self_ty {
        let _ = writeln!(out, "impl {st} {{");
        for s in &stmts {
            let Stmt::Func { name, .. } = s else { continue };
            let sig = &c.sigs[name];
            if !sig.public {
                continue;
            }
            let recv = match sig.recv {
                Recv::None => String::new(),
                Recv::Ref => "&self, ".into(),
                Recv::Mut => "&mut self, ".into(),
            };
            let params: Vec<String> = sig.params.iter().map(|(n, a)| format!("{n}: {}", a.public_text(true))).collect();
            let ret_text = match &sig.ret {
                Some(r) => format!(" -> {}", r.public_text(false)),
                None => String::new(),
            };
            let mut args: Vec<String> = Vec::new();
            if sig.recv != Recv::None {
                args.push("self.clone()".into());
            }
            args.extend(sig.params.iter().map(|(n, a)| into_inner(n, a)));
            let call = format!("{}({})", rust_name(name), args.join(", "));
            let body = match (sig.recv, &sig.ret) {
                (Recv::Mut, None) => format!("*self = {call};"),
                (Recv::Mut, Some(r)) => format!("let (next, out) = {call};\n*self = next;\n{}", from_inner("out", r)),
                (_, Some(r)) => from_inner(&call, r),
                (_, None) => format!("{call};"),
            };
            let recv_text = recv.trim_end_matches(", ");
            let all = if recv_text.is_empty() { params.join(", ") } else if params.is_empty() { recv_text.to_string() } else { format!("{recv}{}", params.join(", ")) };
            for d in &sig.doc {
                let _ = writeln!(out, "    ///{}{d}", if d.is_empty() { "" } else { " " });
            }
            let _ = writeln!(out, "    pub fn {}({all}){ret_text} {{\n{body}\n}}\n", sig.name);
        }
        let _ = writeln!(out, "}}");
    }

    // The save format, for each struct a signature saves or restores.
    let saved: std::collections::BTreeSet<String> = c
        .sigs
        .values()
        .flat_map(|s| s.params.iter().map(|(_, a)| a).chain(s.ret.iter()))
        .filter_map(|a| a.saved().map(str::to_string))
        .collect();
    for t in &saved {
        out.push_str(&save_format(&c, t)?);
    }
    if !saved.is_empty() {
        out.push_str(SAVE_HELPERS);
    }
    // `new()` with no arguments: clippy (rightly) wants `Default` too.
    if let Some(st) = &self_ty
        && c.sigs.values().any(|s| s.public && s.recv == Recv::None && s.name == "new" && s.params.is_empty())
    {
        let _ = writeln!(out, "\nimpl Default for {st} {{\n    fn default() -> Self {{\n        Self::new()\n    }}\n}}");
    }
    if let Some(json) = traces_json {
        out.push_str(&tests(&c, json)?);
        if !saved.is_empty() {
            out.push_str(&save_tests(&c, &saved)?);
        }
    }
    let imports = if out.contains("BTreeMap") { "use std::collections::BTreeMap;\n" } else { "" };
    Ok(out.replacen("\n__IMPORTS__\n", &format!("\n{imports}"), 1))
}

// --- The save format. ---------------------------------------------------------------------
//
// Text, one field per line, with a version and a checksum:
//
//     helix-save 1 Saved
//     failures 3
//     wait_left 60000
//     check 9f0c…
//
// `load` takes exactly what `save` writes and nothing else: the header and version, every field
// in order, valid values, no extra lines, and a checksum (64-bit FNV-1a) of everything above
// it. A file that is missing, cut short, edited or damaged reads as `None`, and the rule
// decides what `None` means — for a lockout, the longest wait.

const SAVE_HELPERS: &str = r#"
/// The checksum on the last line of a saved state: 64-bit FNV-1a of everything above it.
fn save_checksum(body: &str) -> u64 {
    body.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The value on a saved line `name value`.
fn save_field<'a>(line: Option<&'a str>, name: &str) -> Option<&'a str> {
    line?.strip_prefix(name)?.strip_prefix(' ')
}
"#;

fn save_format(c: &Contract, t: &str) -> Result<String, EmitError> {
    let Some(fields) = c.decls.iter().find_map(|d| match d {
        Decl::Struct { name, fields, .. } if name == t => Some(fields.clone()),
        _ => None,
    }) else {
        return err(0, format!("`Save<{t}>`: `{t}` is not a declared struct"));
    };
    let mut writes = String::new();
    let mut reads = String::new();
    for (f, ty) in &fields {
        if f == "check" {
            return err(0, format!("`{t}`: a saved struct cannot have a field named `check` (the format uses it)"));
        }
        let parse = match ty {
            Ty::Int => "i64",
            Ty::Bool => "bool",
            other => return err(0, format!("`Save<{t}>`: field `{f}` is {}; a saved struct holds integers and booleans", other.rust())),
        };
        let _ = writeln!(writes, "        body.push_str(&format!(\"{f} {{}}\\n\", self.{f}));");
        let _ = writeln!(reads, "        let {f} = save_field(lines.next(), \"{f}\")?.parse::<{parse}>().ok()?;");
    }
    let names: Vec<&str> = fields.iter().map(|(f, _)| f.as_str()).collect();
    Ok(format!(
        "\nimpl {t} {{\n    /// This state as text that survives a restart (`helix emit rust`'s save format, version 1).\n    \
         pub fn save(&self) -> String {{\n        let mut body = String::from(\"helix-save 1 {t}\\n\");\n{writes}        \
         format!(\"{{body}}check {{:016x}}\\n\", save_checksum(&body))\n    }}\n\n    \
         /// What `save` wrote, or `None` if the text is missing a part, edited or damaged.\n    \
         pub fn load(text: &str) -> Option<Self> {{\n        let at = text.rfind(\"check \")?;\n        let (body, tail) = text.split_at(at);\n        \
         if tail != format!(\"check {{:016x}}\\n\", save_checksum(body)) {{\n            return None;\n        }}\n        \
         let mut lines = body.lines();\n        if lines.next()? != \"helix-save 1 {t}\" {{\n            return None;\n        }}\n{reads}        \
         if lines.next().is_some() {{\n            return None;\n        }}\n        Some(Self {{ {} }})\n    }}\n}}\n",
        names.join(", ")
    ))
}

/// Round trips, and every damaged copy refused: each single byte changed, and each cut.
fn save_tests(c: &Contract, saved: &std::collections::BTreeSet<String>) -> Result<String, EmitError> {
    let mut out = String::from("\n#[cfg(test)]\nmod helix_save_format {\n    use super::*;\n");
    for t in saved {
        let Some(fields) = c.decls.iter().find_map(|d| match d {
            Decl::Struct { name, fields, .. } if name == t => Some(fields.clone()),
            _ => None,
        }) else {
            continue;
        };
        let sample: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, (f, ty))| match ty {
                Ty::Bool => format!("{f}: {}", i % 2 == 0),
                _ => format!("{f}: {}", 60_000 * (i as i64 + 1) - 7),
            })
            .collect();
        let lower = t.to_lowercase();
        let _ = writeln!(
            out,
            "\n    #[test]\n    fn {lower}_round_trips_and_damage_is_refused() {{\n        let x = {t} {{ {} }};\n        let text = x.save();\n        \
             assert_eq!({t}::load(&text), Some(x.clone()));\n        assert_eq!({t}::load(\"\"), None);\n        \
             for cut in 0..text.len() {{\n            assert_eq!({t}::load(&text[..cut]), None, \"cut at {{cut}}\");\n        }}\n        \
             for i in 0..text.len() {{\n            for b in *b\"09x \\n-\" {{\n                let mut bytes = text.clone().into_bytes();\n                \
             if bytes[i] == b {{\n                    continue;\n                }}\n                bytes[i] = b;\n                \
             let damaged = String::from_utf8(bytes).unwrap_or_default();\n                assert_eq!({t}::load(&damaged), None, \"byte {{i}} set to {{b}}\");\n            }}\n        }}\n    }}",
            sample.join(", ")
        );
    }
    out.push_str("}\n");
    Ok(out)
}

// --- Checked against the Helix: generated call sequences. ---------------------------------

/// A tiny deterministic generator, so the same rule emits the same tests every time.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, xs: &'a [String]) -> &'a String {
        &xs[(self.next() as usize) % xs.len()]
    }
}

/// An argument for `api`, as JSON (for the Helix run) — from the rule's declared domains.
fn gen_arg(api: &Api, c: &Contract, r: &mut Lcg) -> serde_json::Value {
    let dom = |k: &str, dflt: &[&str]| -> Vec<String> { c.domains.get(k).cloned().unwrap_or_else(|| dflt.iter().map(|s| s.to_string()).collect()) };
    let strings = dom("String", &["a", "b", "c"]);
    let ints = dom("Int", &["0", "1", "2", "5", "10"]);
    let t = api.text.replace(' ', "");
    match t.as_str() {
        "u64" | "u32" | "usize" | "i64" | "i32" => serde_json::json!(r.pick(&ints).parse::<i64>().unwrap_or(0)),
        "bool" => serde_json::json!(r.next().is_multiple_of(2)),
        "&str" | "String" => serde_json::json!(r.pick(&strings)),
        "Option<&str>" | "Option<String>" => {
            if r.next().is_multiple_of(3) { serde_json::Value::Null } else { serde_json::json!(r.pick(&strings)) }
        }
        _ if t.starts_with("&[String]") || t == "Vec<String>" => {
            let n = (r.next() % 4) as usize;
            serde_json::json!((0..n).map(|_| r.pick(&strings).clone()).collect::<Vec<_>>())
        }
        _ => serde_json::Value::Null,
    }
}

/// The constructor the traces start from: `new` if there is one, else the first.
fn constructor(c: &Contract) -> Option<&Sig> {
    let mut ctors = c.sigs.values().filter(|s| s.recv == Recv::None && s.public);
    c.sigs.values().find(|s| s.recv == Recv::None && s.public && s.name == "new").or_else(|| ctors.next())
}

/// The call sequences to run: `[{init: [args], steps: [{call, args}]}]`.
pub fn plan_traces(src: &str) -> Result<String, EmitError> {
    let c = parse_contract(src)?;
    let Some(ctor) = constructor(&c) else {
        return Ok("[]".into());
    };
    let methods: Vec<&Sig> = c.sigs.values().filter(|s| s.public && s.recv != Recv::None).collect();
    let mut r = Lcg(0x5eed_1e11);
    let mut traces = Vec::new();
    for _ in 0..c.traces {
        let init: Vec<serde_json::Value> = ctor.params.iter().map(|(_, a)| gen_arg(a, &c, &mut r)).collect();
        let n = 4 + (r.next() % 9) as usize;
        let steps: Vec<serde_json::Value> = (0..n)
            .filter_map(|_| {
                let m = methods.get((r.next() as usize) % methods.len().max(1))?;
                let args: Vec<serde_json::Value> = m.params.iter().map(|(_, a)| gen_arg(a, &c, &mut r)).collect();
                Some(serde_json::json!({"call": m.name, "args": args}))
            })
            .collect();
        traces.push(serde_json::json!({"init": init, "steps": steps}));
    }
    Ok(serde_json::to_string(&traces).unwrap_or_else(|_| "[]".into()))
}

/// A Helix program that runs every trace through the rule and prints the outputs as JSON.
pub fn driver(src: &str, traces: &str) -> Result<String, EmitError> {
    let c = parse_contract(src)?;
    let ctor = c.sigs.iter().find(|(_, s)| constructor(&c).is_some_and(|k| k.name == s.name)).map(|(h, _)| h.clone()).unwrap_or_else(|| "new".into());
    let mut arms = Vec::new();
    for (helix, s) in c.sigs.iter().filter(|(_, s)| s.public && s.recv != Recv::None) {
        let args: Vec<String> = (0..s.params.len()).map(|i| format!("step.args[{i}]")).collect();
        let call = format!("{helix}({})", std::iter::once("s".to_string()).chain(args).collect::<Vec<_>>().join(", "));
        let body = match (s.recv, &s.ret) {
            (Recv::Mut, Some(_)) => format!("do {{\n    r = {call}\n    {{s: r[0], out: r[1]}}\n  }}"),
            (Recv::Mut, None) => format!("{{s: {call}, out: missing}}"),
            _ => format!("{{s: s, out: {call}}}"),
        };
        arms.push(format!("  \"{}\" => {body},", s.name));
    }
    let init_args: Vec<String> = (0..c.sigs.get(&ctor).map_or(0, |s| s.params.len())).map(|i| format!("t.init[{i}]")).collect();
    Ok(format!(
        "{src}\n\n# --- `helix emit rust`'s check: the generated traces, through the rule. ---\n\
         RULE_TRACES = \"\"\"{traces}\"\"\".parse_json()\n\
         fn helix_step(s, step) = match step.call {{\n{}\n  _ => raise(\"unknown call {{step.call}}\"),\n}}\n\
         fn helix_trace(t) = t.steps.scan({{s: {ctor}({}), out: missing}}, (acc, step) => helix_step(acc.s, step)).map((x) => x.out)\n\
         print(RULE_TRACES.map(helix_trace).to_json())\n",
        arms.join("\n"),
        init_args.join(", ")
    ))
}

/// A Rust literal of `api`'s type for the JSON value `v`.
fn lit(v: &serde_json::Value, api_inner: &Ty, api_text: Option<&str>, c: &Contract) -> Result<String, EmitError> {
    use serde_json::Value as J;
    let t = api_text.map(|s| s.replace(' ', ""));
    Ok(match (api_inner, v) {
        (Ty::Opt(_), J::Null) => "None".into(),
        (Ty::Opt(inner), x) => {
            let inner_text = t.as_deref().and_then(|s| s.strip_prefix("Option<")).and_then(|s| s.strip_suffix('>'));
            format!("Some({})", lit(x, inner, inner_text, c)?)
        }
        (Ty::Int, J::Number(n)) => n.to_string(),
        (Ty::Bool, J::Bool(b)) => b.to_string(),
        (Ty::Str, J::String(s)) if t.as_deref() == Some("&str") => format!("{s:?}"),
        (Ty::Str, J::String(s)) => format!("{s:?}.to_string()"),
        (Ty::Vec(e), J::Array(xs)) => {
            let items: Vec<String> = xs.iter().map(|x| lit(x, e, None, c)).collect::<Result<_, _>>()?;
            if t.as_deref().is_some_and(|s| s.starts_with('&')) {
                format!("&[{}]", items.join(", "))
            } else if items.is_empty() {
                "vec![]".into()
            } else {
                format!("vec![{}]", items.join(", "))
            }
        }
        (Ty::Named(n), J::Object(o)) if c.decls.iter().any(|d| matches!(d, Decl::Struct { name, .. } if name == n)) => {
            let Some(Decl::Struct { fields, .. }) = c.decls.iter().find(|d| matches!(d, Decl::Struct { name, .. } if name == n)) else {
                return err(0, format!("`{n}` is not a declared struct"));
            };
            let mut parts = Vec::new();
            for (f, ty) in fields {
                parts.push(format!("{f}: {}", lit(o.get(f).unwrap_or(&J::Null), ty, None, c)?));
            }
            format!("{n} {{ {} }}", parts.join(", "))
        }
        (Ty::Named(n), J::Object(o)) if o.len() == 1 => {
            let (variant, payload) = o.iter().next().map(|(k, v)| (k.clone(), v.clone())).unwrap_or_default();
            let Some(Decl::Enum { variants, .. }) = c.decls.iter().find(|d| matches!(d, Decl::Enum { name, .. } if name == n)) else {
                return err(0, format!("expected a `{n}` in a trace's output"));
            };
            match variants.iter().find(|(v, _)| *v == variant) {
                Some((_, Some(pt))) => format!("{n}::{variant}({})", lit(&payload, pt, None, c)?),
                Some((_, None)) => format!("{n}::{variant}"),
                None => return err(0, format!("`{variant}` is not a variant of `{n}`")),
            }
        }
        (t, v) => return err(0, format!("cannot write {v} as {}", t.rust())),
    })
}

fn tests(c: &Contract, run: &str) -> Result<String, EmitError> {
    let parsed: serde_json::Value = serde_json::from_str(run).map_err(|e| EmitError { line: 0, message: format!("the Helix run did not print JSON: {e}") })?;
    let traces = parsed.get("traces").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let outputs = parsed.get("outputs").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let Some(st) = &c.self_ty else { return Ok(String::new()) };
    let Some(ctor) = constructor(c) else { return Ok(String::new()) };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n/// Generated call sequences, run through the Helix rule when this file was emitted: the Rust\n/// must give the same outputs.\n#[cfg(test)]\nmod helix_replays {{\n    use super::*;\n"
    );
    for (i, (t, outs)) in traces.iter().zip(&outputs).enumerate() {
        let init = t.get("init").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let args: Vec<String> = ctor.params.iter().zip(&init).map(|((_, a), v)| lit(v, &a.inner, Some(&a.text), c)).collect::<Result<_, _>>()?;
        let steps = t.get("steps").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mutates = steps.iter().any(|st| {
            let name = st.get("call").and_then(|v| v.as_str()).unwrap_or("");
            c.sigs.values().any(|s| s.name == name && s.recv == Recv::Mut)
        });
        let mut body = format!("let {}p = {st}::{}({});\n", if mutates { "mut " } else { "" }, ctor.name, args.join(", "));
        let outs = outs.as_array().cloned().unwrap_or_default();
        for (step, o) in steps.iter().zip(&outs) {
            let name = step.get("call").and_then(|v| v.as_str()).unwrap_or("");
            let Some(sig) = c.sigs.values().find(|s| s.name == name) else { continue };
            let sargs = step.get("args").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let a: Vec<String> = sig.params.iter().zip(&sargs).map(|((_, api), v)| lit(v, &api.inner, Some(&api.text), c)).collect::<Result<_, _>>()?;
            let call = format!("p.{name}({})", a.join(", "));
            match &sig.ret {
                Some(r) if r.saved().is_some() => {
                    let t = r.saved().unwrap_or_default();
                    let want = lit(o, &r.inner, None, c)?;
                    let _ = writeln!(body, "assert_eq!({t}::load(&{call}), Some({want}));");
                }
                Some(r) if r.inner == Ty::Bool => {
                    let bang = if o.as_bool() == Some(true) { "" } else { "!" };
                    let _ = writeln!(body, "assert!({bang}{call});");
                }
                Some(r) => {
                    let want = lit(o, &r.inner, Some(&r.text), c)?;
                    let _ = writeln!(body, "assert_eq!({call}, {want});");
                }
                None => {
                    let _ = writeln!(body, "{call};");
                }
            }
        }
        let _ = writeln!(out, "    #[test]\n    fn trace_{i:03}() {{\n{body}}}\n");
    }
    out.push_str("}\n");
    Ok(out)
}

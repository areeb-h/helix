//! `fn main` as the command line (ADR 0037 D1).
//!
//! Before this, `helix run tool.helix --threads 8` ran the program and **discarded the
//! arguments in silence** — not rejected, not warned about, just gone. That is the worst
//! of the three possible behaviours, because the command looks like it worked. A bundled
//! `helix build` artifact did the same, so you could ship a binary that could not take a
//! flag.
//!
//! **The binding rule is not invented here.** It is the rule Helix already uses at a call
//! site, which was measured on the v0.6.0 binary before this module was written:
//! `go(10, 3)`, `go(a: 10, b: 3)` and `go(b: 3, a: 10)` all answer the same, and a
//! trailing default may be omitted. So `tool 10 3`, `tool --a 10 --b 3` and
//! `tool --b 3 --a 10` do too. A reader who knows how to call a Helix function already
//! knows how to invoke a Helix tool.
//!
//! **It is a desugar, not runtime plumbing.** Argv is converted to literal expressions and
//! a `main(…)` call is appended to the program, so the type checker validates the call
//! like any other, and all three engines run identical code — no new evaluator path to
//! keep in agreement, which is the failure this project keeps paying for elsewhere.

use crate::ast::{Expr, Stmt, TypeAnn};
use crate::error::HelixError;

/// The `fn main` a program declares, in the form the binder needs.
pub struct MainSig<'a> {
    pub params: &'a [(String, Option<TypeAnn>)],
    pub defaults: &'a [Option<Expr>],
    pub line: usize,
    pub col: usize,
}

/// The `fn main` a program declares, if it declares one.
pub fn find(stmts: &[Stmt]) -> Option<MainSig<'_>> {
    stmts.iter().find_map(|s| match s {
        Stmt::Func { name, params, defaults, line, col, .. } if name == "main" => {
            Some(MainSig { params, defaults, line: *line, col: *col })
        }
        _ => None,
    })
}

/// Can this parameter be built from a command-line string at all?
///
/// An `Array`/`Tensor`/`DataFrame`/`Dna` parameter cannot, and saying so **at check time**
/// is the point (ADR 0037 D6): the alternative is a tool that installs, runs, and fails on
/// its first invocation.
fn buildable(ty: &Option<TypeAnn>) -> bool {
    !matches!(
        ty,
        Some(TypeAnn::Array) | Some(TypeAnn::Tensor) | Some(TypeAnn::DataFrame) | Some(TypeAnn::Dna)
    )
}

/// The parameter that cannot come from argv, if any — for `helix check`.
pub fn unbindable_param<'a>(sig: &'a MainSig<'a>) -> Option<&'a str> {
    sig.params.iter().find(|(_, ty)| !buildable(ty)).map(|(n, _)| n.as_str())
}

/// Convert one argv string to the literal the parameter's type asks for.
///
/// The error names the option AND the value it could not take, because "invalid integer"
/// on a command line with six options is a puzzle rather than a diagnostic.
fn convert(name: &str, ty: &Option<TypeAnn>, raw: &str, line: usize, col: usize) -> Result<Expr, HelixError> {
    // `want` already carries its own article ("an Int", "a Float"), so the template must
    // not add one: "pass a an Int value" was the first thing this printed.
    let bad = |want: &str, short: &str| {
        HelixError::new(format!("`--{name}` expects {want}, but got `{raw}`"), line, col)
            .hint(format!("pass {want}, e.g. `--{name} <{short}>`."))
    };
    Ok(match ty {
        Some(TypeAnn::Int) => Expr::Int(raw.parse::<i64>().map_err(|_| bad("an Int", "int"))?),
        Some(TypeAnn::Float) => Expr::Float(raw.parse::<f64>().map_err(|_| bad("a Float", "float"))?),
        // `Num` takes either, preferring Int so `--n 3` stays an Int as it would in source.
        Some(TypeAnn::Num) => match raw.parse::<i64>() {
            Ok(i) => Expr::Int(i),
            Err(_) => Expr::Float(raw.parse::<f64>().map_err(|_| bad("a number", "number"))?),
        },
        Some(TypeAnn::Bool) => match raw {
            "true" => Expr::Bool(true),
            "false" => Expr::Bool(false),
            _ => return Err(bad("`true` or `false`", "true|false")),
        },
        // A String parameter, and an UNANNOTATED one: argv is strings, so that is the
        // honest default. A wrong assumption surfaces inside `main` as an ordinary type
        // error rather than being guessed at here.
        _ => Expr::Str(raw.to_string()),
    })
}

/// Bind `argv` to `main`'s parameters, producing the call's arguments.
///
/// Every parameter is passed explicitly — an omitted one gets its declared default
/// expression — so the call does not depend on the parser's trailing-default fill and
/// reads the same whether the program is one file or ten.
pub fn bind(sig: &MainSig<'_>, argv: &[String]) -> Result<Vec<Expr>, HelixError> {
    let (line, col) = (sig.line, sig.col);
    let mut given: Vec<Option<Expr>> = vec![None; sig.params.len()];
    let index_of = |n: &str| sig.params.iter().position(|(p, _)| p == n);

    let mut positional = 0usize;
    let mut i = 0usize;
    while i < argv.len() {
        let a = &argv[i];
        if let Some(name) = a.strip_prefix("--") {
            // `--name=value` and `--name value` are the same request.
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (name, None),
            };
            let Some(idx) = index_of(name) else {
                return Err(HelixError::new(
                    format!("`main` has no parameter `{name}`"),
                    line,
                    col,
                )
                .hint(format!(
                    "it takes: {}. Run with `--help` to see them.",
                    sig.params.iter().map(|(p, _)| format!("--{p}")).collect::<Vec<_>>().join(", ")
                )));
            };
            let (pname, ty) = &sig.params[idx];
            // A Bool defaulting to `false` may be given bare: `--verbose` means
            // `--verbose true`. It is a shorthand for a value, not a separate concept,
            // which is why nothing else in this function knows about flags.
            let raw = match inline {
                Some(v) => v,
                None if matches!(ty, Some(TypeAnn::Bool)) => "true".to_string(),
                None => {
                    let Some(v) = argv.get(i + 1) else {
                        return Err(HelixError::new(
                            format!("`--{pname}` needs a value"),
                            line,
                            col,
                        ));
                    };
                    i += 1;
                    v.clone()
                }
            };
            given[idx] = Some(convert(pname, ty, &raw, line, col)?);
        } else {
            // Positional: fills the next parameter not already given by name.
            while positional < given.len() && given[positional].is_some() {
                positional += 1;
            }
            if positional >= sig.params.len() {
                return Err(HelixError::new(
                    format!("too many arguments: `main` takes {}", sig.params.len()),
                    line,
                    col,
                )
                .hint("run with `--help` to see the parameters.".to_string()));
            }
            let (pname, ty) = &sig.params[positional];
            given[positional] = Some(convert(pname, ty, a, line, col)?);
            positional += 1;
        }
        i += 1;
    }

    // Fill the gaps from the declared defaults, and refuse a required parameter that was
    // never supplied — BY NAME, before anything runs.
    let mut out = Vec::with_capacity(sig.params.len());
    for (idx, (pname, _)) in sig.params.iter().enumerate() {
        match given[idx].take() {
            Some(e) => out.push(e),
            None => match sig.defaults.get(idx).and_then(|d| d.clone()) {
                Some(d) => out.push(d),
                None => {
                    return Err(HelixError::new(
                        format!("`main` needs `{pname}`, and it was not given"),
                        line,
                        col,
                    )
                    .hint(format!(
                        "pass it positionally or as `--{pname} <value>`; `--help` lists them all."
                    )));
                }
            },
        }
    }
    Ok(out)
}

/// The `main(…)` call to append to the program.
pub fn call(args: Vec<Expr>, line: usize, col: usize) -> Stmt {
    Stmt::Expr(Expr::Call { name: "main".to_string(), args, line, col })
}

/// `--help`, built from the declaration and the `##` doc comment above `fn main`.
///
/// Answered WITHOUT running the program, which is what makes it safe for a script whose
/// top level has effects — the common shape, since a script's top level is its program.
pub fn help(sig: &MainSig<'_>, src: &str, tool: &str) -> String {
    let mut out = String::new();
    let doc = doc_above_main(src);
    if !doc.is_empty() {
        out.push_str(&doc);
        out.push('\n');
    }
    // Required parameters are always a prefix — the parser refuses a non-default after a
    // default — so usage can be written left to right without sorting anything.
    let mut usage = format!("usage: {tool}");
    for (idx, (name, ty)) in sig.params.iter().enumerate() {
        let optional = sig.defaults.get(idx).is_some_and(|d| d.is_some());
        // A Bool with a default is written bare on a real command line, so write it bare
        // here: `[--verbose <verbose>]` describes a form nobody types.
        let flag = optional && matches!(ty, Some(TypeAnn::Bool));
        usage.push_str(&if flag {
            format!(" [--{name}]")
        } else if optional {
            format!(" [--{name} <{name}>]")
        } else {
            format!(" <{name}>")
        });
    }
    out.push_str(&usage);
    out.push_str("\n\n");
    if sig.params.is_empty() {
        out.push_str("takes no arguments\n");
        return out;
    }
    for (idx, (name, ty)) in sig.params.iter().enumerate() {
        let tyname = match ty {
            Some(t) => format!("{t:?}"),
            None => "String".to_string(),
        };
        let d = sig.defaults.get(idx).and_then(|d| d.as_ref());
        let tail = match d {
            Some(e) => format!("  (default: {})", literal_text(e)),
            None => "  (required)".to_string(),
        };
        out.push_str(&format!("  --{name:<14} {tyname}{tail}\n"));
    }
    out
}

/// A default's source text, for `--help`. Defaults are literals (ast.rs), so this covers
/// them; anything else prints as the type name rather than a lie.
fn literal_text(e: &Expr) -> String {
    match e {
        Expr::Int(i) => i.to_string(),
        Expr::Float(f) => crate::value::fmt_float(*f),
        Expr::Str(s) => format!("\"{s}\""),
        Expr::Bool(b) => b.to_string(),
        Expr::Missing => "missing".to_string(),
        _ => "…".to_string(),
    }
}

/// The `##` block immediately above `fn main`, with its markers stripped.
///
/// Textual, like the doc-example extractor (`doctest`), because doc comments are trivia
/// and never reach the AST. Only a block that ENDS on the line before `fn main` counts,
/// so an unrelated comment earlier in the file cannot become a tool's help text.
fn doc_above_main(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let Some(at) = lines.iter().position(|l| l.trim_start().starts_with("fn main")) else {
        return String::new();
    };
    let mut start = at;
    while start > 0 && lines[start - 1].trim_start().starts_with("##") {
        start -= 1;
    }
    lines[start..at]
        .iter()
        .map(|l| l.trim_start().trim_start_matches('#').trim_start().to_string())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
        + if start < at { "\n" } else { "" }
}

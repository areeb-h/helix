//! Module loader. `import a.b.c [as alias]` pulls in the file `a/b/c.helix`
//! (relative to the importing module) and exposes it as `alias` (default: the
//! last path segment); this resolves the whole import graph (deduping shared
//! modules by canonical path, rejecting cycles),
//! then rewrites every module into ONE flat statement list that the existing
//! type-check → compile → run pipeline consumes unchanged.
//!
//! Namespacing is done by rewriting the AST, not by teaching the compiler about
//! modules: each module gets a unique prefix `m<N>`, its top-level functions and
//! globals are renamed `m<N>$name`, references to them are renamed to match, and a
//! qualified access `dep.member` becomes a direct reference to `dep`'s mangled
//! name. (`$` never appears in user source, so the names can't collide.) Modules
//! load in dependency order, so the entry module's top level runs last — matching
//! Helix's define-before-use semantics across files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::{Expr, InterpPart, Stmt};
use crate::error::HelixError;

/// The result of loading an entry file and its import graph.
pub struct Loaded {
    pub stmts: Vec<Stmt>,
    /// True if more than one file was loaded (so names were namespaced and error
    /// messages need the internal `m<N>$` prefixes stripped before display).
    pub multi_module: bool,
}

/// Load `entry` and everything it transitively imports, returning the combined
/// statement list. A single-file program is returned unchanged (no namespacing).
/// On a lex/parse/resolve error the message is already rendered (with the
/// *correct* module's filename and caret).
pub fn load(entry: &Path) -> Result<Loaded, String> {
    let mut loader = Loader::default();
    loader.load_file(entry)?;
    // Single file, no imports: hand back the unmodified AST so nothing is mangled
    // and error messages stay pristine — the overwhelmingly common case.
    if loader.modules.len() == 1 {
        let stmts = loader.modules.into_iter().next().unwrap().stmts;
        return Ok(Loaded { stmts, multi_module: false });
    }
    // Modules are in post-order (each dependency before the module that imports it,
    // entry last), so concatenating their rewrites preserves define-before-use.
    let mut out = Vec::new();
    for idx in 0..loader.modules.len() {
        out.extend(rewrite_module(&loader.modules, idx));
    }
    Ok(Loaded { stmts: out, multi_module: true })
}

struct Module {
    stmts: Vec<Stmt>,
    /// Each import's alias mapped to the loaded module's index.
    imports: Vec<(String, usize)>,
}

#[derive(Default)]
struct Loader {
    modules: Vec<Module>,
    by_path: HashMap<PathBuf, usize>,
    /// Canonical paths currently being loaded — for cycle detection.
    in_progress: Vec<PathBuf>,
}

impl Loader {
    fn load_file(&mut self, path: &Path) -> Result<usize, String> {
        let canon = path
            .canonicalize()
            .map_err(|e| format!("error: cannot read `{}`: {}\n", path.display(), e))?;
        if let Some(&i) = self.by_path.get(&canon) {
            return Ok(i); // already loaded (shared dependency)
        }
        if self.in_progress.contains(&canon) {
            return Err(format!(
                "error: import cycle detected involving `{}`\n",
                canon.display()
            ));
        }
        self.in_progress.push(canon.clone());

        let src = std::fs::read_to_string(&canon)
            .map_err(|e| format!("error: cannot read `{}`: {}\n", canon.display(), e))?;
        let fname = canon.to_string_lossy().into_owned();
        let toks = crate::lexer::lex(&src).map_err(|e| e.render(&src, &fname))?;
        let stmts = crate::parser::parse(toks).map_err(|e| e.render(&src, &fname))?;

        let dir = canon.parent().unwrap_or_else(|| Path::new("."));
        let mut imports = Vec::new();
        for s in &stmts {
            if let Stmt::Import { segments, alias, line, col } = s {
                // `import a.b.c` → `<dir>/a/b/c.helix` (relative to this module).
                let mut dep_path = dir.to_path_buf();
                for seg in &segments[..segments.len() - 1] {
                    dep_path.push(seg);
                }
                dep_path.push(format!("{}.helix", segments.last().unwrap()));
                if !dep_path.is_file() {
                    let shown = segments.join(".");
                    return Err(HelixError::new(format!("cannot find module `{shown}`"), *line, *col)
                        .hint(format!("expected a file `{}`.", dep_path.display()))
                        .render(&src, &fname));
                }
                let dep_idx = self.load_file(&dep_path)?;
                // Key the import by the alias — that's the name user code reaches it
                // through (`alias.member`).
                imports.push((alias.clone(), dep_idx));
            }
        }

        self.in_progress.pop();
        let idx = self.modules.len();
        self.modules.push(Module { stmts, imports });
        self.by_path.insert(canon, idx);
        Ok(idx)
    }
}

/// The rename context for one module.
struct Ctx {
    /// This module's own prefix, e.g. `m2`.
    prefix: String,
    /// Top-level names defined in this module (functions + globals) — the things
    /// that get mangled when referenced.
    top_level: HashSet<String>,
    /// Each import alias → that dependency's prefix.
    imports: HashMap<String, String>,
}

fn rewrite_module(modules: &[Module], idx: usize) -> Vec<Stmt> {
    let m = &modules[idx];
    let ctx = Ctx {
        prefix: format!("m{idx}"),
        top_level: top_level_names(&m.stmts),
        imports: m
            .imports
            .iter()
            .map(|(n, dep)| (n.clone(), format!("m{dep}")))
            .collect(),
    };
    let mut out = Vec::with_capacity(m.stmts.len());
    for s in &m.stmts {
        if matches!(s, Stmt::Import { .. }) {
            continue; // imports are resolved away
        }
        let mut s = s.clone();
        rewrite_stmt(&mut s, &ctx);
        out.push(s);
    }
    out
}

/// Top-level function and global names — collected before any renaming.
fn top_level_names(stmts: &[Stmt]) -> HashSet<String> {
    let mut names = HashSet::new();
    for s in stmts {
        match s {
            Stmt::Func { name, .. } | Stmt::Assign { name, .. } => {
                names.insert(name.clone());
            }
            Stmt::Destructure { names: ns, .. } => {
                for n in ns {
                    names.insert(n.clone());
                }
            }
            _ => {}
        }
    }
    names
}

fn mangle(prefix: &str, name: &str) -> String {
    format!("{prefix}${name}")
}

fn rewrite_stmt(s: &mut Stmt, ctx: &Ctx) {
    match s {
        Stmt::Func { name, params, body, .. } => {
            *name = mangle(&ctx.prefix, name);
            let bound: HashSet<String> = params.iter().map(|(p, _)| p.clone()).collect();
            rw(body, ctx, &bound);
        }
        Stmt::Assign { name, value, .. } => {
            *name = mangle(&ctx.prefix, name);
            rw(value, ctx, &HashSet::new());
        }
        Stmt::Destructure { names, value, .. } => {
            for n in names.iter_mut() {
                *n = mangle(&ctx.prefix, n);
            }
            rw(value, ctx, &HashSet::new());
        }
        Stmt::Expr(e) => rw(e, ctx, &HashSet::new()),
        Stmt::Import { .. } => {}
    }
}

/// Rewrite an expression in place. `bound` is the set of local names in scope
/// (parameters, `let` bindings) — those are never mangled.
fn rw(e: &mut Expr, ctx: &Ctx, bound: &HashSet<String>) {
    // `dep.member(...)` / `dep.member` where `dep` is an imported module → a direct
    // reference to the dependency's mangled name. Handled before generic recursion
    // because they replace the whole node.
    if let Expr::Method { recv, name, args, line, col } = e {
        if let Some(dep) = module_of(recv, ctx, bound) {
            for a in args.iter_mut() {
                rw(a, ctx, bound);
            }
            *e = Expr::Call {
                name: mangle(&dep, name),
                args: std::mem::take(args),
                line: *line,
                col: *col,
            };
            return;
        }
    }
    if let Expr::Field { recv, name, line, col } = e {
        if let Some(dep) = module_of(recv, ctx, bound) {
            *e = Expr::Ident { name: mangle(&dep, name), line: *line, col: *col };
            return;
        }
    }

    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => {}
        Expr::Ident { name, .. } => {
            if !bound.contains(name) && ctx.top_level.contains(name) {
                *name = mangle(&ctx.prefix, name);
            }
        }
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(e) = p {
                    rw(e, ctx, bound);
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                rw(x, ctx, bound);
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                rw(v, ctx, bound);
            }
        }
        Expr::Field { recv, .. } => rw(recv, ctx, bound),
        Expr::Unary { expr, .. } => rw(expr, ctx, bound),
        Expr::Binary { left, right, .. } => {
            rw(left, ctx, bound);
            rw(right, ctx, bound);
        }
        Expr::Call { name, args, .. } => {
            for a in args.iter_mut() {
                rw(a, ctx, bound);
            }
            if !bound.contains(name)
                && !crate::interp::BUILTIN_FNS.contains(&name.as_str())
                && ctx.top_level.contains(name)
            {
                *name = mangle(&ctx.prefix, name);
            }
        }
        Expr::Method { recv, args, .. } => {
            rw(recv, ctx, bound);
            for a in args.iter_mut() {
                rw(a, ctx, bound);
            }
        }
        Expr::Index { recv, index, .. } => {
            rw(recv, ctx, bound);
            rw(index, ctx, bound);
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            rw(recv, ctx, bound);
            for o in [start, stop, step] {
                if let Some(x) = o {
                    rw(x, ctx, bound);
                }
            }
        }
        Expr::Lambda { params, body } => {
            let mut b = bound.clone();
            b.extend(params.iter().cloned());
            rw(body, ctx, &b);
        }
        Expr::Let { bindings, body } => {
            let mut b = bound.clone();
            for (n, v) in bindings.iter_mut() {
                rw(v, ctx, &b);
                b.insert(n.clone());
            }
            rw(body, ctx, &b);
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            rw(cond, ctx, bound);
            rw(then_branch, ctx, bound);
            rw(else_branch, ctx, bound);
        }
    }
}

/// If `recv` is a bare identifier naming an imported module (not shadowed by a
/// local), return that module's prefix.
fn module_of(recv: &Expr, ctx: &Ctx, bound: &HashSet<String>) -> Option<String> {
    match recv {
        Expr::Ident { name, .. } if !bound.contains(name) => ctx.imports.get(name).cloned(),
        _ => None,
    }
}

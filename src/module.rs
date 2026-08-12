//! Module loader. `import a.b.c [as alias]` pulls in the file `a/b/c.helix` —
//! found beside the importing file, else under the **project root** (the `helix.toml`
//! directory, or a loose script's own directory), else on the stdlib / `HELIX_PATH`
//! search path — and exposes it as `alias` (default: the last path segment). This
//! resolves the whole import graph (deduping shared modules by canonical path,
//! rejecting cycles),
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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::{Expr, InterpPart, Stmt};
use crate::error::HelixError;

/// One source file's place in the flattened program: the global line it starts at
/// (1-based), and its source text and filename. Errors are rendered against the file
/// whose line range contains the error, with the line mapped back to a local number.
pub struct Span {
    pub start_line: usize,
    pub source: String,
    pub filename: String,
}

/// The result of loading an entry file and its import graph.
pub struct Loaded {
    pub stmts: Vec<Stmt>,
    /// Source spans in ascending global-line order, for error attribution.
    pub spans: Vec<Span>,
    /// True if more than one file was loaded (so names were namespaced and error
    /// messages need the internal `m<N>$` prefixes stripped before display).
    pub multi_module: bool,
}

/// Resolve a (global) error line to the file it belongs to and the line within that
/// file, given the spans in ascending start order. For a single-file program (one
/// span starting at line 1) this is the identity.
pub fn locate(spans: &[Span], line: usize) -> (&str, &str, usize) {
    let span = spans
        .iter()
        .rev()
        .find(|s| s.start_line <= line)
        .unwrap_or(&spans[0]);
    // Saturate: a position-free error can carry line 0 (e.g. a format-spec failure
    // stamped before a source line is known). It matches no span, falls back to
    // `spans[0]` (start_line 1), and a plain `line - start_line` would UNDERFLOW —
    // a host panic under overflow checks, a garbage location in release. Report it at
    // the first line rather than aborting.
    let local = line.saturating_sub(span.start_line).saturating_add(1).max(1);
    (&span.source, &span.filename, local)
}

/// `(start_line, filename)` for every loaded file, ascending — the minimum needed to
/// answer "which file is this line in?" at RUNTIME, which is what `source_path` resolves
/// against. Published as a process-global by the runner because a builtin is dispatched by
/// name and receives only its call position; it cannot otherwise know which module it was
/// written in. Deliberately not the whole `Span` (the sources are large and error
/// rendering, which does need them, has them already).
/// A `RwLock` rather than a `OnceLock` because a process can run more than one program:
/// `helix test` runs every test file in turn, and keeping the first one's map would make
/// `source_path` in the second resolve against the wrong file. Replaced per program, and
/// read from whichever thread a builtin happens to run on.
static FILE_LINES: std::sync::RwLock<Vec<(usize, String)>> =
    std::sync::RwLock::new(Vec::new());

/// Publish the running program's line→file map. Called once per program, before it runs.
pub fn set_file_lines(files: Vec<(usize, String)>) {
    if let Ok(mut w) = FILE_LINES.write() {
        *w = files;
    }
}

/// The absolute path of the file containing global `line`, or `None` when no program is
/// running with a source on disk (the REPL, a unit test).
pub fn file_of_line(line: usize) -> Option<String> {
    let files = FILE_LINES.read().ok()?;
    files
        .iter()
        .rev()
        .find(|(start, _)| *start <= line)
        .or_else(|| files.first())
        .map(|(_, name)| name.clone())
}

/// The directories searched for a non-local import (`import std.stats`), in priority
/// order after the importing file's own directory: every `HELIX_PATH` entry, then the
/// install-relative standard-library locations beside the executable. A stdlib module
/// `std/stats.helix` is found because some root *contains* a `std/` directory.
fn search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(p) = std::env::var_os("HELIX_PATH") {
        roots.extend(std::env::split_paths(&p));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        roots.push(dir.to_path_buf()); // <exe_dir>/std/...
        roots.push(dir.join("../lib/helix")); // <prefix>/lib/helix/std/... (FHS install)
    }
    roots
}

/// The project context for `entry`: its package dependencies, and the **project root**
/// that in-project imports are anchored at. Walking up from the entry file, the root is
/// the nearest directory containing a `helix.toml`; for a loose script with no manifest
/// it is the entry file's own directory. Anchoring every module at one root gives each a
/// single, stable import path (`import lib.geometry` means `<root>/lib/geometry.helix`
/// no matter which file imports it), so a file in a subdirectory can reach a module in
/// another — which a purely relative-to-the-importer scheme cannot express.
fn project_context(entry: &Path) -> Result<(BTreeMap<String, PathBuf>, PathBuf), String> {
    let canon = entry
        .canonicalize()
        .map_err(|e| format!("error: cannot read `{}`: {}\n", entry.display(), e))?;
    let entry_dir = canon.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let mut dir = Some(entry_dir.as_path());
    while let Some(d) = dir {
        if d.join("helix.toml").is_file() {
            // Resolve, and (if locked) verify the sources still match `helix.lock`.
            let dirs = crate::pkg::resolve_for_run(d).map_err(|e| {
                let mut s = format!("error: {}\n", e.message);
                if let Some(h) = &e.hint {
                    s.push_str(&format!("  {h}\n"));
                }
                s
            })?;
            return Ok((dirs, d.to_path_buf()));
        }
        dir = d.parent();
    }
    // No manifest: the entry file's directory is the project root.
    Ok((BTreeMap::new(), entry_dir))
}

/// Build the relative file path an import resolves to: `import a.b.c` → `a/b/c.helix`.
fn import_rel_path(segments: &[String]) -> PathBuf {
    let mut rel = PathBuf::new();
    for seg in &segments[..segments.len() - 1] {
        rel.push(seg);
    }
    rel.push(format!("{}.helix", segments.last().unwrap()));
    rel
}

/// Load `entry` and everything it transitively imports, returning the combined
/// statement list. A single-file program is returned unchanged (no namespacing).
/// On a lex/parse/resolve error the message is already rendered (with the
/// *correct* module's filename and caret).
pub fn load(entry: &Path) -> Result<Loaded, String> {
    let (deps, project_root) = project_context(entry)?;
    // Resolution order for an import: the importing file's own directory (local siblings),
    // then the project root, then the stdlib / `HELIX_PATH` search roots.
    let mut roots = vec![project_root];
    roots.extend(search_roots());
    let mut loader = Loader { roots, deps, ..Loader::default() };
    loader.load_file(entry, true)?;
    // Single file, no imports: hand back the unmodified AST so nothing is mangled
    // and error messages stay pristine — the overwhelmingly common case.
    if loader.modules.len() == 1 {
        let m = loader.modules.into_iter().next().unwrap();
        let span = Span { start_line: 1, source: m.source, filename: m.filename };
        return Ok(Loaded { stmts: m.stmts, spans: vec![span], multi_module: false });
    }
    // Modules are in post-order (each dependency before the module that imports it,
    // entry last), so concatenating their rewrites preserves define-before-use. Each
    // module's line numbers are offset into a global range so a runtime error's line
    // unambiguously identifies its source file (see `Loaded::locate`).
    let mut out = Vec::new();
    let mut spans = Vec::with_capacity(loader.modules.len());
    let mut offset = 0usize;
    for idx in 0..loader.modules.len() {
        spans.push(Span {
            start_line: offset + 1,
            source: loader.modules[idx].source.clone(),
            filename: loader.modules[idx].filename.clone(),
        });
        // A visibility error from the rewrite carries a *global* line; map it back to
        // this module's local line and render against this module's own source.
        let stmts = rewrite_module(&loader.modules, idx, offset).map_err(|mut e| {
            e.line = e.line.saturating_sub(offset);
            e.render(&loader.modules[idx].source, &loader.modules[idx].filename)
        })?;
        out.extend(stmts);
        offset += loader.modules[idx].source.lines().count();
    }
    Ok(Loaded { stmts: out, spans, multi_module: true })
}

struct Module {
    stmts: Vec<Stmt>,
    /// Each whole-module import's alias mapped to the loaded module's index.
    imports: Vec<(String, usize)>,
    /// Each selectively-imported name mapped to the module it came from.
    selected: Vec<(String, usize)>,
    /// The names this module marks `export` — its public surface (ADR 0019). Only these
    /// are reachable from an importer (qualified `alias.name` or selective import).
    exports: HashSet<String>,
    /// This module's own source and filename, for rendering errors against the file
    /// the erroring code actually came from (not the entry file).
    source: String,
    filename: String,
}

#[derive(Default)]
struct Loader {
    modules: Vec<Module>,
    by_path: HashMap<PathBuf, usize>,
    /// Canonical paths currently being loaded — for cycle detection.
    in_progress: Vec<PathBuf>,
    /// Search roots for non-local imports (stdlib / `HELIX_PATH`).
    roots: Vec<PathBuf>,
    /// Declared package dependencies (`name -> source directory`), resolved from the
    /// project's `helix.toml`. An `import name.module` resolves within `name`'s dir.
    deps: BTreeMap<String, PathBuf>,
}

impl Loader {
    fn load_file(&mut self, path: &Path, is_entry: bool) -> Result<usize, String> {
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
        let mut stmts = crate::parser::parse(toks).map_err(|e| e.render(&src, &fname))?;

        // Lower `import python.a.b [as alias]` into `alias = python.import("a.b")`
        // before resolving file imports — so it rides the normal pipeline (and the
        // resolver below never hunts for a `python/...helix` file). `python` itself
        // is a predefined global, so the lowered assign just calls a method on it.
        for s in stmts.iter_mut() {
            if let Stmt::Import { segments, alias, selected: _, line, col } = s
                && segments.first().map(|x| x.as_str()) == Some("python") {
                    if segments.len() < 2 {
                        return Err(HelixError::new(
                            "`import python` needs a module, e.g. `import python.numpy`",
                            *line,
                            *col,
                        )
                        .hint("Python modules import as `import python.<module> [as alias]`.")
                        .render(&src, &fname));
                    }
                    let module = segments[1..].join(".");
                    let alias = alias.clone();
                    let (l, c) = (*line, *col);
                    *s = Stmt::Assign {
                        name: alias,
                        mutable: false,
                        exported: false,
                        value: Expr::Method {
                            recv: Box::new(Expr::Ident { name: "python".to_string(), line: l, col: c }),
                            name: "import".to_string(),
                            args: vec![Expr::Str(module)],
                            named: vec![],
                            line: l,
                            col: c,
                        },
                        line: l,
                        col: c,
                    };
                }
        }

        let dir = canon.parent().unwrap_or_else(|| Path::new("."));
        let mut imports = Vec::new();
        let mut selected_names = Vec::new();
        for s in &stmts {
            if let Stmt::Import { segments, alias, selected, line, col } = s {
                // Resolve `import a.b.c` → `a/b/c.helix`, trying this module's own
                // directory first (local imports win), then each search root (stdlib /
                // `HELIX_PATH`).
                let rel = import_rel_path(segments);
                let local = dir.join(&rel);
                // A package dependency: `import dep.module` resolves within `dep`'s
                // source directory (the first segment selects the package); plain
                // `import dep` loads `dep`'s same-named module. Dependencies win over
                // ambiguous local files, matching the manifest's explicit intent.
                let dep_file = self.deps.get(&segments[0]).map(|d| {
                    if segments.len() == 1 {
                        d.join(format!("{}.helix", segments[0]))
                    } else {
                        d.join(import_rel_path(&segments[1..]))
                    }
                });
                let dep_path = if let Some(p) = dep_file.filter(|p| p.is_file()) {
                    p
                } else if local.is_file() {
                    local
                } else if let Some(found) =
                    self.roots.iter().map(|r| r.join(&rel)).find(|p| p.is_file())
                {
                    found
                } else {
                    let shown = segments.join(".");
                    let err =
                        HelixError::new(format!("cannot find module `{shown}`"), *line, *col)
                            .hint(format!(
                                "expected `{}` beside this file or under the project root",
                                rel.display()
                            ));
                    return Err(err.render(&src, &fname));
                };
                let dep_idx = self.load_file(&dep_path, false)?;
                match selected {
                    // Selective: each chosen name resolves to the dependency directly —
                    // but only if that module actually `export`s it (ADR 0019). Validate
                    // here, at the import, so the error names the module instead of
                    // surfacing later as a bare "not defined" at the use site.
                    Some(names) => {
                        for n in names {
                            if !self.modules[dep_idx].exports.contains(n) {
                                let shown = segments.join(".");
                                return Err(HelixError::new(
                                    format!("`{n}` is not exported by module `{shown}`"),
                                    *line,
                                    *col,
                                )
                                .hint(format!(
                                    "mark it `export {n} = …` / `export fn {n}(…)` in that module, or check the spelling."
                                ))
                                .render(&src, &fname));
                            }
                            // Two modules exporting the same name, both imported
                            // selectively, silently resolved to whichever came last.
                            if let Some((_, prev)) = selected_names.iter().find(|(m, _)| m == n)
                                && *prev != dep_idx
                            {
                                return Err(HelixError::new(
                                    format!("`{n}` is already imported from another module"),
                                    *line,
                                    *col,
                                )
                                .hint(format!(
                                    "two modules cannot supply the same name — import the \
                                     module itself and qualify the use (`{}.{n}`).",
                                    segments.join(".")
                                ))
                                .render(&src, &fname));
                            }
                            selected_names.push((n.clone(), dep_idx));
                        }
                    }
                    // Whole module: reached through the alias (`alias.member`).
                    None => {
                        // `import a.shared` + `import b.shared` both bind `shared` — the
                        // LAST one silently won, so `shared.who()` returned B's answer with
                        // no diagnostic, and swapping the two import lines changed the
                        // program's output. An import binds a name like anything else, so a
                        // collision between two different modules is an error. Importing the
                        // SAME module twice stays fine: it binds the same thing.
                        if let Some((_, prev)) = imports.iter().find(|(a, _)| *a == *alias)
                            && *prev != dep_idx
                        {
                            return Err(HelixError::new(
                                format!("`{alias}` is already bound to a different module"),
                                *line,
                                *col,
                            )
                            .hint(format!(
                                "`import a.{alias}` and `import b.{alias}` both bind `{alias}` \
                                 — alias one of them: `import {} as <name>`.",
                                segments.join(".")
                            ))
                            .render(&src, &fname));
                        }
                        imports.push((alias.clone(), dep_idx));
                    }
                }
            }
        }

        // A *module* (a non-entry file, loaded via `import`) may only define things —
        // functions, globals, imports. A bare top-level expression statement (a stray
        // `print(...)`, any side effect) is rejected, so importing a module never runs
        // arbitrary code (ADR 0019). The entry file is exempt: it's the script that runs.
        // Checked *after* import resolution so a cycle / missing-module error wins.
        if !is_entry {
            for s in &stmts {
                if let Stmt::Expr(e) = s {
                    let (l, c) = e.position();
                    return Err(HelixError::new(
                        "a module may only contain definitions; a bare top-level expression runs nothing here",
                        l,
                        c,
                    )
                    .hint("side effects belong in the entry file you run; in a module, wrap them in an `export fn` the caller invokes.")
                    .render(&src, &fname));
                }
            }
        }

        self.in_progress.pop();
        let exports = exported_names(&stmts);
        let idx = self.modules.len();
        self.modules.push(Module {
            stmts,
            imports,
            selected: selected_names,
            exports,
            source: src,
            filename: fname,
        });
        self.by_path.insert(canon, idx);
        Ok(idx)
    }
}

/// A module function's call signature, as the loader needs it to resolve a qualified call:
/// parameter names (to place named arguments) and per-parameter defaults (to fill omissions).
#[derive(Clone)]
struct FnSig {
    params: Vec<String>,
    defaults: Vec<Option<Expr>>,
}

/// The exported-function signatures of a module, keyed by name. Only `export`ed functions are
/// reachable as `alias.member`, so only those need resolving.
///
/// A FACADE RE-EXPORT counts as one. `export greet = inner.greet` binds the function *value*,
/// so its signature lived only in `inner` and everything the signature carries — defaults and
/// named arguments — was lost exactly one hop out:
///
///     export greet = inner.greet          # `facade.greet("hi", loud: true)` was an error
///     export fn greet(name, loud: Bool = false) = inner.greet(name, loud: loud)   # worked
///
/// A facade is the ordinary way to give a package one front door, and the wrapper form is
/// pure duplication that silently rots when the target's parameters change. Following the
/// alias makes the re-export indistinguishable from the original at the call site.
/// Terminates because the loader rejects import cycles and modules load dependency-first.
fn module_fn_sigs(m: &Module, modules: &[Module]) -> HashMap<String, FnSig> {
    let mut sigs = HashMap::new();
    for s in &m.stmts {
        match s {
            Stmt::Func { name, params, defaults, exported: true, .. } => {
                sigs.insert(
                    name.clone(),
                    FnSig {
                        params: params.iter().map(|(n, _)| n.clone()).collect(),
                        defaults: defaults.clone(),
                    },
                );
            }
            Stmt::Assign { name, exported: true, value, .. } => {
                if let Expr::Field { recv, name: member, .. } = value
                    && let Expr::Ident { name: alias, .. } = &**recv
                    && let Some((_, dep)) = m.imports.iter().find(|(a, _)| a == alias)
                    && let Some(sig) = module_fn_sigs(&modules[*dep], modules).get(member)
                {
                    sigs.insert(name.clone(), sig.clone());
                }
            }
            _ => {}
        }
    }
    sigs
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
    /// Each dependency prefix → that dependency's exported names, so a qualified
    /// `alias.member` access can be checked against the module's public surface.
    import_exports: HashMap<String, HashSet<String>>,
    /// Each dependency prefix → (exported function name → its signature: parameter names +
    /// per-parameter defaults). A qualified call `alias.f(...)` resolves named arguments and
    /// fills omitted defaults against this at load time — the parser can't, because it only
    /// sees same-file signatures. So the runtime still receives a plain positional call.
    import_sigs: HashMap<String, HashMap<String, FnSig>>,
    /// Each selectively-imported name → its dependency's prefix, so a bare reference
    /// rewrites to the dependency's mangled name.
    selected: HashMap<String, String>,
    /// Added to every line number, mapping this module into its global line range.
    line_offset: usize,
}

/// Add `off` to a statement's own line number (its sub-expressions are handled by `rw`).
fn offset_stmt_line(s: &mut Stmt, off: usize) {
    match s {
        Stmt::Assign { line, .. }
        | Stmt::Destructure { line, .. }
        | Stmt::Func { line, .. }
        | Stmt::Import { line, .. } => *line += off,
        Stmt::Expr(_) => {}
    }
}

/// Add `off` to an expression node's own line number (line-bearing variants only).
fn offset_expr_line(e: &mut Expr, off: usize) {
    match e {
        Expr::Ident { line, .. }
        | Expr::Field { line, .. }
        | Expr::Unary { line, .. }
        | Expr::Binary { line, .. }
        | Expr::Call { line, .. }
        | Expr::Method { line, .. }
        | Expr::Index { line, .. }
        | Expr::Slice { line, .. }
        | Expr::If { line, .. }
        | Expr::Try { line, .. } => *line += off,
        _ => {}
    }
}

fn rewrite_module(
    modules: &[Module],
    idx: usize,
    line_offset: usize,
) -> Result<Vec<Stmt>, HelixError> {
    let m = &modules[idx];
    let ctx = Ctx {
        prefix: format!("m{idx}"),
        top_level: top_level_names(&m.stmts),
        imports: m
            .imports
            .iter()
            .map(|(n, dep)| (n.clone(), format!("m{dep}")))
            .collect(),
        import_exports: m
            .imports
            .iter()
            .map(|(_, dep)| (format!("m{dep}"), modules[*dep].exports.clone()))
            .collect(),
        import_sigs: m
            .imports
            .iter()
            .map(|(_, dep)| (format!("m{dep}"), module_fn_sigs(&modules[*dep], modules)))
            .collect(),
        selected: m
            .selected
            .iter()
            .map(|(n, dep)| (n.clone(), format!("m{dep}")))
            .collect(),
        line_offset,
    };
    let mut out = Vec::with_capacity(m.stmts.len());
    for s in &m.stmts {
        if matches!(s, Stmt::Import { .. }) {
            continue; // imports are resolved away
        }
        let mut s = s.clone();
        rewrite_stmt(&mut s, &ctx)?;
        out.push(s);
    }
    Ok(out)
}

/// The names a module marks `export` — its public surface (ADR 0019).
fn exported_names(stmts: &[Stmt]) -> HashSet<String> {
    let mut names = HashSet::new();
    for s in stmts {
        match s {
            Stmt::Func { name, exported: true, .. } | Stmt::Assign { name, exported: true, .. } => {
                names.insert(name.clone());
            }
            Stmt::Destructure { names: ns, exported: true, .. } => {
                names.extend(ns.iter().cloned());
            }
            _ => {}
        }
    }
    names
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

fn rewrite_stmt(s: &mut Stmt, ctx: &Ctx) -> Result<(), HelixError> {
    offset_stmt_line(s, ctx.line_offset);
    match s {
        Stmt::Func { name, params, body, .. } => {
            *name = mangle(&ctx.prefix, name);
            let bound: HashSet<String> = params.iter().map(|(p, _)| p.clone()).collect();
            rw(body, ctx, &bound)?;
        }
        Stmt::Assign { name, value, .. } => {
            *name = mangle(&ctx.prefix, name);
            rw(value, ctx, &HashSet::new())?;
        }
        Stmt::Destructure { names, value, .. } => {
            for n in names.iter_mut() {
                *n = mangle(&ctx.prefix, n);
            }
            rw(value, ctx, &HashSet::new())?;
        }
        Stmt::Expr(e) => rw(e, ctx, &HashSet::new())?,
        Stmt::Import { .. } => {}
    }
    Ok(())
}

/// Rewrite an expression in place. `bound` is the set of local names in scope
/// (parameters, `let` bindings) — those are never mangled. Fails if a qualified
/// `alias.member` reaches a name the dependency doesn't `export` (ADR 0019).
fn rw(e: &mut Expr, ctx: &Ctx, bound: &HashSet<String>) -> Result<(), HelixError> {
    // Offset this node's line into the module's global range first, so any node the
    // rewrites below synthesize from `*line` inherits the corrected number.
    offset_expr_line(e, ctx.line_offset);
    // `dep.member(...)` / `dep.member` where `dep` is an imported module → a direct
    // reference to the dependency's mangled name. Handled before generic recursion
    // because they replace the whole node. The member must be exported by the module.
    if let Expr::Method { recv, name, args, named, line, col } = e
        && let Some(dep) = module_of(recv, ctx, bound) {
            check_exported(ctx, recv, &dep, name, *line, *col)?;
            for a in args.iter_mut() {
                rw(a, ctx, bound)?;
            }
            for (_, v) in named.iter_mut() {
                rw(v, ctx, bound)?;
            }
            // Resolve named arguments and omitted defaults against the callee's signature —
            // the parser couldn't, since `dep`'s definition lives in another file. The runtime
            // only ever sees the resulting plain positional call.
            let resolved = resolve_qualified_call(
                ctx,
                &dep,
                name,
                std::mem::take(args),
                std::mem::take(named),
                *line,
                *col,
            )?;
            *e = Expr::Call { name: mangle(&dep, name), args: resolved, line: *line, col: *col };
            return Ok(());
        }
    if let Expr::Field { recv, name, line, col } = e
        && let Some(dep) = module_of(recv, ctx, bound) {
            check_exported(ctx, recv, &dep, name, *line, *col)?;
            *e = Expr::Ident { name: mangle(&dep, name), line: *line, col: *col };
            return Ok(());
        }

    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => {}
        // A `@column` names a frame column, never an imported binding — leave it.
        Expr::Column { .. } => {}
        Expr::Ident { name, .. } => {
            if !bound.contains(name) {
                if ctx.top_level.contains(name) {
                    *name = mangle(&ctx.prefix, name);
                } else if let Some(dep) = ctx.selected.get(name) {
                    *name = mangle(dep, name);
                }
            }
        }
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(e, _) = p {
                    rw(e, ctx, bound)?;
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                rw(x, ctx, bound)?;
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                rw(v, ctx, bound)?;
            }
        }
        Expr::RecordUpdate { base, fields, .. } => {
            rw(base, ctx, bound)?;
            for (_, v) in fields {
                rw(v, ctx, bound)?;
            }
        }
        Expr::Field { recv, .. } => rw(recv, ctx, bound)?,
        Expr::Unary { expr, .. } => rw(expr, ctx, bound)?,
        Expr::Binary { left, right, .. } => {
            rw(left, ctx, bound)?;
            rw(right, ctx, bound)?;
        }
        Expr::Call { name, args, .. } => {
            for a in args.iter_mut() {
                rw(a, ctx, bound)?;
            }
            if !bound.contains(name) {
                // A module-local definition (or selectively-imported name) of the same
                // name as a builtin *shadows* it — so it must mangle to the user's
                // function even though `lookup` would find the builtin. Only a name that
                // is neither defined here nor imported is left bare to hit the builtin.
                // (Matches the `Ident` arm, which already mangles `top_level` first.)
                if ctx.top_level.contains(name) {
                    *name = mangle(&ctx.prefix, name);
                } else if let Some(dep) = ctx.selected.get(name) {
                    *name = mangle(dep, name);
                }
            }
        }
        Expr::Method { recv, args, named, .. } => {
            rw(recv, ctx, bound)?;
            for a in args.iter_mut() {
                rw(a, ctx, bound)?;
            }
            // A non-module method that carries named args is a checker error, but its arg
            // values may still reference imports — rewrite them so the error (or a future
            // valid use) sees resolved names.
            for (_, v) in named.iter_mut() {
                rw(v, ctx, bound)?;
            }
        }
        Expr::CallValue { callee, args, .. } => {
            rw(callee, ctx, bound)?;
            for a in args.iter_mut() {
                rw(a, ctx, bound)?;
            }
        }
        Expr::Index { recv, index, .. } => {
            rw(recv, ctx, bound)?;
            rw(index, ctx, bound)?;
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            rw(recv, ctx, bound)?;
            for x in [start, stop, step].into_iter().flatten() {
                rw(x, ctx, bound)?;
            }
        }
        Expr::Lambda { params, body } => {
            let mut b = bound.clone();
            b.extend(params.iter().cloned());
            rw(body, ctx, &b)?;
        }
        Expr::Let { bindings, body } => {
            let mut b = bound.clone();
            for (n, v) in bindings.iter_mut() {
                rw(v, ctx, &b)?;
                b.insert(n.clone());
            }
            rw(body, ctx, &b)?;
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            rw(cond, ctx, bound)?;
            rw(then_branch, ctx, bound)?;
            rw(else_branch, ctx, bound)?;
        }
        Expr::Try { expr, .. } => rw(expr, ctx, bound)?,
        Expr::Match { scrutinee, arms, .. } => {
            rw(scrutinee, ctx, bound)?;
            for arm in arms.iter_mut() {
                let mut b = bound.clone();
                for name in crate::interp::pattern_binding_names(&arm.pattern) {
                    b.insert(name);
                }
                if let Some(g) = &mut arm.guard {
                    rw(g, ctx, &b)?;
                }
                rw(&mut arm.body, ctx, &b)?;
            }
        }
    }
    Ok(())
}

/// Enforce that `alias.member` reaches only a name the dependency `export`s. `dep` is
/// the dependency's prefix (`m<N>`); `recv` is the alias identifier (for the message).
fn check_exported(
    ctx: &Ctx,
    recv: &Expr,
    dep: &str,
    member: &str,
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    if let Some(exports) = ctx.import_exports.get(dep)
        && !exports.contains(member)
    {
        let alias = match recv {
            Expr::Ident { name, .. } => name.as_str(),
            _ => dep,
        };
        return Err(HelixError::new(
            format!("`{member}` is not exported by module `{alias}`"),
            line,
            col,
        )
        .hint(format!(
            "only `export`ed names are reachable as `{alias}.{member}`; mark it `export` in that module, or check the spelling."
        )));
    }
    Ok(())
}

/// Resolve a qualified call `dep.member(pos, named)` into the positional argument list the
/// runtime runs: place named arguments into their parameter slots and fill any omission with the
/// parameter's default, all from the callee's signature (which the parser couldn't see — the
/// definition is in another file). This mirrors the parser's own same-file resolution, so a
/// qualified call behaves exactly like a bare one. Named arguments and unknown-parameter /
/// duplicate errors are reported here (at the call's position, `line`/`col`).
///
/// When the callee's signature isn't known (a re-exported value, not a function) the arguments
/// pass through unchanged if there are no named ones; a named argument in that case is an error
/// (there's no signature to bind it to). Defaults are literal expressions, so cloning one into a
/// foreign call site is always safe.
fn resolve_qualified_call(
    ctx: &Ctx,
    dep: &str,
    member: &str,
    pos: Vec<Expr>,
    named: Vec<(String, Expr)>,
    line: usize,
    col: usize,
) -> Result<Vec<Expr>, HelixError> {
    let Some(sig) = ctx.import_sigs.get(dep).and_then(|s| s.get(member)) else {
        if named.is_empty() {
            return Ok(pos);
        }
        return Err(HelixError::new(
            format!("`{member}` cannot take named arguments"),
            line,
            col,
        )
        .hint("named arguments are only supported when calling a function; pass positionally."));
    };
    // Pure-positional fast path: fill any omitted trailing defaults (the common case).
    if named.is_empty() {
        if pos.len() >= sig.params.len() {
            return Ok(pos); // enough already (or too many → arity error downstream)
        }
        let mut out = pos;
        for d in &sig.defaults[out.len()..] {
            match d {
                Some(v) => out.push(v.clone()),
                None => break, // a required parameter with no default — arity check reports it
            }
        }
        return Ok(out);
    }
    // Named arguments present: bind each into its parameter slot, then fill the rest from
    // defaults. Positional arguments occupy the leading slots.
    let n = sig.params.len();
    if pos.len() > n {
        return Err(HelixError::new(
            format!(
                "`{member}` takes {n} parameter{}, but {} positional arguments were given",
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
                format!("`{member}` has no parameter named `{pname}`"),
                line,
                col,
            )
            .hint(format!("its parameters are: {}", sig.params.join(", "))));
        };
        if slots[idx].is_some() {
            return Err(HelixError::new(
                format!("parameter `{pname}` of `{member}` was given more than once"),
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
                        format!("`{member}` is missing an argument for parameter `{}`", sig.params[i]),
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

/// If `recv` is a bare identifier naming an imported module (not shadowed by a
/// local), return that module's prefix.
fn module_of(recv: &Expr, ctx: &Ctx, bound: &HashSet<String>) -> Option<String> {
    match recv {
        Expr::Ident { name, .. } if !bound.contains(name) => ctx.imports.get(name).cloned(),
        _ => None,
    }
}

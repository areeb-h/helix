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
    /// This module's path relative to the project root, `/`-separated -- the name it
    /// answers to inside a built program's archive (`helix build`).
    ///
    /// It is recorded HERE, at the moment of resolution, rather than derived afterwards
    /// from the canonical path, because only the resolver knows which rung of the ladder
    /// matched: a sibling, a package dependency, or a search root. Deriving it later
    /// would mean guessing, and guessing wrong is how two files end up sharing one key.
    pub key: String,
}

/// The result of loading an entry file and its import graph.
pub struct Loaded {
    pub stmts: Vec<Stmt>,
    /// Source spans in ascending global-line order, for error attribution.
    pub spans: Vec<Span>,
    /// True if more than one file was loaded (so names were namespaced and error
    /// messages need the internal `m<N>$` prefixes stripped before display).
    pub multi_module: bool,
    /// The ENTRY module's namespace prefix, when there is one — so a caller that needs a
    /// top-level name of the entry file (`fn main`) can spell it. `None` for a single
    /// file, where nothing is namespaced.
    ///
    /// It has to be the entry's specifically, not "any module's": an imported library
    /// declaring `fn main` must not become the program's entry point.
    pub entry_prefix: Option<String>,
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
fn project_context(entry: &Path) -> Result<ProjectContext, String> {
    let canon = entry
        .canonicalize()
        .map_err(|e| format!("error: cannot read `{}`: {}\n", entry.display(), e))?;
    let entry_dir = canon.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();

    // The NEAREST manifest walking up — the anchor as it has always been chosen.
    let mut near = None;
    let mut dir = Some(entry_dir.as_path());
    while let Some(d) = dir {
        if d.join("helix.toml").is_file() {
            near = Some(d.to_path_buf());
            break;
        }
        dir = d.parent();
    }
    let Some(near) = near else {
        // No manifest: the entry file's directory is the project root.
        return Ok(ProjectContext { deps: BTreeMap::new(), root: entry_dir, from_manifest: false });
    };

    // THE DECLARED AUTHORITY CEILING, INSTALLED FROM THE MANIFEST THAT GOVERNS THIS
    // PROGRAM — the package's own, not the workspace root's.
    //
    // This is the one function that knows WHICH manifest governs a program, and it runs
    // once per load, which is why the install lives here rather than in a caller that
    // would have to repeat the walk up. `install_ceiling` is single-write, so a process
    // that loads several programs keeps the first one's ceiling; that matches the way the
    // environment authority is installed, and an authority that could be replaced mid-run
    // is a thing worth not having.
    //
    // A `[capabilities]` block in a DEPENDENCY is not consulted. A library cannot grant
    // itself authority the program that imports it did not declare, which is the whole
    // point of a ceiling; the reverse — a dependency NARROWING the program — is a real
    // idea and a different feature (per-evaluation attenuation, ADR 0021).
    if let Ok(Some(m)) = crate::pkg::Manifest::load(&near)
        && let Some(caps) = m.capabilities.clone()
    {
        crate::capability::install_ceiling(caps);
    }

    // …then one further question: is this package a MEMBER of a workspace above it? If so
    // the workspace root anchors, and the member's manifest goes on meaning only "this is
    // a package" (ADR 0040).
    let root = workspace_root_for(&near)?.unwrap_or(near);

    // Resolve, and (if locked) verify the sources still match `helix.lock`.
    let dirs = crate::pkg::resolve_for_run(&root).map_err(manifest_err)?;
    Ok(ProjectContext { deps: dirs, root, from_manifest: true })
}

/// Render a manifest failure the way `project_context`'s caller expects.
fn manifest_err(e: crate::error::HelixError) -> String {
    let mut s = format!("error: {}\n", e.message);
    if let Some(h) = &e.hint {
        s.push_str(&format!("  {h}\n"));
    }
    s
}

/// The workspace root that claims `member`, or `None` when it anchors at itself.
///
/// Walks up looking for the first ancestor manifest carrying a `[workspace]` table. If it
/// lists `member`, that ancestor is the module root. If it does not, `member` anchors at
/// itself — a package vendored inside an unrelated workspace is not that workspace's
/// business, and the failed-import diagnostic names whichever root won, so the quiet case
/// is still legible.
fn workspace_root_for(member: &Path) -> Result<Option<PathBuf>, String> {
    let own = crate::pkg::Manifest::load(member).map_err(manifest_err)?;
    // A workspace root anchors at itself; nesting is one level by decision, so it never
    // looks further up.
    if own.as_ref().is_some_and(|m| m.workspace.is_some()) {
        return Ok(None);
    }
    let mut dir = member.parent();
    while let Some(d) = dir {
        if d.join("helix.toml").is_file()
            && let Some(m) = crate::pkg::Manifest::load(d).map_err(manifest_err)?
            && let Some(ws) = &m.workspace
        {
            // EVERY LISTED MEMBER MUST EXIST, checked here rather than only for the one
            // being loaded. A typo in `members` would otherwise leave that package
            // silently self-anchored — the exact failure this table exists to end — and
            // it would present as a confusing import error inside the package, far from
            // the line that caused it.
            for name in &ws.members {
                if !d.join(name).join("helix.toml").is_file() {
                    return Err(format!(
                        "error: the workspace at `{}` lists the member `{name}`, but there \
                         is no `helix.toml` in `{}`\n  a member is a package directory; \
                         remove the entry or add its manifest\n",
                        d.join("helix.toml").display(),
                        d.join(name).display()
                    ));
                }
            }
            let claimed = ws
                .members
                .iter()
                .any(|name| d.join(name).canonicalize().is_ok_and(|p| p == member));
            if !claimed {
                return Ok(None);
            }
            // A member's own `[dependencies]` would resolve against a manifest that is no
            // longer the project root, so it would declare something and do nothing.
            // Refuse rather than ignore; per-member resolution is a real feature and a
            // separate decision.
            if own.as_ref().is_some_and(|m| !m.dependencies.is_empty()) {
                return Err(format!(
                    "error: `{}` is a member of the workspace at `{}`, so its \
                     `[dependencies]` are not resolved\n  declare them in the workspace \
                     root's manifest instead\n",
                    member.join("helix.toml").display(),
                    d.join("helix.toml").display()
                ));
            }
            return Ok(Some(d.to_path_buf()));
        }
        dir = d.parent();
    }
    Ok(None)
}

/// What `project_context` found. `from_manifest` exists ONLY so a failed import can say
/// where the anchor came from.
///
/// A field report spent three experiments establishing that a `helix.toml` one directory
/// down had silently become the module root: a repo of several packages put a manifest in
/// each (which is what `helix add <name> --path <dir>` consumes), and checking a file
/// inside one anchored imports at that package instead of the repo. `cannot find module
/// `ui.parse`` was true and useless — the anchor is the whole answer and nothing printed
/// it. Naming the directory AND the file that chose it turns that investigation into one
/// command.
struct ProjectContext {
    deps: BTreeMap<String, PathBuf>,
    /// The directory in-project imports are anchored at.
    root: PathBuf,
    /// True when a `helix.toml` in `root` set it; false when `root` is just the entry
    /// file's own directory because no manifest was found above it.
    from_manifest: bool,
}

/// Build the relative file path an import resolves to: `import a.b.c` → `a/b/c.helix`.
/// Join a `/`-separated key's directory with a relative path, yielding another key.
///
/// Keys are always `/`-separated regardless of host, because a bundle built on one
/// platform must resolve identically on another.
fn key_join(parent_key: &str, rel: &Path) -> String {
    let dir = match parent_key.rfind('/') {
        Some(i) => &parent_key[..i],
        None => "",
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    if dir.is_empty() { rel } else { format!("{dir}/{rel}") }
}

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
/// A load failure, carrying BOTH halves: the rendered text every caller has always
/// printed, and the structured diagnostic when the failure has a position.
///
/// Loading used to fail with a `String` — the message already rendered — which meant a
/// parse error, the most common failure an agent or an editor hits, arrived as prose
/// with no line, column or hint to act on. The rendered form is kept rather than
/// reconstructed because it is the part that TEACHES: a 14-case sweep of the mistakes
/// agents make found eleven whose help text names the exact fix, and a machine-readable
/// diagnostic that dropped that prose in favour of a code would be a downgrade.
///
/// Some failures genuinely have no position — a missing file, an import cycle — so the
/// structured half is optional, and `From<String>` keeps those sites unchanged.
#[derive(Debug)]
pub struct Diag {
    /// What `helix check` prints. Byte-identical to what it printed before this existed.
    pub rendered: String,
    /// Present when the failure points AT something: message, line, col, hint.
    pub err: Option<HelixError>,
    /// The file the line and column refer to (a load can fail inside an import).
    pub filename: Option<String>,
}

impl From<String> for Diag {
    fn from(rendered: String) -> Self {
        Diag { rendered, err: None, filename: None }
    }
}

/// `e.into_diag(&src, &fname)` — render exactly as before, and keep the structure.
trait IntoDiag {
    fn into_diag(self, src: &str, fname: &str) -> Diag;
}

impl IntoDiag for HelixError {
    fn into_diag(self, src: &str, fname: &str) -> Diag {
        Diag {
            rendered: self.render(src, fname),
            err: Some(self),
            filename: Some(fname.to_string()),
        }
    }
}

/// Load a program, rendering any failure — the long-standing signature, and what every
/// caller that only prints an error still uses.
pub fn load(entry: &Path) -> Result<Loaded, String> {
    load_diag(entry).map_err(|d| d.rendered)
}

/// Load a program, keeping the structure of a failure (`helix check --json`).
pub fn load_diag(entry: &Path) -> Result<Loaded, Diag> {
    let ProjectContext { deps, root: project_root, from_manifest } = project_context(entry)?;
    // Resolution order for an import: the importing file's own directory (local siblings),
    // then the project root, then the stdlib / `HELIX_PATH` search roots.
    let mut roots = vec![project_root.clone()];
    roots.extend(search_roots());
    let mut loader =
        Loader { roots, deps, project_root, anchored_by_manifest: from_manifest, ..Loader::default() };
    // THE ENTRY'S KEY ANCHORS EVERY OTHER ONE. It is the entry's path relative to the
    // project root, so a program whose entry sits in a subdirectory keeps that structure:
    // `sub/main.helix` importing a sibling `util` becomes `sub/util.helix`, which stays
    // distinct from a root-level `util.helix`. Flattening to a bare file name would
    // silently merge those two.
    let entry_key = entry
        .canonicalize()
        .ok()
        .and_then(|c| {
            let root = loader.project_root.canonicalize().unwrap_or_else(|_| loader.project_root.clone());
            c.strip_prefix(&root).ok().map(|r| r.to_string_lossy().replace('\\', "/"))
        })
        .unwrap_or_else(|| {
            entry.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "main.helix".into())
        });
    loader.load_file(entry, entry_key, true)?;
    assemble(loader)
}

/// Load a program whose modules come from a built program's archive, not the filesystem.
///
/// The virtual project root is `""`, so every join the resolver performs reproduces the
/// key the build recorded: an entry of `sub/main.helix` importing a sibling `util` asks
/// for `sub/util.helix`, and a search-root import asks for `std/json.helix`. There are no
/// package dependencies, because a bundle has no manifest to consult -- the build already
/// followed them and stored what it found under the path a plain root import reproduces.
pub fn load_archive(modules: Vec<(String, String)>, entry: usize) -> Result<Loaded, Diag> {
    if entry >= modules.len() {
        return Err(Diag::from("error: this program's archive names no entry module\n".to_string()));
    }
    let entry_key = modules[entry].0.clone();
    let map: HashMap<PathBuf, String> =
        modules.into_iter().map(|(k, v)| (normalize(Path::new(&k)), v)).collect();
    let mut loader = Loader {
        roots: vec![PathBuf::new()],
        project_root: PathBuf::new(),
        store: Store::Archive(map),
        ..Loader::default()
    };
    loader.load_file(Path::new(&entry_key), entry_key.clone(), true)?;
    assemble(loader)
}

fn assemble(loader: Loader) -> Result<Loaded, Diag> {
    // Single file, no imports: hand back the unmodified AST so nothing is mangled
    // and error messages stay pristine — the overwhelmingly common case.
    if loader.modules.len() == 1 {
        let m = loader.modules.into_iter().next().unwrap();
        let span = Span { start_line: 1, source: m.source, filename: m.filename, key: m.key };
        return Ok(Loaded {
            stmts: m.stmts,
            spans: vec![span],
            multi_module: false,
            entry_prefix: None,
        });
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
            key: loader.modules[idx].key.clone(),
        });
        // A visibility error from the rewrite carries a *global* line; map it back to
        // this module's local line and render against this module's own source.
        let stmts = rewrite_module(&loader.modules, idx, offset).map_err(|mut e| {
            e.line = e.line.saturating_sub(offset);
            e.into_diag(&loader.modules[idx].source, &loader.modules[idx].filename)
        })?;
        out.extend(stmts);
        offset += loader.modules[idx].source.lines().count();
    }
    // Post-order: dependencies first, ENTRY LAST — the same ordering the loop above
    // relies on for define-before-use.
    let entry_prefix = Some(format!("m{}", loader.modules.len() - 1));
    Ok(Loaded { stmts: out, spans, multi_module: true, entry_prefix })
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
    /// See [`Span::key`].
    key: String,
}

/// Where module sources come from.
///
/// The filesystem for an interpreted run; a built program's own archive for a bundled
/// one. The point is that there is ONE resolver: `load_file` and the whole import ladder
/// run unchanged over either store, and only the three questions they ask of the world --
/// does this path exist, what is its canonical form, what does it contain -- are answered
/// differently. A bundled program and an interpreted one therefore cannot disagree about
/// what `import a.b` means, which is a stronger property than two implementations that
/// happen to agree today.
#[derive(Default)]
enum Store {
    #[default]
    Fs,
    /// Key -> source, keyed exactly as [`Span::key`] records: project-root-relative and
    /// `/`-separated. The build already resolved every import against the real root list
    /// and kept the winner, so this map needs no root list of its own.
    Archive(HashMap<PathBuf, String>),
}

/// Resolve `.` and `..` lexically. An archive has no symlinks and no working directory,
/// so this IS canonicalisation there -- and unlike the filesystem call it cannot fail,
/// which matters because a missing module must surface as the resolver's own "cannot
/// find" diagnostic rather than as an I/O error from a path that never existed.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl Store {
    fn is_file(&self, p: &Path) -> bool {
        match self {
            Store::Fs => p.is_file(),
            Store::Archive(m) => m.contains_key(&normalize(p)),
        }
    }

    fn canonicalize(&self, p: &Path) -> std::io::Result<PathBuf> {
        match self {
            Store::Fs => p.canonicalize(),
            Store::Archive(_) => Ok(normalize(p)),
        }
    }

    fn read_to_string(&self, p: &Path) -> std::io::Result<String> {
        match self {
            Store::Fs => std::fs::read_to_string(p),
            Store::Archive(m) => m.get(&normalize(p)).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "not in this program's archive")
            }),
        }
    }
}

#[derive(Default)]
struct Loader {
    /// See [`Store`].
    store: Store,
    modules: Vec<Module>,
    by_path: HashMap<PathBuf, usize>,
    /// Canonical paths currently being loaded — for cycle detection.
    in_progress: Vec<PathBuf>,
    /// Search roots for non-local imports (stdlib / `HELIX_PATH`).
    roots: Vec<PathBuf>,
    /// Declared package dependencies (`name -> source directory`), resolved from the
    /// project's `helix.toml`. An `import name.module` resolves within `name`'s dir.
    deps: BTreeMap<String, PathBuf>,
    /// The directory in-project imports are anchored at (`roots[0]`), kept by name so a
    /// diagnostic can print it without depending on the ordering of `roots`.
    project_root: PathBuf,
    /// Whether a `helix.toml` chose `project_root` — see [`ProjectContext`].
    anchored_by_manifest: bool,
}

impl Loader {
    fn load_file(&mut self, path: &Path, key: String, is_entry: bool) -> Result<usize, Diag> {
        let canon = self
            .store
            .canonicalize(path)
            .map_err(|e| format!("error: cannot read `{}`: {}\n", path.display(), e))?;
        if let Some(&i) = self.by_path.get(&canon) {
            return Ok(i); // already loaded (shared dependency)
        }
        if self.in_progress.contains(&canon) {
            return Err(Diag::from(format!(
                "error: import cycle detected involving `{}`\n",
                canon.display()
            )));
        }
        self.in_progress.push(canon.clone());

        let src = self
            .store
            .read_to_string(&canon)
            .map_err(|e| format!("error: cannot read `{}`: {}\n", canon.display(), e))?;
        let fname = canon.to_string_lossy().into_owned();
        let toks = crate::lexer::lex(&src).map_err(|e| e.into_diag(&src, &fname))?;
        let mut stmts = crate::parser::parse(toks).map_err(|e| e.into_diag(&src, &fname))?;

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
                        .into_diag(&src, &fname));
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
                            ufcs: None,
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
                // `import dep` loads `dep`'s same-named module.
                let dep_file = self.deps.get(&segments[0]).map(|d| {
                    if segments.len() == 1 {
                        d.join(format!("{}.helix", segments[0]))
                    } else {
                        d.join(import_rel_path(&segments[1..]))
                    }
                });
                // A FILE'S OWN SIBLING WINS OVER A DEPENDENCY KEY. This used to be the other
                // way round ("dependencies win over ambiguous local files, matching the
                // manifest's explicit intent"), and that is a supply-chain hazard rather than
                // a preference, because `self.deps` is the ROOT project's map and is consulted
                // for EVERY file in the graph — including files inside a dependency, which
                // have no say in it and cannot see it:
                //
                //     mathlib/helpers.helix    export fn scale(x) = x * 2   <- private sibling
                //     mathlib/mathlib.helix    import helpers ...
                //
                //     app deps {mathlib}             -> mathlib.go(10) == 20
                //     app deps {mathlib, helpers}    -> mathlib.go(10) == 1010
                //
                // A consumer adding an unrelated package named `helpers` silently rewired a
                // correct, self-contained library's private internals. Exit 0, `helix check`
                // ok, and all three engines agree — because all three are equally wrong, so
                // the differential oracle cannot see it. The author cannot defend (they do not
                // know what else will be installed) and the consumer cannot detect it.
                //
                // A file's own directory is the one thing it unambiguously owns, so it wins.
                // This does NOT fix the whole class: a module that imports a name it has no
                // sibling for can still bind a consumer's dependency. That library is already
                // broken standalone, which is far less dangerous, and closing it properly
                // means resolving each package against ITS OWN manifest — a semantics change
                // that needs an ADR of its own.
                // The KEY follows the rung, not the path. A sibling is named relative to
                // the importing module's own key; a dependency or a search-root hit is
                // named by the import itself, which is exactly what a virtual root
                // reproduces when the program runs from an archive.
                let (dep_path, dep_key) = if self.store.is_file(&local) {
                    (local, key_join(&key, &rel))
                } else if let Some(p) = dep_file.filter(|p| self.store.is_file(p)) {
                    (p, key_join("", &rel))
                } else if let Some(found) =
                    self.roots.iter().map(|r| r.join(&rel)).find(|p| self.store.is_file(p))
                {
                    (found, key_join("", &rel))
                } else {
                    let shown = segments.join(".");
                    // NAME THE ANCHOR. "under the project root" was true and unusable: the
                    // root is the whole answer to why an import failed, and a reader has
                    // no way to see it. It is `helix.toml`'s location when there is one,
                    // which is the case that surprises people, because a manifest is also
                    // how a directory declares itself a package.
                    let anchor = if self.anchored_by_manifest {
                        format!(
                            "the project root is `{}`, set by the `helix.toml` there",
                            self.project_root.display()
                        )
                    } else {
                        format!(
                            "the project root is `{}` — the entry file's own directory, as no \
                             `helix.toml` was found above it",
                            self.project_root.display()
                        )
                    };
                    let mut hint = format!(
                        "expected `{}` beside this file or under the project root; {anchor}",
                        rel.display()
                    );
                    // THE DOUBLED-SEGMENT CASE, which is the one a multi-package repo hits.
                    // With a manifest in `ui/`, `import ui.parse` written inside `ui/`
                    // resolves to `ui/ui/parse.helix`. The first segment matching the root's
                    // own name is a precise signal, so it is worth saying outright rather
                    // than leaving to be deduced from the two facts above.
                    if self.anchored_by_manifest
                        && let Some(first) = segments.first()
                        && self.project_root.file_name().is_some_and(|n| n == first.as_str())
                    {
                        hint.push_str(&format!(
                            ".\nnote: `{first}` is also the name of that root directory, so this \
                             import looks for `{}` inside it. If `{first}` is a package within a \
                             larger project, its `helix.toml` is what anchors imports here",
                            rel.display()
                        ));
                    }
                    let err = HelixError::new(format!("cannot find module `{shown}`"), *line, *col)
                        .hint(hint);
                    return Err(err.into_diag(&src, &fname));
                };
                let dep_idx = self.load_file(&dep_path, dep_key, false)?;
                match selected {
                    // Selective: each chosen name resolves to the dependency directly —
                    // but only if that module actually `export`s it (ADR 0019). Validate
                    // here, at the import, so the error names the module instead of
                    // surfacing later as a bare "not defined" at the use site.
                    Some(names) => {
                        for n in names {
                            if !self.modules[dep_idx].exports.contains(n) {
                                let shown = segments.join(".");
                                // A BUILTIN asked for by selective import is the common
                                // near-miss, and "not exported" alone reads as "this
                                // language cannot do that". A field report probing
                                // whether selective import existed at all wrote
                                // `import core.{clamp}`, got exactly that message, and
                                // recorded the FEATURE as missing — when the feature had
                                // shipped and `clamp` simply needs no import. The
                                // diagnostic was true and still produced a false
                                // negative, so it now says where the name really lives.
                                let hint = if crate::registry::is_builtin_name(n) {
                                    format!(
                                        "`{n}` is a builtin — it is already available everywhere, with no import."
                                    )
                                } else {
                                    format!(
                                        "mark it `export {n} = …` / `export fn {n}(…)` in that module, or check the spelling."
                                    )
                                };
                                return Err(HelixError::new(
                                    format!("`{n}` is not exported by module `{shown}`"),
                                    *line,
                                    *col,
                                )
                                .hint(hint)
                                .into_diag(&src, &fname));
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
                                .into_diag(&src, &fname));
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
                            .into_diag(&src, &fname));
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
                    .into_diag(&src, &fname));
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
            key,
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
    /// Each selectively-imported name → its exported signature, so a bare call
    /// `f(...)` fills omitted trailing defaults exactly as the qualified
    /// `alias.f(...)` spelling does (they used to be silently dropped here).
    selected_sigs: HashMap<String, FnSig>,
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

/// Refuse a module binding over a seeded global constant (`Interp::new`), which
/// is immutable in every file — module top-levels MANGLE, so without this check
/// `export pi = 3` silently shadowed the constant module-wide where the same
/// line in a single-file program refuses at run time.
fn refuse_seeded(name: &str, line: usize, col: usize) -> Result<(), HelixError> {
    if crate::interp::seeded_names().any(|n| n == name) {
        let (msg, hint) = crate::error::immutable_reassign(name);
        return Err(HelixError::new(msg, line, col).hint(hint));
    }
    Ok(())
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
        selected_sigs: m
            .selected
            .iter()
            .filter_map(|(n, dep)| {
                module_fn_sigs(&modules[*dep], modules).get(n).cloned().map(|s| (n.clone(), s))
            })
            .collect(),
        line_offset,
    };
    let mut out = Vec::with_capacity(m.stmts.len());
    // `mut pi = ...` stays legal — an explicit shadow, exactly as in a single
    // file — and it unlocks the name for the statements after it, mirroring the
    // interpreter's `bind` order (a plain rebind of a mutable binding is legal).
    let mut mut_shadows: HashSet<String> = HashSet::new();
    for s in &m.stmts {
        if matches!(s, Stmt::Import { .. }) {
            continue; // imports are resolved away
        }
        match s {
            Stmt::Assign { name, mutable, line, col, .. } => {
                if *mutable {
                    mut_shadows.insert(name.clone());
                } else if !mut_shadows.contains(name) {
                    refuse_seeded(name, *line + line_offset, *col)?;
                }
            }
            Stmt::Destructure { names, mutable, line, col, .. } => {
                for n in names {
                    if *mutable {
                        mut_shadows.insert(n.clone());
                    } else if !mut_shadows.contains(n) {
                        refuse_seeded(n, *line + line_offset, *col)?;
                    }
                }
            }
            Stmt::Func { name, line, col, .. } if !mut_shadows.contains(name) => {
                refuse_seeded(name, *line + line_offset, *col)?;
            }
            _ => {}
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
    if let Expr::Method { recv, name, args, named, line, col, .. } = e
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
                    // Fill omitted trailing defaults from the dependency's
                    // signature, exactly as `resolve_qualified_call` does for the
                    // qualified spelling. Defaults are literal expressions (see
                    // `Stmt::Func`), so cloning them across files is safe.
                    if let Some(sig) = ctx.selected_sigs.get(name.as_str())
                        && args.len() < sig.params.len()
                    {
                        for d in &sig.defaults[args.len()..] {
                            match d {
                                Some(v) => args.push(v.clone()),
                                None => break,
                            }
                        }
                    }
                    *name = mangle(dep, name);
                }
            }
        }
        Expr::Method { recv, name, args, named, ufcs, .. } => {
            rw(recv, ctx, bound)?;
            for a in args.iter_mut() {
                rw(a, ctx, bound)?;
            }
            // THE UFCS FALLBACK'S NAME, resolved exactly as a free call of it would be —
            // and stored beside the method name rather than replacing it, because the
            // method name is matched against type tables and must stay as written.
            //
            // The precedence is the `Expr::Call` arm's, for the same reasons: a local
            // binding shadows (so `bound` declines outright), this module's own top-level
            // definition wins over an import, and a name that is neither is left alone —
            // it can only be a real method or an error.
            //
            // Trailing defaults are NOT filled here, where the `Call` arm fills them: the
            // arguments belong to the method reading as well, and a split call site emits
            // both. A selectively-imported verb with omitted defaults therefore works
            // qualified and not in method position — recorded rather than silently
            // differing.
            if !bound.contains(name) {
                if ctx.top_level.contains(name) {
                    *ufcs = Some(mangle(&ctx.prefix, name));
                } else if let Some(dep) = ctx.selected.get(name) {
                    *ufcs = Some(mangle(dep, name));
                }
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
        Expr::Let { bindings, body, .. } => {
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
        // Same near-miss, reached the other way: `core.clamp` for a name that is a
        // builtin and therefore needs no module at all.
        let hint = if crate::registry::is_builtin_name(member) {
            format!("`{member}` is a builtin — call it directly as `{member}(…)`, with no module prefix.")
        } else {
            format!(
                "only `export`ed names are reachable as `{alias}.{member}`; mark it `export` in that module, or check the spelling."
            )
        };
        return Err(HelixError::new(
            format!("`{member}` is not exported by module `{alias}`"),
            line,
            col,
        )
        .hint(hint));
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

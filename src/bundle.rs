//! Standalone-executable packaging (`helix build`).
//!
//! A Helix program is normally `.helix` source plus an installed `helix`. That is a
//! poor distribution story for a language whose whole pitch is "single binary, no
//! dependency hell" — a collaborator needs the toolchain *and* your source tree to
//! run anything. `helix build script.helix -o tool` closes that gap: it produces one
//! native executable that runs with nothing installed.
//!
//! The mechanism is the **self-append overlay** (the same trick `deno compile` and
//! PyInstaller use): copy the running interpreter, then append the program — its
//! source, its filename, two length words and a magic marker — to the end of the
//! file. The OS loader ignores bytes past the executable image, so the copy still
//! runs as `helix`; on startup [`embedded`] reads the trailer back and runs the
//! attached program instead of parsing the command line. No compiler or toolchain is
//! needed at build time — the runtime *is* the stub.
//!
//! The overlay is an ARCHIVE of `(key, source)` pairs, so a program and everything it
//! imports travel together. v1 held exactly one source string, which is the only reason
//! `helix build` used to refuse any program with an import -- and that refusal is what
//! pushed a field user into inlining modules by hand, where they reimplemented Helix's
//! lexer and got the `{{` doubling convention wrong.
//!
//! A bundled program is loaded by the SAME resolver an interpreted one uses
//! ([`crate::module::load_archive`]); only the store beneath it changes. So an import
//! cannot resolve one way from source and another way from a bundle.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::HelixError;

/// Trailer marker — version-stamped so a future payload format can be told apart from
/// this one (an older `helix` reading a newer overlay simply sees a non-matching magic
/// and treats the file as un-bundled rather than misparsing it).
const MAGIC: &[u8; 8] = b"HLXBND01";

/// The fixed-size tail of a v1 overlay: `[name_len: u32][src_len: u64][MAGIC: 8]`.
const TRAILER_LEN: u64 = 4 + 8 + 8;

/// v2: the payload is an ARCHIVE of modules rather than a single source string.
///
/// v1 could hold exactly one file, which is the only reason `helix build` refused any
/// program with an import -- and that refusal is what pushed a field user into inlining
/// modules by hand, where they reimplemented Helix's lexer and got the `{{` doubling
/// convention wrong. Nothing about the design required one file.
///
/// v1 is still READ. `--runtime` lets a bundle be built against a different `helix`
/// binary, so the reader and the writer are not always the same version, and an old
/// runtime handed a new overlay must recognise it rather than misparse it.
const MAGIC_V2: &[u8; 8] = b"HLXBND02";

/// `[payload_len: u64][MAGIC_V2: 8]`.
const TRAILER_V2_LEN: u64 = 8 + 8;

/// A built program's embedded source: every module it needs, and which one to run.
pub struct Embedded {
    /// `key -> source`, keys being project-root-relative and `/`-separated, exactly as
    /// [`crate::module::Span::key`] recorded them at resolution time.
    pub modules: Vec<(String, String)>,
    /// Index into `modules` of the entry file.
    pub entry: usize,
}

fn read_u32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

fn read_u64(b: &[u8], at: usize) -> Option<usize> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?) as usize)
}

/// Parse a v2 payload: `[n u32][entry u32]` then `n` x `[key_len u32][src_len u64][key][src]`.
fn parse_v2(b: &[u8]) -> Option<Embedded> {
    let n = read_u32(b, 0)?;
    let entry = read_u32(b, 4)?;
    let mut at = 8;
    let mut modules = Vec::with_capacity(n);
    for _ in 0..n {
        let key_len = read_u32(b, at)?;
        let src_len = read_u64(b, at + 4)?;
        at += 12;
        let key = String::from_utf8(b.get(at..at.checked_add(key_len)?)?.to_vec()).ok()?;
        at += key_len;
        let src = String::from_utf8(b.get(at..at.checked_add(src_len)?)?.to_vec()).ok()?;
        at += src_len;
        modules.push((key, src));
    }
    if entry >= modules.len() {
        return None;
    }
    Some(Embedded { modules, entry })
}

/// If the running executable carries an embedded program (it was produced by
/// `helix build`), return its `(source, filename)`. A plain `helix` binary — whose
/// final bytes are not the magic — returns `None`, so the normal CLI runs.
///
/// This is checked once at process start, before argument parsing, so it must be
/// cheap and infallible-by-falling-back: any I/O hiccup or malformed tail yields
/// `None` (run as the ordinary interpreter) rather than an error.
pub fn embedded() -> Option<Embedded> {
    let exe = std::env::current_exe().ok()?;
    let mut f = std::fs::File::open(&exe).ok()?;
    let total = f.metadata().ok()?.len();
    if total < TRAILER_V2_LEN {
        return None;
    }
    // Magic occupies the final 8 bytes.
    f.seek(SeekFrom::End(-8)).ok()?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).ok()?;
    if &magic == MAGIC_V2 {
        f.seek(SeekFrom::End(-(TRAILER_V2_LEN as i64))).ok()?;
        let mut len_bytes = [0u8; 8];
        f.read_exact(&mut len_bytes).ok()?;
        let payload_len = u64::from_le_bytes(len_bytes);
        let whole = payload_len.checked_add(TRAILER_V2_LEN)?;
        if whole > total {
            return None;
        }
        f.seek(SeekFrom::Start(total - whole)).ok()?;
        let mut body = vec![0u8; payload_len as usize];
        f.read_exact(&mut body).ok()?;
        return parse_v2(&body);
    }
    if &magic != MAGIC {
        return None;
    }
    if total < TRAILER_LEN {
        return None;
    }
    // The two length words precede the magic.
    f.seek(SeekFrom::End(-(TRAILER_LEN as i64))).ok()?;
    let mut sizes = [0u8; 12];
    f.read_exact(&mut sizes).ok()?;
    let name_len = u32::from_le_bytes(sizes[0..4].try_into().ok()?) as u64;
    let src_len = u64::from_le_bytes(sizes[4..12].try_into().ok()?);
    // The payload body (name + source) sits just before the trailer. A corrupt length
    // that would run past the file is rejected (return None → run as plain helix).
    let body = name_len.checked_add(src_len)?;
    let payload = body.checked_add(TRAILER_LEN)?;
    if payload > total {
        return None;
    }
    f.seek(SeekFrom::Start(total - payload)).ok()?;
    let mut name = vec![0u8; name_len as usize];
    f.read_exact(&mut name).ok()?;
    let mut src = vec![0u8; src_len as usize];
    f.read_exact(&mut src).ok()?;
    // v1 held exactly one module and no import graph.
    Some(Embedded {
        modules: vec![(String::from_utf8(name).ok()?, String::from_utf8(src).ok()?)],
        entry: 0,
    })
}

/// `helix build <entry> [-o out]` — bundle a program and every module it imports into a
/// standalone executable. Validates that the program loads and type-checks (so a broken
/// program fails the *build*, not every later run of the artifact), then writes the
/// runtime + overlay to `out` (default: the entry's filename stem). Returns the output
/// path.
///
/// Must run on the big stack — `module::load` and `types::check` recurse over the AST.
/// What a built program needs from its runtime.
pub struct Built {
    pub path: PathBuf,
    /// The optional Cargo features this program's code actually reaches, sorted.
    pub features: Vec<&'static str>,
    pub bytes: u64,
    /// How many modules travelled inside the artifact.
    pub modules: usize,
    /// The runtime that was copied: a `--runtime` path, or `None` for this interpreter.
    pub runtime: Option<String>,
}

/// The optional features a program's own code requires.
///
/// This walks the loaded program with [`crate::visit::walk_stmt`], whose exhaustive match
/// means a new `Expr` variant fails compilation there rather than being silently skipped
/// here.
///
/// It can OVER-report: a user function named `read_csv` shadows the builtin, and this
/// counts the name either way. That is the safe direction -- over-reporting leaves
/// someone on a larger runtime that works, where under-reporting hands them an artifact
/// that dies on its first frame.
fn features_used(stmts: &[crate::ast::Stmt]) -> Vec<&'static str> {
    use crate::ast::Expr;
    let mut set = std::collections::BTreeSet::new();
    for s in stmts {
        crate::visit::walk_stmt(s, &mut |e| {
            let name = match e {
                Expr::Call { name, .. } | Expr::Method { name, .. } => name.as_str(),
                _ => return,
            };
            if let Some(f) = crate::registry::feature_of(name) {
                set.insert(f);
            }
        });
    }
    set.into_iter().collect()
}

pub fn build(
    entry: &Path,
    out: Option<&Path>,
    runtime: Option<&Path>,
) -> Result<Built, HelixError> {
    let mkerr = |m: String| HelixError::new(m, 0, 0);

    // Load the import graph. A single file comes back un-mangled; more than one is
    // namespaced into one statement list, and every module's source is kept in `spans`.
    let loaded = crate::module::load(entry).map_err(|rendered| {
        // `module::load` returns an already-rendered (caret-annotated) error string;
        // surface it as the message so the user sees exactly what failed.
        mkerr(rendered.trim_end().to_string())
    })?;

    // Type-check up front: a standalone exe that only fails when run would be a trap.
    crate::types::check(&loaded.stmts).map_err(|mut e| {
        let (src, filename, local) = crate::module::locate(&loaded.spans, e.line);
        e.line = local;
        mkerr(format!("the program does not type-check:\n{}", e.render(src, filename)))
    })?;

    // THE ARCHIVE. `module::load` already collected every module's source, and each span
    // carries the project-root-relative key the resolver assigned it, so a bundled program
    // and an interpreted one name their modules identically.
    //
    // A DUPLICATE KEY IS REFUSED, not resolved by arrival order. Two different files CAN
    // claim one key -- a package dependency outranks the project root in the resolution
    // ladder, so a dep's `mathlib/go.helix` and a project file of that path reached from
    // elsewhere collide -- and silently keeping whichever landed last is the same shape of
    // bug as the supply-chain hazard `module.rs` documents at length: exit 0, `helix check`
    // ok, all three engines agreeing because all three are equally wrong.
    let mut archive: Vec<(String, String)> = Vec::with_capacity(loaded.spans.len());
    let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for sp in &loaded.spans {
        if let Some(prev) = seen.insert(sp.key.as_str(), sp.filename.as_str()) {
            return Err(mkerr(format!(
                "two modules would be stored under the same name `{}` in the bundle:\n  {}\n  {}",
                sp.key, prev, sp.filename
            ))
            .hint(
                "a package dependency and a project file resolved to the same import path. \
                 Rename one, or move it, so each module has a distinct path relative to the \
                 project root.",
            ));
        }
        archive.push((sp.key.clone(), sp.source.clone()));
    }
    // Modules come back in post-order with the entry LAST (see `module::load_diag`); a
    // single-file program has exactly one span, so this is index 0 there.
    let entry_idx = archive.len().saturating_sub(1);

    // WHICH RUNTIME GETS EMBEDDED. By default the running interpreter — when `helix build`
    // is invoked, the current exe is a plain `helix` with no overlay, exactly the clean
    // runtime we want.
    //
    // `--runtime` exists because that default is a size decision made by accident. A field
    // report shipped a 120 MB web server: the invoking binary was the GATE build, so the
    // artifact carried polars, six genomics crates, the Cranelift backend and debug symbols
    // for a program that calls none of them. The same program on a `--no-default-features`
    // release runtime is 6.7 MB — smaller than the equivalent Go binary, and it still serves
    // HTTP, renders templates and reads files. The size was always a choice; there was no
    // way to make it.
    //
    // A runtime that is itself a bundle is REFUSED rather than nested: the result would carry
    // two payloads and run the inner one, which is a confusing way to ship the wrong program.
    let me = match runtime {
        Some(p) => p.to_path_buf(),
        None => std::env::current_exe()
            .map_err(|e| mkerr(format!("cannot locate the running `helix` binary: {e}")))?,
    };
    let mut image = std::fs::read(&me)
        .map_err(|e| mkerr(format!("cannot read the `helix` binary at `{}`: {e}", me.display())))?;
    let tail = |m: &[u8; 8]| image.len() >= 8 && &image[image.len() - 8..] == m;
    if tail(MAGIC) || tail(MAGIC_V2) {
        return Err(mkerr(format!(
            "`{}` is already a built program, not a runtime",
            me.display()
        ))
        .hint(
            "point `--runtime` at a plain `helix` binary. Embedding one bundle inside another \
             would carry two programs and run the inner one.",
        ));
    }

    // Append the v2 overlay: [payload][payload_len u64][MAGIC_V2], the payload being
    // [n u32][entry u32] then n x [key_len u32][src_len u64][key][src].
    let mut payload = Vec::new();
    payload.extend_from_slice(&(archive.len() as u32).to_le_bytes());
    payload.extend_from_slice(&(entry_idx as u32).to_le_bytes());
    for (k, src) in &archive {
        payload.extend_from_slice(&(k.len() as u32).to_le_bytes());
        payload.extend_from_slice(&(src.len() as u64).to_le_bytes());
        payload.extend_from_slice(k.as_bytes());
        payload.extend_from_slice(src.as_bytes());
    }
    let payload_len = payload.len() as u64;
    image.extend_from_slice(&payload);
    image.extend_from_slice(&payload_len.to_le_bytes());
    image.extend_from_slice(MAGIC_V2);

    let out_path = out.map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(entry.file_stem().map(|s| s.to_os_string()).unwrap_or_else(|| "program".into()))
    });
    std::fs::write(&out_path, &image)
        .map_err(|e| mkerr(format!("cannot write `{}`: {e}", out_path.display())))?;

    // Mark it executable (a copied-then-rewritten file does not inherit the mode bits).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&out_path) {
            let mut perm = meta.permissions();
            perm.set_mode(perm.mode() | 0o111);
            let _ = std::fs::set_permissions(&out_path, perm);
        }
    }
    Ok(Built {
        features: features_used(&loaded.stmts),
        bytes: image.len() as u64,
        modules: archive.len(),
        runtime: runtime.map(|p| p.display().to_string()),
        path: out_path,
    })
}

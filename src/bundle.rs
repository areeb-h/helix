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
//! v1 bundles **single-file** programs. A program that imports other modules is
//! rejected with a clear error (multi-module embedding is a planned follow-on);
//! gating on the module loader means that check is exact, not a source-text guess.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::HelixError;

/// Trailer marker — version-stamped so a future payload format can be told apart from
/// this one (an older `helix` reading a newer overlay simply sees a non-matching magic
/// and treats the file as un-bundled rather than misparsing it).
const MAGIC: &[u8; 8] = b"HLXBND01";

/// The fixed-size tail of the overlay: `[name_len: u32][src_len: u64][MAGIC: 8]`.
const TRAILER_LEN: u64 = 4 + 8 + 8;

/// If the running executable carries an embedded program (it was produced by
/// `helix build`), return its `(source, filename)`. A plain `helix` binary — whose
/// final bytes are not the magic — returns `None`, so the normal CLI runs.
///
/// This is checked once at process start, before argument parsing, so it must be
/// cheap and infallible-by-falling-back: any I/O hiccup or malformed tail yields
/// `None` (run as the ordinary interpreter) rather than an error.
pub fn embedded() -> Option<(String, String)> {
    let exe = std::env::current_exe().ok()?;
    let mut f = std::fs::File::open(&exe).ok()?;
    let total = f.metadata().ok()?.len();
    if total < TRAILER_LEN {
        return None;
    }
    // Magic occupies the final 8 bytes.
    f.seek(SeekFrom::End(-8)).ok()?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).ok()?;
    if &magic != MAGIC {
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
    Some((String::from_utf8(src).ok()?, String::from_utf8(name).ok()?))
}

/// `helix build <entry> [-o out]` — bundle a single-file program into a standalone
/// executable. Validates that the program loads and type-checks (so a broken program
/// fails the *build*, not every later run of the artifact), then writes the runtime +
/// overlay to `out` (default: the entry's filename stem). Returns the output path.
///
/// Must run on the big stack — `module::load` and `types::check` recurse over the AST.
pub fn build(
    entry: &Path,
    out: Option<&Path>,
    runtime: Option<&Path>,
) -> Result<PathBuf, HelixError> {
    let mkerr = |m: String| HelixError::new(m, 0, 0);

    // Load the import graph. A single file comes back un-mangled; >1 module means the
    // program imports, which v1 can't yet embed.
    let loaded = crate::module::load(entry).map_err(|rendered| {
        // `module::load` returns an already-rendered (caret-annotated) error string;
        // surface it as the message so the user sees exactly what failed.
        mkerr(rendered.trim_end().to_string())
    })?;
    if loaded.multi_module {
        return Err(mkerr(format!(
            "`helix build` can only bundle a single-file program yet, but `{}` imports other modules",
            entry.display()
        ))
        .hint("inline the imports into one file for now — multi-module bundling is coming."));
    }

    // Type-check up front: a standalone exe that only fails when run would be a trap.
    crate::types::check(&loaded.stmts).map_err(|mut e| {
        let (src, filename, local) = crate::module::locate(&loaded.spans, e.line);
        e.line = local;
        mkerr(format!("the program does not type-check:\n{}", e.render(src, filename)))
    })?;

    let source = &loaded.spans[0].source;
    let filename = entry
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "program.helix".to_string());

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
    if image.len() >= MAGIC.len() && &image[image.len() - MAGIC.len()..] == MAGIC {
        return Err(mkerr(format!(
            "`{}` is already a built program, not a runtime",
            me.display()
        ))
        .hint(
            "point `--runtime` at a plain `helix` binary. Embedding one bundle inside another \
             would carry two programs and run the inner one.",
        ));
    }

    // Append the overlay: [name][source][name_len u32][src_len u64][MAGIC].
    image.extend_from_slice(filename.as_bytes());
    image.extend_from_slice(source.as_bytes());
    image.extend_from_slice(&(filename.len() as u32).to_le_bytes());
    image.extend_from_slice(&(source.len() as u64).to_le_bytes());
    image.extend_from_slice(MAGIC);

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
    Ok(out_path)
}

//! Builtins: file readers and filesystem verbs — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

#[inline]
pub(super) fn a_read_csv(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(s) => Ok(Value::dataframe(dataframe::read_csv(s, line, col)?)),
            other => Err(type_err("read_csv", "a string path", other, line, col)),
        }
    
}

// Generic text/JSON readers — the non-tabular counterpart to read_csv,
// for reloading config, library definitions, and saved JSON across runs.

#[inline]
pub(super) fn a_read_text(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(s) => Ok(Value::Str(Rc::new(read_text_file(s, line, col)?))),
            other => Err(type_err("read_text", "a string path", other, line, col)),
        }
    
}

#[inline]
pub(super) fn a_read_json(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(s) => {
                let text = read_text_file(s, line, col)?;
                crate::json::parse(&text)
                    .map_err(|m| HelixError::new(format!("invalid JSON in `{s}`: {m}"), line, col))
            }
            other => Err(type_err("read_json", "a string path", other, line, col)),
        }
    
}

#[inline]
pub(super) fn a_file_exists(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(s) => Ok(Value::Bool(std::path::Path::new(s.as_str()).exists())),
            other => Err(type_err("file_exists", "a string path", other, line, col)),
        }
    
}

// List a directory's immediate entries as full (dir-joined) path strings,
// sorted for a reproducible order — so the result feeds straight into
// `read_csv`/`read_text` and a program can ingest "whatever files exist".

#[inline]
pub(super) fn a_read_dir(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        let dir = match &args[0] {
            Value::Str(s) => s,
            other => return Err(type_err("read_dir", "a directory path string", other, line, col)),
        };
        let rd = std::fs::read_dir(dir.as_str()).map_err(|e| {
            HelixError::new(format!("could not read directory `{dir}`: {e}"), line, col)
                .hint("check the path exists and is a directory.")
        })?;
        let mut paths = Vec::new();
        for entry in rd {
            let entry = entry.map_err(|e| {
                HelixError::new(format!("error reading directory `{dir}`: {e}"), line, col)
            })?;
            paths.push(entry.path().to_string_lossy().into_owned());
        }
        paths.sort();
        Ok(Value::array(paths.into_iter().map(|p| Value::Str(Rc::new(p))).collect()))
    
}

// Storage-lifecycle ops. `remove_file` is idempotent (false if the file
// wasn't there); `mkdir` makes the directory and any parents, returning
// whether it was newly created. Both error only on a real I/O failure.

#[inline]
pub(super) fn a_remove_file(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(s) => {
                let p = std::path::Path::new(s.as_str());
                if p.exists() {
                    std::fs::remove_file(p).map_err(|e| {
                        HelixError::new(format!("could not remove `{s}`: {e}"), line, col)
                    })?;
                    Ok(Value::Bool(true))
                } else {
                    Ok(Value::Bool(false))
                }
            }
            other => Err(type_err("remove_file", "a string path", other, line, col)),
        }
    
}

#[inline]
pub(super) fn a_mkdir(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(s) => {
                let existed = std::path::Path::new(s.as_str()).exists();
                std::fs::create_dir_all(s.as_str()).map_err(|e| {
                    HelixError::new(format!("could not create directory `{s}`: {e}"), line, col)
                })?;
                Ok(Value::Bool(!existed))
            }
            other => Err(type_err("mkdir", "a string path", other, line, col)),
        }
    
}

// ---------------------------------------------------------------------------------------
// THE DURABLE-STORAGE SUBSTRATE (ADR 0041).
//
// A field report building a versioned store on Helix found the surface was `mkdir`,
// `remove_file`, `file_exists`, `read_dir`, `read_text`, `write_to`, `append_to` — and
// concluded, correctly, that **write-temp-then-rename is not expressible**. It designed
// around the gap with signed heads and newest-that-verifies, which is a genuinely better
// design; but "the bytes reached the OS" is not durability, and no amount of cleverness
// above the filesystem can supply what the filesystem never promised.
//
// These are the missing promises, and they are deliberately the WHOLE set rather than the
// three that were asked for. A partial durability story is worse than none: a program that
// calls `fsync` and skips `sync_dir` believes it committed and did not.

/// `rename(from, to)` — the ATOMIC COMMIT primitive. Within one filesystem a rename either
/// happened or did not; a reader never observes a half-written destination, and an existing
/// destination is replaced in the same instant.
///
/// This is what makes write-temp-then-rename expressible, which is the standard shape for
/// updating a file without a window in which it is corrupt.
///
/// **Renaming across filesystems fails** (`EXDEV`) rather than silently degrading to
/// copy-then-delete, because that copy is exactly the non-atomic window callers came here
/// to avoid. The error says so.
#[inline]
pub(super) fn a_rename(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 2, line, col)?;
    let (from, to) = match (&args[0], &args[1]) {
        (Value::Str(a), Value::Str(b)) => (a, b),
        (other, Value::Str(_)) => return Err(type_err("rename", "a string path", other, line, col)),
        (_, other) => return Err(type_err("rename", "a string path", other, line, col)),
    };
    std::fs::rename(from.as_str(), to.as_str()).map_err(|e| {
        let h = if e.raw_os_error() == Some(18) {
            "`rename` is atomic only WITHIN one filesystem, so it refuses to cross a mount \
             point rather than silently copying — a copy has the half-written window you \
             came here to avoid. Write the temporary file beside its destination."
        } else {
            "check the source exists and the destination's directory does."
        };
        HelixError::new(format!("could not rename `{from}` to `{to}`: {e}"), line, col).hint(h)
    })?;
    Ok(Value::Bool(true))
}

/// `fsync(path)` — flush one file's bytes to the storage device and wait for it.
///
/// Without this, `write_to` returning means the bytes reached the OS page cache, which a
/// power loss discards. THIS IS HALF OF DURABILITY: the file's CONTENTS are safe, but the
/// directory entry naming it may not be. See [`a_sync_dir`].
#[inline]
pub(super) fn a_fsync(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 1, line, col)?;
    match &args[0] {
        Value::Str(s) => {
            let f = std::fs::File::open(s.as_str()).map_err(|e| {
                HelixError::new(format!("could not open `{s}` to sync: {e}"), line, col)
            })?;
            f.sync_all().map_err(|e| {
                HelixError::new(format!("could not sync `{s}`: {e}"), line, col)
            })?;
            Ok(Value::Bool(true))
        }
        other => Err(type_err("fsync", "a string path", other, line, col)),
    }
}

/// `sync_dir(path)` — flush a DIRECTORY entry, so a create or rename inside it survives a
/// crash. **The step everyone forgets**, and the reason this set is not just `fsync`.
///
/// After `rename(tmp, final)` the file's contents may be durable while the rename itself is
/// not: on a crash the directory can revert and the commit disappears even though `fsync`
/// reported success. Committing durably is therefore: write, `fsync` the file, `rename`,
/// `sync_dir` the parent.
///
/// **Answers `false` on a platform that cannot do it**, rather than `true`. Windows exposes
/// no directory flush through the standard library, and returning `true` there would be a
/// durability claim this cannot keep — the precise shape of lie that makes a storage engine
/// lose data on exactly one platform. A caller that needs the guarantee can test the answer.
#[inline]
pub(super) fn a_sync_dir(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 1, line, col)?;
    let s = match &args[0] {
        Value::Str(s) => s,
        other => return Err(type_err("sync_dir", "a directory path", other, line, col)),
    };
    let p = std::path::Path::new(s.as_str());
    if !p.is_dir() {
        return Err(HelixError::new(format!("`{s}` is not a directory"), line, col)
            .hint("`sync_dir` flushes a DIRECTORY entry; use `fsync` for a file."));
    }
    #[cfg(unix)]
    {
        let d = std::fs::File::open(p).map_err(|e| {
            HelixError::new(format!("could not open `{s}` to sync: {e}"), line, col)
        })?;
        d.sync_all().map_err(|e| {
            HelixError::new(format!("could not sync directory `{s}`: {e}"), line, col)
        })?;
        Ok(Value::Bool(true))
    }
    #[cfg(not(unix))]
    {
        // Not an error: the directory exists and the request was well formed. `false` is the
        // honest answer — "this platform did not flush it" — and a durability-critical
        // caller can branch on it instead of being told a comfortable lie.
        Ok(Value::Bool(false))
    }
}

/// `create_new(path, contents)` — create a file ONLY if it does not exist, atomically.
/// `true` if this call created it, `false` if it was already there (and nothing is written).
///
/// Two jobs one primitive does that `file_exists` + `write_to` cannot, because that pair has
/// a race between the two calls:
///   * **A lock, or leader election.** Whoever creates the file wins, decided by the kernel.
///   * **A safe content-addressed write.** A chunk named by its own hash must never be
///     rewritten; `false` means "already stored", which is success, not failure.
#[inline]
pub(super) fn a_create_new(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 2, line, col)?;
    let (path, body) = match (&args[0], &args[1]) {
        (Value::Str(p), Value::Str(b)) => (p, b),
        (other, Value::Str(_)) => return Err(type_err("create_new", "a string path", other, line, col)),
        (_, other) => return Err(type_err("create_new", "string contents", other, line, col)),
    };
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path.as_str()) {
        Ok(mut f) => {
            f.write_all(body.as_bytes()).map_err(|e| {
                HelixError::new(format!("could not write `{path}`: {e}"), line, col)
            })?;
            Ok(Value::Bool(true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Value::Bool(false)),
        Err(e) => Err(HelixError::new(
            format!("could not create `{path}`: {e}"),
            line,
            col,
        )),
    }
}

/// `file_size(path)` — the file's length in BYTES, from metadata alone.
///
/// O(1). `read_text(p).length()` is O(file) and counts CHARACTERS, which is a different
/// number for any non-ASCII content — so it is neither a fast nor a correct substitute.
#[inline]
pub(super) fn a_file_size(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 1, line, col)?;
    match &args[0] {
        Value::Str(s) => {
            let md = std::fs::metadata(s.as_str()).map_err(|e| {
                HelixError::new(format!("could not stat `{s}`: {e}"), line, col)
            })?;
            Ok(Value::Int(md.len() as i64))
        }
        other => Err(type_err("file_size", "a string path", other, line, col)),
    }
}

/// `read_at(path, offset, len)` — read a slice WITHOUT reading the whole file.
///
/// The primitive that makes a page-oriented store possible: every read was O(file), so an
/// index lookup paid for the entire dataset. Reads at most `len` bytes from `offset` and
/// returns what is there, so a short final page is the shorter string rather than an error
/// — the same courtesy `pread` extends.
///
/// **The slice must be valid UTF-8**, because a Helix `Str` is. A record boundary that
/// splits a multi-byte character is refused by name rather than replaced with U+FFFD, which
/// would silently corrupt the byte the caller asked for. Arbitrary binary needs a `Bytes`
/// type, which the language does not yet have; see ADR 0041's open question.
#[inline]
pub(super) fn a_read_at(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 3, line, col)?;
    let path = match &args[0] {
        Value::Str(s) => s,
        other => return Err(type_err("read_at", "a string path", other, line, col)),
    };
    let off = int_arg(&args[1], "read_at", "an offset", line, col)?;
    let len = int_arg(&args[2], "read_at", "a length", line, col)?;
    if off < 0 || len < 0 {
        return Err(HelixError::new(
            format!("`read_at` needs a non-negative offset and length, got {off} and {len}"),
            line,
            col,
        ));
    }
    use std::io::{Read, Seek};
    let mut f = std::fs::File::open(path.as_str()).map_err(|e| {
        HelixError::new(format!("could not open `{path}`: {e}"), line, col)
    })?;
    f.seek(std::io::SeekFrom::Start(off as u64)).map_err(|e| {
        HelixError::new(format!("could not seek `{path}` to {off}: {e}"), line, col)
    })?;
    let mut buf = vec![0u8; len as usize];
    let mut filled = 0usize;
    // `read` may return short without being at EOF, so loop until it returns 0.
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => {
                return Err(HelixError::new(
                    format!("could not read `{path}`: {e}"),
                    line,
                    col,
                ))
            }
        }
    }
    buf.truncate(filled);
    match String::from_utf8(buf) {
        Ok(t) => Ok(Value::Str(Rc::new(t))),
        Err(e) => Err(HelixError::new(
            format!(
                "the bytes at offset {off} of `{path}` are not valid UTF-8 (byte {})",
                off as usize + e.utf8_error().valid_up_to()
            ),
            line,
            col,
        )
        .hint(
            "a Helix string is UTF-8, so a slice that splits a multi-byte character is \
             refused rather than replaced — align the offset to a record boundary.",
        )),
    }
}

/// `write_at(path, offset, text)` — overwrite bytes in place, extending the file if the
/// write runs past the end. Returns the number of BYTES written.
///
/// `read_at`'s twin: updating one page of a store meant rewriting the whole file, which is
/// O(file) per update and — worse — replaces data that was already durable.
#[inline]
pub(super) fn a_write_at(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 3, line, col)?;
    let path = match &args[0] {
        Value::Str(s) => s,
        other => return Err(type_err("write_at", "a string path", other, line, col)),
    };
    let off = int_arg(&args[1], "write_at", "an offset", line, col)?;
    let body = match &args[2] {
        Value::Str(s) => s,
        other => return Err(type_err("write_at", "string contents", other, line, col)),
    };
    if off < 0 {
        return Err(HelixError::new(
            format!("`write_at` needs a non-negative offset, got {off}"),
            line,
            col,
        ));
    }
    use std::io::{Seek, Write};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.as_str())
        .map_err(|e| HelixError::new(format!("could not open `{path}`: {e}"), line, col))?;
    f.seek(std::io::SeekFrom::Start(off as u64)).map_err(|e| {
        HelixError::new(format!("could not seek `{path}` to {off}: {e}"), line, col)
    })?;
    f.write_all(body.as_bytes()).map_err(|e| {
        HelixError::new(format!("could not write `{path}`: {e}"), line, col)
    })?;
    Ok(Value::Int(body.len() as i64))
}

/// `truncate(path, len)` — set a file's length, discarding anything past it.
///
/// How a write-ahead log is reclaimed after a checkpoint. Growing is legal and zero-fills,
/// which is how a page file is preallocated.
#[inline]
pub(super) fn a_truncate(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 2, line, col)?;
    let path = match &args[0] {
        Value::Str(s) => s,
        other => return Err(type_err("truncate", "a string path", other, line, col)),
    };
    let len = int_arg(&args[1], "truncate", "a length", line, col)?;
    if len < 0 {
        return Err(HelixError::new(
            format!("`truncate` needs a non-negative length, got {len}"),
            line,
            col,
        ));
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path.as_str())
        .map_err(|e| HelixError::new(format!("could not open `{path}`: {e}"), line, col))?;
    f.set_len(len as u64).map_err(|e| {
        HelixError::new(format!("could not truncate `{path}` to {len}: {e}"), line, col)
    })?;
    Ok(Value::Bool(true))
}

/// `remove_dir(path)` — remove an EMPTY directory. `false` if it was not there, matching
/// `remove_file`'s idempotence.
///
/// **Empty only, and never recursive.** A recursive delete is one typo away from removing a
/// tree the caller did not name, and this language refuses to make that a one-liner. Remove
/// the contents with `read_dir` and `remove_file`, which keeps the decision at the call site.
#[inline]
pub(super) fn a_remove_dir(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 1, line, col)?;
    match &args[0] {
        Value::Str(s) => {
            let p = std::path::Path::new(s.as_str());
            if !p.exists() {
                return Ok(Value::Bool(false));
            }
            std::fs::remove_dir(p).map_err(|e| {
                HelixError::new(format!("could not remove directory `{s}`: {e}"), line, col)
                    .hint(
                        "`remove_dir` removes an EMPTY directory and is never recursive — \
                         remove the contents first with `read_dir` and `remove_file`.",
                    )
            })?;
            Ok(Value::Bool(true))
        }
        other => Err(type_err("remove_dir", "a directory path", other, line, col)),
    }
}

/// `lock_file(path)` / `try_lock_file(path)` — take a KERNEL-HELD exclusive lock.
///
/// `create_new` is atomic but its lock file does not release when the holder crashes, so the
/// next process cannot tell a live writer from a corpse. A kernel lock lives on the open
/// descriptor, so it is released by `release()`, by the handle being dropped, by the process
/// exiting, AND by the process being killed — there is nothing stale to recover.
///
/// `try_lock_file` answers `missing` when another process holds it. That is an ANSWER, not
/// an error: a store that reports "another process has this open" beats one that hangs.
///
/// ADVISORY on every platform: they exclude other lock takers, not other writers.
#[inline]
pub(super) fn a_lock_file(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    arity(name, &args, 1, line, col)?;
    let path = match &args[0] {
        Value::Str(s) => s,
        other => return Err(type_err(name, "a string path", other, line, col)),
    };
    let blocking = name == "lock_file";
    match crate::filelock::LockHandle::acquire(path.as_str(), blocking) {
        Ok(Some(h)) => Ok(Value::Lock(Rc::new(h))),
        // Only `try_lock_file` can reach this: the blocking form waits instead.
        Ok(None) => Ok(Value::Missing),
        Err(e) => Err(HelixError::new(
            format!("could not lock `{path}`: {e}"),
            line,
            col,
        )),
    }
}

/// An `Int` argument, or the type error the other verbs produce.
fn int_arg(v: &Value, who: &str, what: &str, line: usize, col: usize) -> Result<i64, HelixError> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(type_err(who, what, other, line, col)),
    }
}

// Content hash (hex SHA-256) for content-addressing / reproducibility ids.

#[inline]
pub(super) fn a_read_parquet(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(s) => Ok(Value::dataframe(dataframe::read_parquet(s, line, col)?)),
            other => Err(type_err("read_parquet", "a string path", other, line, col)),
        }
    
}

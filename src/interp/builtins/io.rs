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

// Content hash (hex SHA-256) for content-addressing / reproducibility ids.

#[inline]
pub(super) fn a_read_parquet(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(s) => Ok(Value::dataframe(dataframe::read_parquet(s, line, col)?)),
            other => Err(type_err("read_parquet", "a string path", other, line, col)),
        }
    
}

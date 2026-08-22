//! DataFrame layer — a thin shim re-exporting the **backend seam** (ADR 0012).
//!
//! The engine logic now lives behind `crate::backend`: a `DataHandle` trait (one
//! `impl` per engine, default `backend::polars`) plus a backend-agnostic column-
//! expression IR (`ColExpr`). DataFrame verbs are method calls on a `Df` handle;
//! this module just re-exports the seam's names and the active backend's readers,
//! so the interpreter/VM keep a stable `dataframe::` surface and no `polars::`
//! type ever escapes `backend/polars.rs`.

pub use crate::backend::ast_to_colexpr;
#[cfg(feature = "dataframes")]
pub use crate::backend::polars::{read_csv, read_parquet};

// Without the engine the readers still exist — they answer with the same clean
// error `build_frame` gives, so the builtin arms above this shim never change.
#[cfg(not(feature = "dataframes"))]
pub fn read_csv(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, crate::error::HelixError> {
    let _ = path;
    Err(crate::backend::no_dataframes(line, col))
}

#[cfg(not(feature = "dataframes"))]
pub fn read_parquet(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, crate::error::HelixError> {
    let _ = path;
    Err(crate::backend::no_dataframes(line, col))
}

//! DataFrame layer — a thin shim re-exporting the **backend seam** (ADR 0012).
//!
//! The engine logic now lives behind `crate::backend`: a `DataHandle` trait (one
//! `impl` per engine, default `backend::polars`) plus a backend-agnostic column-
//! expression IR (`ColExpr`). DataFrame verbs are method calls on a `Df` handle;
//! this module just re-exports the seam's names and the active backend's readers,
//! so the interpreter/VM keep a stable `dataframe::` surface and no `polars::`
//! type ever escapes `backend/polars.rs`.

pub use crate::backend::ast_to_colexpr;
#[cfg(all(feature = "dataframes", not(feature = "native-df")))]
pub use crate::backend::polars::{read_csv, read_parquet};

// Dual-engine dev build: route per the same selection build_frame uses.
#[cfg(all(feature = "dataframes", feature = "native-df"))]
pub fn read_csv(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, crate::error::HelixError> {
    if crate::backend::native_selected() {
        return crate::backend::native::read_csv(path, line, col);
    }
    crate::backend::polars::read_csv(path, line, col)
}

#[cfg(all(feature = "dataframes", feature = "native-df"))]
pub fn read_parquet(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, crate::error::HelixError> {
    if crate::backend::native_selected() {
        return crate::backend::native::read_parquet(path, line, col);
    }
    crate::backend::polars::read_parquet(path, line, col)
}

// Native-only build: CSV is native; parquet is a Stage 2 promise, said plainly.
#[cfg(all(not(feature = "dataframes"), feature = "native-df"))]
pub use crate::backend::native::read_csv;

#[cfg(all(not(feature = "dataframes"), feature = "native-df"))]
pub use crate::backend::native::read_parquet;

// Without the engine the readers still exist — they answer with the same clean
// error `build_frame` gives, so the builtin arms above this shim never change.
#[cfg(all(not(feature = "dataframes"), not(feature = "native-df")))]
pub fn read_csv(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, crate::error::HelixError> {
    let _ = path;
    Err(crate::backend::no_dataframes(line, col))
}

#[cfg(all(not(feature = "dataframes"), not(feature = "native-df")))]
pub fn read_parquet(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, crate::error::HelixError> {
    let _ = path;
    Err(crate::backend::no_dataframes(line, col))
}

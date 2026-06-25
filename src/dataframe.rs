//! DataFrame layer — a thin shim re-exporting the **backend seam** (ADR 0012).
//!
//! The engine logic now lives behind `crate::backend`: a `DataHandle` trait (one
//! `impl` per engine, default `backend::polars`) plus a backend-agnostic column-
//! expression IR (`ColExpr`). DataFrame verbs are method calls on a `Df` handle;
//! this module just re-exports the seam's names and the active backend's readers,
//! so the interpreter/VM keep a stable `dataframe::` surface and no `polars::`
//! type ever escapes `backend/polars.rs`.

pub use crate::backend::ast_to_colexpr;
pub use crate::backend::polars::{read_csv, read_parquet};

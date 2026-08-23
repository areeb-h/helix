//! Parquet for the native engine (ADR 0033 Stage 2) — the apache `parquet`
//! crate WITHOUT its arrow half. Layout:
//!   read.rs    — SerializedFileReader → typed column loop → `Col`s
//!   write.rs   — schema build + single-row-group writer, zstd like the sibling
//!   foreign.rs — flat foreign dtypes (DATE/TIMESTAMP/TIME/DECIMAL/INT96) as
//!                their string forms, matching the polars bridge's totality
//!
//! The compatibility contract: the polars engine writes zstd by default, so this
//! reader carries zstd (plus snappy/lz4/gzip); the writer emits zstd level 3 so
//! either engine opens the other's files. Nested schemas (lists/structs/maps)
//! are refused with a clean error naming the column — flat frames are the
//! engine's data model, not a parquet limitation to paper over.

mod foreign;
mod read;
mod rle;
mod write;

pub use read::read_parquet;
pub(crate) use read::{PendingCol, SendCol};
pub use write::write_parquet;

use crate::error::HelixError;

/// Every parquet error carries the path and the verb — the engine's own shape.
fn pq_err(what: &str, path: &str, e: impl std::fmt::Display, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("could not {what} parquet `{path}`: {e}"), line, col)
}

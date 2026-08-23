//! The **native DataFrame engine** (ADR 0033 Stage 1) — eager, deterministic,
//! and semantically the language itself (ADR 0034): column expressions run
//! through the interpreter's own scalar kernel, aggregations implement the
//! missing-propagation doctrine directly, and every verb reduces to a plain
//! index computation with no parallel merge order to defend against.
//!
//! Layout (one purpose per file, deliberately):
//!   columns.rs — typed storage (`Col`) + validity masks
//!   eval.rs    — ColExpr evaluation through `interp::ops::eval_binary`
//!   logic.rs   — the three strict logical ops the scalar kernel short-circuits
//!   verbs.rs   — filter/select/with/head/vstack/unique
//!   sort.rs    — multi-key stable sort, missing first
//!   join.rs    — hash join, four kinds, left-then-right order
//!   group.rs   — first-seen-order grouped aggregation
//!   fast.rs    — typed fast paths that reproduce eval.rs/group.rs exactly
//!   key.rs     — hashable row keys shared by join/group/unique
//!   csv.rs     — RFC 4180 read/write with the pinned inference policy
//!   tests.rs   — the differential campaign against the polars oracle

mod columns;
mod csv;
mod eval;
mod fast;
mod group;
mod join;
mod key;
mod logic;
mod parquet_io;
mod sort;
mod verbs;
#[cfg(test)]
mod tests;

use std::cell::OnceCell;
use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

use super::{ColData, ColExpr, DataHandle, Df};
use columns::Col;

pub use csv::read_csv;
pub use parquet_io::read_parquet;

/// One column slot: decoded data, or a pending parquet column that decodes on
/// first touch. The memo cell is shared through the frame's `Rc`, so `cache()`
/// clones and every take/read after a decode reuses it.
pub(crate) struct LazyCol {
    cell: OnceCell<Col>,
    pending: Option<parquet_io::PendingCol>,
}

impl LazyCol {
    fn eager(c: Col) -> LazyCol {
        let cell = OnceCell::new();
        let _ = cell.set(c);
        LazyCol { cell, pending: None }
    }

    fn ready(&self) -> Option<&Col> {
        self.cell.get()
    }

    /// The column, decoding it now if pending. Compute happens BEFORE the memo
    /// write, so a decode error surfaces cleanly and can retry.
    fn get(&self, line: usize, col: usize) -> Result<&Col, HelixError> {
        if let Some(c) = self.cell.get() {
            return Ok(c);
        }
        let pending = self.pending.as_ref().ok_or_else(|| {
            HelixError::new("internal: an empty column slot", line, col)
        })?;
        let computed = pending
            .decode()
            .map_err(|m| HelixError::new(format!("could not read parquet: {m}"), line, col))?;
        Ok(self.cell.get_or_init(|| computed))
    }
}

/// A frame: named columns of one shared length, possibly still on disk. The
/// storage is `Rc`-shared so `cache()` is a refcount bump and a decoded column
/// is decoded once per FILE, not per handle.
pub struct NativeFrame {
    cols: Rc<Vec<(String, LazyCol)>>,
    rows: usize,
}

impl NativeFrame {
    /// Construct from decoded columns, enforcing the frame invariants:
    /// consistent lengths and unique names — the same rules `build_frame`
    /// promises every backend checks.
    fn new(cols: Vec<(String, Col)>, line: usize, col: usize) -> Result<NativeFrame, HelixError> {
        let rows = cols.first().map(|(_, c)| c.len()).unwrap_or(0);
        for (name, c) in &cols {
            if c.len() != rows {
                return Err(HelixError::new(
                    format!("column `{name}` has {} rows, but the frame has {rows}", c.len()),
                    line,
                    col,
                ));
            }
        }
        for (i, (name, _)) in cols.iter().enumerate() {
            if cols[..i].iter().any(|(n, _)| n == name) {
                return Err(HelixError::new(format!("duplicate column `{name}`"), line, col));
            }
        }
        Ok(NativeFrame {
            cols: Rc::new(cols.into_iter().map(|(n, c)| (n, LazyCol::eager(c))).collect()),
            rows,
        })
    }

    /// Construct with pending (undecoded) parquet columns. The row count comes
    /// from the file's footer, so `count()` never forces a decode.
    pub(crate) fn new_pending(
        cols: Vec<(String, parquet_io::PendingCol)>,
        rows: usize,
    ) -> NativeFrame {
        NativeFrame {
            cols: Rc::new(
                cols.into_iter()
                    .map(|(n, p)| (n, LazyCol { cell: OnceCell::new(), pending: Some(p) }))
                    .collect(),
            ),
            rows,
        }
    }

    fn len(&self) -> usize {
        self.rows
    }

    fn width(&self) -> usize {
        self.cols.len()
    }

    /// Every column, decoded. Pending columns decode IN PARALLEL on first full
    /// materialization (the same worker path the reader uses), then memoize.
    fn columns(&self, line: usize, col: usize) -> Result<Vec<(&String, &Col)>, HelixError> {
        use rayon::prelude::*;
        let pending: Vec<(usize, &parquet_io::PendingCol)> = self
            .cols
            .iter()
            .enumerate()
            .filter(|(_, (_, lc))| lc.ready().is_none())
            .filter_map(|(i, (_, lc))| lc.pending.as_ref().map(|p| (i, p)))
            .collect();
        if !pending.is_empty() {
            // Workers produce the Send intermediates; the Rc wrapping happens
            // here on the engine thread.
            let decoded: Vec<(usize, Result<parquet_io::SendCol, String>)> = pending
                .par_iter()
                .map(|(i, p)| (*i, p.decode_send()))
                .collect();
            for (i, r) in decoded {
                let seg = r.map_err(|m| {
                    HelixError::new(format!("could not read parquet: {m}"), line, col)
                })?;
                let _ = self.cols[i].1.cell.set(parquet_io::PendingCol::finish(seg));
            }
        }
        self.cols
            .iter()
            .map(|(n, lc)| lc.get(line, col).map(|c| (n, c)))
            .collect()
    }

    /// A column by name, with the column list in the error — the same shape the
    /// polars backend answers. Decodes ONLY this column if it is pending.
    fn col(&self, name: &str, line: usize, col: usize) -> Result<&Col, HelixError> {
        match self.cols.iter().find(|(n, _)| n == name) {
            Some((_, lc)) => lc.get(line, col),
            None => {
                let names: Vec<&str> = self.cols.iter().map(|(n, _)| n.as_str()).collect();
                Err(HelixError::new(format!("no column `{name}`"), line, col)
                    .hint(format!("columns: {}.", names.join(", "))))
            }
        }
    }

    /// Gather whole rows by index — the primitive every verb reduces to.
    /// Materializes (the output frame owns real data).
    fn take(&self, idx: &[usize]) -> NativeFrame {
        let cols: Vec<(String, Col)> = match self.columns(0, 0) {
            Ok(cs) => cs.iter().map(|(n, c)| ((*n).clone(), c.take(idx))).collect(),
            // A decode failure surfaces on the fallible paths; take() has no
            // Result channel, so an unreadable pending column becomes Null —
            // the fallible verbs all call columns()/col() first in practice.
            Err(_) => self
                .cols
                .iter()
                .map(|(n, _)| (n.clone(), Col::Null { len: idx.len() }))
                .collect(),
        };
        let rows = idx.len();
        NativeFrame {
            cols: Rc::new(cols.into_iter().map(|(n, c)| (n, LazyCol::eager(c))).collect()),
            rows,
        }
    }
}

/// Build an eager native frame from backend-agnostic reader columns.
pub fn build_frame(
    columns: Vec<(String, ColData)>,
    line: usize,
    col: usize,
) -> Result<Df, HelixError> {
    let cols: Vec<(String, Col)> =
        columns.into_iter().map(|(n, d)| (n, Col::from_coldata(d))).collect();
    NativeFrame::new(cols, line, col).map(|f| Rc::new(f) as Df)
}

impl DataHandle for NativeFrame {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn column_names(&self, _line: usize, _col: usize) -> Result<Vec<String>, HelixError> {
        Ok(self.cols.iter().map(|(n, _)| n.clone()).collect())
    }

    fn filter(&self, pred: &ColExpr, line: usize, col: usize) -> Result<Df, HelixError> {
        verbs::filter(self, pred, line, col).map(|f| Rc::new(f) as Df)
    }

    fn select(&self, names: &[String], line: usize, col: usize) -> Result<Df, HelixError> {
        verbs::select(self, names, line, col).map(|f| Rc::new(f) as Df)
    }

    fn with_columns(
        &self,
        cols: &[(String, ColExpr)],
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError> {
        verbs::with_columns(self, cols, line, col).map(|f| Rc::new(f) as Df)
    }

    fn sort(&self, names: &[String], line: usize, col: usize) -> Result<Df, HelixError> {
        sort::sort(self, names, line, col).map(|f| Rc::new(f) as Df)
    }

    fn join(
        &self,
        right: &Df,
        keys: &[String],
        how: &str,
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError> {
        let Some(r) = right.as_any().downcast_ref::<NativeFrame>() else {
            return Err(HelixError::new(
                "cannot join frames from two different DataFrame engines",
                line,
                col,
            ));
        };
        join::join(self, r, keys, how, line, col).map(|f| Rc::new(f) as Df)
    }

    fn head(&self, n: usize) -> Df {
        Rc::new(verbs::head(self, n)) as Df
    }

    fn vstack(&self, bottom: &Df, line: usize, col: usize) -> Result<Df, HelixError> {
        let Some(b) = bottom.as_any().downcast_ref::<NativeFrame>() else {
            return Err(HelixError::new(
                "cannot vstack frames from two different DataFrame engines",
                line,
                col,
            ));
        };
        verbs::vstack(self, b, line, col).map(|f| Rc::new(f) as Df)
    }

    fn unique_by(&self, subset: &[String], line: usize, col: usize) -> Result<Df, HelixError> {
        verbs::unique_by(self, subset, line, col).map(|f| Rc::new(f) as Df)
    }

    fn group_agg(
        &self,
        keys: &[String],
        agg: &str,
        value_col: &str,
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError> {
        group::group_agg(self, keys, agg, value_col, line, col).map(|f| Rc::new(f) as Df)
    }

    fn row_count(&self, _line: usize, _col: usize) -> Result<usize, HelixError> {
        Ok(self.len())
    }

    fn column_values(&self, name: &str, line: usize, col: usize) -> Result<Vec<Value>, HelixError> {
        let c = self.col(name, line, col)?;
        Ok((0..c.len()).map(|i| c.get(i)).collect())
    }

    /// The storage is Rc-shared and per-column memoized — a cache IS a clone
    /// of the handle.
    fn cache(&self, _line: usize, _col: usize) -> Result<Df, HelixError> {
        Ok(Rc::new(NativeFrame { cols: self.cols.clone(), rows: self.rows }) as Df)
    }

    fn write_parquet(&self, path: &str, line: usize, col: usize) -> Result<(), HelixError> {
        parquet_io::write_parquet(self, path, line, col)
    }

    fn write_csv(&self, path: &str, sep: u8, line: usize, col: usize) -> Result<(), HelixError> {
        csv::write_csv(self, path, sep, line, col)
    }
}

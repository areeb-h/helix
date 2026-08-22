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
//!   key.rs     — hashable row keys shared by join/group/unique
//!   csv.rs     — RFC 4180 read/write with the pinned inference policy
//!   tests.rs   — the differential campaign against the polars oracle

mod columns;
mod csv;
mod eval;
mod group;
mod join;
mod key;
mod logic;
mod sort;
mod verbs;
#[cfg(test)]
mod tests;

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

use super::{ColData, ColExpr, DataHandle, Df};
use columns::Col;

pub use csv::read_csv;

/// An eager frame: named, typed columns of one shared length.
pub struct NativeFrame {
    cols: Vec<(String, Col)>,
}

impl NativeFrame {
    /// Construct, enforcing the frame invariants: at least consistent lengths
    /// and unique names — the same rules `build_frame` promises every backend
    /// checks.
    fn new(cols: Vec<(String, Col)>, line: usize, col: usize) -> Result<NativeFrame, HelixError> {
        if let Some(first) = cols.first() {
            let n = first.1.len();
            for (name, c) in &cols {
                if c.len() != n {
                    return Err(HelixError::new(
                        format!(
                            "column `{name}` has {} rows, but `{}` has {n}",
                            c.len(),
                            first.0
                        ),
                        line,
                        col,
                    ));
                }
            }
        }
        for (i, (name, _)) in cols.iter().enumerate() {
            if cols[..i].iter().any(|(n, _)| n == name) {
                return Err(HelixError::new(format!("duplicate column `{name}`"), line, col));
            }
        }
        Ok(NativeFrame { cols })
    }

    fn len(&self) -> usize {
        self.cols.first().map(|(_, c)| c.len()).unwrap_or(0)
    }

    fn width(&self) -> usize {
        self.cols.len()
    }

    fn columns(&self) -> &[(String, Col)] {
        &self.cols
    }

    /// A column by name, with the column list in the error — the same shape the
    /// polars backend answers.
    fn col(&self, name: &str, line: usize, col: usize) -> Result<&Col, HelixError> {
        self.cols.iter().find(|(n, _)| n == name).map(|(_, c)| c).ok_or_else(|| {
            let names: Vec<&str> = self.cols.iter().map(|(n, _)| n.as_str()).collect();
            HelixError::new(format!("no column `{name}`"), line, col)
                .hint(format!("columns: {}.", names.join(", ")))
        })
    }

    /// Gather whole rows by index — the primitive every verb reduces to.
    fn take(&self, idx: &[usize]) -> NativeFrame {
        NativeFrame {
            cols: self.cols.iter().map(|(n, c)| (n.clone(), c.take(idx))).collect(),
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

    /// Eager engines are their own cache.
    fn cache(&self, _line: usize, _col: usize) -> Result<Df, HelixError> {
        Ok(Rc::new(self.take(&(0..self.len()).collect::<Vec<_>>())) as Df)
    }

    fn write_parquet(&self, _path: &str, line: usize, col: usize) -> Result<(), HelixError> {
        Err(HelixError::new("this build has no parquet support", line, col)
            .hint("parquet for the native engine lands in ADR 0033 Stage 2; build with `--features dataframes` for it today."))
    }

    fn write_csv(&self, path: &str, sep: u8, line: usize, col: usize) -> Result<(), HelixError> {
        csv::write_csv(self, path, sep, line, col)
    }
}

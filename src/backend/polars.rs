//! The **Polars backend** — the default `DataHandle`. ALL `polars::` types are
//! confined to this file (ADR 0012): a Polars API break, or swapping engines,
//! touches only here. Operations are lazy (they extend the `LazyFrame` query
//! plan); Polars fuses the whole plan and executes it multi-threaded — and, with
//! the streaming engine, out-of-core — at the single `collect()` materialization
//! point (`row_count`/`column_values`/`collect_string`/`write_parquet`).
//!
//! The interesting part is `lower`: it translates the backend-agnostic
//! [`ColExpr`] (e.g. `age > 40`) into a Polars expression, so a Helix
//! `patients.where(age > 40)` runs as a native Arrow filter rather than a
//! row-by-row interpreter loop. This is what makes ADR 0003 real.

use std::any::Any;
use std::rc::Rc;

use polars::prelude::col as pcol;
use polars::prelude::*;

use super::{ColExpr, DataHandle, Df};
use crate::ast::{BinOp, UnOp};
use crate::error::HelixError;
use crate::value::Value;

/// A Polars-backed lazy frame — Helix's default DataFrame engine.
pub struct PolarsFrame {
    lf: LazyFrame,
}

/// Wrap a Polars `LazyFrame` as a backend-agnostic Helix DataFrame handle. The
/// single point where a Polars frame becomes a `Df` — used by the CSV/Parquet/VCF
/// readers and the Python bridge.
pub fn wrap_lazy(lf: LazyFrame) -> Df {
    Rc::new(PolarsFrame { lf })
}

/// Wrap an eager Polars `DataFrame` (used by the Python bridge).
pub fn from_polars_df(df: DataFrame) -> Df {
    wrap_lazy(df.lazy())
}

/// Construct a frame from backend-agnostic [`super::ColData`] columns — the genomics
/// readers' entry point, so all Polars `Column`/`DataFrame` construction stays in
/// this file. A build error (duplicate column name, length mismatch) becomes a clean
/// Helix error instead of a leaked Polars `Display`.
pub fn build_frame(
    columns: Vec<(String, super::ColData)>,
    line: usize,
    col: usize,
) -> Result<Df, HelixError> {
    use super::ColData;
    let cols: Vec<Column> = columns
        .into_iter()
        .map(|(name, data)| {
            let n: PlSmallStr = name.as_str().into();
            match data {
                ColData::Str(v) => Column::new(n, v),
                ColData::StrOpt(v) => Column::new(n, v),
                ColData::Int(v) => Column::new(n, v),
                ColData::IntOpt(v) => Column::new(n, v),
                ColData::Float(v) => Column::new(n, v),
                ColData::Bool(v) => Column::new(n, v),
            }
        })
        .collect();
    let df = DataFrame::new_infer_height(cols)
        .map_err(|e| HelixError::new(format!("could not build the table: {e}"), line, col))?;
    Ok(from_polars_df(df))
}

/// Extract the underlying `LazyFrame` from a handle, for the Python bridge (which
/// hands Arrow buffers to `polars.DataFrame`). Errors if the active backend isn't
/// Polars — the bridge is Polars/Arrow-specific by construction.
#[cfg(feature = "python")]
pub fn as_lazyframe(h: &Df, line: usize, col: usize) -> Result<LazyFrame, HelixError> {
    match h.as_any().downcast_ref::<PolarsFrame>() {
        Some(pf) => Ok(pf.lf.clone()),
        None => Err(HelixError::new(
            "the Python bridge requires the Polars DataFrame backend",
            line,
            col,
        )),
    }
}

/// Map a Polars error into a friendly Helix error at a source position.
fn pl<T>(r: PolarsResult<T>, ctx: &str, line: usize, col: usize) -> Result<T, HelixError> {
    r.map_err(|e| HelixError::new(format!("{}: {}", ctx, e), line, col))
}

/// Convert a Helix scalar into a Polars literal expression. Non-scalars are
/// rejected up front by `ast_to_colexpr`, so this stays total in practice.
fn value_to_lit(v: &Value, line: usize, col: usize) -> Result<Expr, HelixError> {
    Ok(match v {
        Value::Int(i) => lit(*i),
        Value::Float(f) => lit(*f),
        Value::Str(s) => lit(s.as_str().to_string()),
        Value::Bool(b) => lit(*b),
        Value::Missing => lit(NULL),
        other => {
            return Err(HelixError::new(
                format!(
                    "cannot use a value of type {} inside a DataFrame query",
                    other.type_name()
                ),
                line,
                col,
            ))
        }
    })
}

/// Lower the backend-agnostic [`ColExpr`] into a Polars expression (the back half
/// of the verb→engine seam; the front half is `super::ast_to_colexpr`).
fn lower(e: &ColExpr) -> Result<Expr, HelixError> {
    Ok(match e {
        ColExpr::Col(name) => pcol(name.as_str()),
        ColExpr::Lit(v) => value_to_lit(v, 0, 0)?,
        ColExpr::Unary(op, inner) => {
            let i = lower(inner)?;
            match op {
                UnOp::Neg => lit(0) - i,
                UnOp::Not => i.not(),
            }
        }
        ColExpr::Binary(op, l, r) => {
            let l = lower(l)?;
            let r = lower(r)?;
            match op {
                BinOp::Add => l + r,
                BinOp::Sub => l - r,
                BinOp::Mul => l * r,
                BinOp::Div => l / r,
                BinOp::Mod => l % r,
                BinOp::Pow => l.pow(r),
                BinOp::Eq => l.eq(r),
                BinOp::Ne => l.neq(r),
                BinOp::Lt => l.lt(r),
                BinOp::Gt => l.gt(r),
                BinOp::Le => l.lt_eq(r),
                BinOp::Ge => l.gt_eq(r),
                BinOp::And => l.and(r),
                BinOp::Or => l.or(r),
                // `col ?? default` — replace nulls with the default.
                BinOp::Coalesce => l.fill_null(r),
                // Bitwise operators have no faithful column lowering (shifts in
                // particular — Polars `.shift` is a row operation, not a bit shift),
                // so reject them in a DataFrame query rather than do the wrong thing.
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                    return Err(HelixError::new(
                        format!("bitwise operator `{}` isn't supported inside a DataFrame query", op.symbol()),
                        0,
                        0,
                    )
                    .hint("compute bitwise expressions on arrays or scalars, then build the DataFrame."));
                }
            }
        }
    })
}

/// Column names from a plan's schema — cheap (header/metadata only, no scan).
fn schema_names(lf: &LazyFrame, line: usize, col: usize) -> Result<Vec<String>, HelixError> {
    let mut lf = lf.clone();
    let schema = pl(lf.collect_schema(), "could not read DataFrame schema", line, col)?;
    Ok(schema.iter_names().map(|s| s.to_string()).collect())
}

pub fn read_csv(path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
    let lf = pl(
        LazyCsvReader::new(path.into())
            .with_has_header(true)
            .finish(),
        &format!("could not open CSV `{}`", path),
        line,
        col,
    )?;
    Ok(wrap_lazy(lf))
}

pub fn read_parquet(path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
    let lf = pl(
        LazyFrame::scan_parquet(path.into(), ScanArgsParquet::default()),
        &format!("could not open Parquet `{}`", path),
        line,
        col,
    )?;
    Ok(wrap_lazy(lf))
}

/// Convert one Polars cell to a Helix value. Nulls map to `missing`; integers and
/// floats of any width widen to `Int`/`Float`; strings and booleans map across
/// directly; any remaining dtype (dates, categoricals, …) falls back to its string
/// form so the conversion is total and never panics.
fn anyvalue_to_value(av: &AnyValue) -> Value {
    match av {
        AnyValue::Null => Value::Missing,
        AnyValue::Boolean(b) => Value::Bool(*b),
        AnyValue::Int8(n) => Value::Int(*n as i64),
        AnyValue::Int16(n) => Value::Int(*n as i64),
        AnyValue::Int32(n) => Value::Int(*n as i64),
        AnyValue::Int64(n) => Value::Int(*n),
        AnyValue::UInt8(n) => Value::Int(*n as i64),
        AnyValue::UInt16(n) => Value::Int(*n as i64),
        AnyValue::UInt32(n) => Value::Int(*n as i64),
        AnyValue::UInt64(n) => Value::Int(*n as i64),
        AnyValue::Float32(f) => Value::Float(*f as f64),
        AnyValue::Float64(f) => Value::Float(*f),
        AnyValue::String(s) => Value::Str(Rc::new((*s).to_string())),
        AnyValue::StringOwned(s) => Value::Str(Rc::new(s.to_string())),
        other => Value::Str(Rc::new(other.to_string())),
    }
}

impl DataHandle for PolarsFrame {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn column_names(&self, line: usize, col: usize) -> Result<Vec<String>, HelixError> {
        schema_names(&self.lf, line, col)
    }

    fn filter(&self, pred: &ColExpr, _line: usize, _col: usize) -> Result<Df, HelixError> {
        let e = lower(pred)?;
        Ok(wrap_lazy(self.lf.clone().filter(e)))
    }

    fn select(&self, names: &[String], _line: usize, _col: usize) -> Result<Df, HelixError> {
        let exprs: Vec<Expr> = names.iter().map(|n| pcol(n.as_str())).collect();
        Ok(wrap_lazy(self.lf.clone().select(exprs)))
    }

    /// Add or replace columns from `name = expr` pairs (`df.with({bmi: weight /
    /// height})`). An expression aliased to an existing column name replaces it.
    fn with_columns(
        &self,
        cols: &[(String, ColExpr)],
        _line: usize,
        _col: usize,
    ) -> Result<Df, HelixError> {
        let mut exprs = Vec::with_capacity(cols.len());
        for (name, ce) in cols {
            exprs.push(lower(ce)?.alias(name.as_str()));
        }
        Ok(wrap_lazy(self.lf.clone().with_columns(exprs)))
    }

    fn sort(&self, names: &[String], _line: usize, _col: usize) -> Result<Df, HelixError> {
        let exprs: Vec<Expr> = names.iter().map(|n| pcol(n.as_str())).collect();
        Ok(wrap_lazy(
            self.lf.clone().sort_by_exprs(exprs, SortMultipleOptions::default()),
        ))
    }

    /// Join on one or more shared key columns (`a.join(b, id)`). `how` is `inner`
    /// (default), `left`, `right`, or `outer`; non-key columns from `right` get a
    /// `_right` suffix on any name that collides with a left column.
    fn join(
        &self,
        right: &Df,
        keys: &[String],
        how: &str,
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError> {
        let join_type = match how {
            "inner" => JoinType::Inner,
            "left" => JoinType::Left,
            "right" => JoinType::Right,
            "outer" | "full" => JoinType::Full,
            _ => {
                return Err(
                    HelixError::new(format!("unknown join type `{}`", how), line, col)
                        .hint("use \"inner\", \"left\", \"right\", or \"outer\"."),
                )
            }
        };
        let rf = match right.as_any().downcast_ref::<PolarsFrame>() {
            Some(pf) => pf,
            None => {
                return Err(HelixError::new(
                    "cannot join DataFrames from different backends",
                    line,
                    col,
                ))
            }
        };
        let left_cols = schema_names(&self.lf, line, col)?;
        let right_cols = schema_names(&rf.lf, line, col)?;
        super::validate_join_keys(&left_cols, &right_cols, keys, line, col)?;
        let on: Vec<Expr> = keys.iter().map(|k| pcol(k.as_str())).collect();
        Ok(wrap_lazy(self.lf.clone().join(
            rf.lf.clone(),
            on.clone(),
            on,
            // Coalesce the key columns for *every* join type. Without this, a `full`
            // (outer) join leaves both `key` and `key_right` with nulls split across
            // them — a different, surprising shape from inner/left/right. Coalescing
            // gives one key column uniformly (standard SQL FULL-OUTER semantics).
            JoinArgs::new(join_type)
                .with_suffix(Some("_right".into()))
                .with_coalesce(JoinCoalesce::CoalesceColumns),
        )))
    }

    fn head(&self, n: usize) -> Df {
        // Clamp rather than truncate via `as u32` (which wrapped large counts).
        wrap_lazy(self.lf.clone().limit(n.min(u32::MAX as usize) as u32))
    }

    /// One grouped aggregation: `group(keys).<agg>(value_col)`. Lazy.
    fn group_agg(
        &self,
        keys: &[String],
        agg: &str,
        value_col: &str,
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError> {
        let key_exprs: Vec<Expr> = keys.iter().map(|k| pcol(k.as_str())).collect();
        let c = pcol(value_col);
        let agg_expr = match agg {
            "mean" => c.mean(),
            "sum" => c.sum(),
            "min" => c.min(),
            "max" => c.max(),
            "count" => c.count(),
            "std" => c.std(1),
            _ => {
                return Err(
                    HelixError::new(format!("`{}` is not a grouped aggregation", agg), line, col)
                        .hint("try mean, sum, min, max, count, or std."),
                )
            }
        };
        // `group_by_stable` (not `group_by`) so the result rows come out in a
        // deterministic, first-seen group order. Plain parallel `group_by` returns
        // groups in a hash-dependent order that varies run-to-run — a reproducibility
        // hazard for a scientific language, and the sole cause of the VM/tree-walker
        // parity flakiness on the grouped examples.
        Ok(wrap_lazy(self.lf.clone().group_by_stable(key_exprs).agg([agg_expr])))
    }

    /// Row count via a `len()` pushdown — avoids materializing the columns.
    fn row_count(&self, line: usize, col: usize) -> Result<usize, HelixError> {
        let df = pl(
            self.lf.clone().select([len().alias("n")]).collect(),
            "could not count rows",
            line,
            col,
        )?;
        let n = pl(
            df.column("n").and_then(|c| c.get(0)),
            "could not count rows",
            line,
            col,
        )?;
        Ok(n.try_extract::<u64>().unwrap_or(0) as usize)
    }

    /// Materialize a single column as Helix values (`df.column("age")`). Polars
    /// nulls become `missing`, so the missing-propagation rule carries through.
    /// The column name is validated up front for a clean error.
    fn column_values(&self, name: &str, line: usize, col: usize) -> Result<Vec<Value>, HelixError> {
        let cols = schema_names(&self.lf, line, col)?;
        if !cols.iter().any(|c| c == name) {
            return Err(
                HelixError::new(format!("no column `{}` in the DataFrame", name), line, col)
                    .hint(format!("columns: {}", cols.join(", "))),
            );
        }
        let msg = format!("could not read column `{}`", name);
        let df = pl(self.lf.clone().select([pcol(name)]).collect(), &msg, line, col)?;
        let column = pl(df.column(name), &msg, line, col)?;
        let mut out = Vec::with_capacity(column.len());
        for i in 0..column.len() {
            out.push(anyvalue_to_value(&pl(column.get(i), &msg, line, col)?));
        }
        Ok(out)
    }

    /// Materialize **once** into memory and re-wrap as lazy, so later queries reuse
    /// the in-memory result instead of re-scanning the source. Eager by design.
    fn cache(&self, line: usize, col: usize) -> Result<Df, HelixError> {
        let df = pl(
            self.lf.clone().collect(),
            "could not materialize for `cache`",
            line,
            col,
        )?;
        Ok(wrap_lazy(df.lazy()))
    }

    /// Stream the lazy plan to a Parquet file via Polars' sink — bounded memory,
    /// no full materialization (the out-of-core write path for big results).
    fn write_parquet(&self, path: &str, line: usize, col: usize) -> Result<(), HelixError> {
        let dest = SinkDestination::File {
            target: SinkTarget::Path(path.into()),
        };
        let format = FileWriteFormat::Parquet(std::sync::Arc::new(ParquetWriteOptions::default()));
        let plan = pl(
            self.lf.clone().sink(dest, format, UnifiedSinkArgs::default()),
            &format!("could not set up Parquet sink for `{}`", path),
            line,
            col,
        )?;
        pl(
            plan.collect(),
            &format!("could not write Parquet `{}`", path),
            line,
            col,
        )
        .map(|_| ())
    }

    /// Materialize the lazy plan and serialize it as delimited text via Polars'
    /// `CsvWriter` (the stable write API; the streaming sink's CSV format is more
    /// volatile). CSV writing itself is fast — the cost is the one `collect`.
    fn write_csv(&self, path: &str, sep: u8, line: usize, col: usize) -> Result<(), HelixError> {
        let mut df = pl(
            self.lf.clone().collect(),
            &format!("could not materialize for CSV `{}`", path),
            line,
            col,
        )?;
        let mut file = std::fs::File::create(path).map_err(|e| {
            HelixError::new(format!("could not create `{}`: {}", path, e), line, col)
        })?;
        CsvWriter::new(&mut file)
            .with_separator(sep)
            .finish(&mut df)
            .map_err(|e| HelixError::new(format!("could not write CSV `{}`: {}", path, e), line, col))
    }

    fn collect_string(&self) -> Result<String, String> {
        match self.lf.clone().collect() {
            Ok(df) => Ok(format!("{}", df)),
            Err(e) => Err(format!("{}", e)),
        }
    }
}

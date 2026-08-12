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
    /// True when the rows of this plan come from a **CSV text scan**.
    ///
    /// CSV is the one source where Polars can answer "how many rows?" from a
    /// machinery that never runs the parser: `select(len())` over a bare CSV scan
    /// is rewritten to a newline/quote scan of the bytes (`FunctionIR::FastCount`,
    /// and the 0-width-projection height path behind it). That count can *survive
    /// a file the parser cannot read*, so `count()` used to answer `1` with exit 0
    /// for a CSV whose every other operation errored. See `row_count`.
    ///
    /// Parquet has no such split — its row count is metadata, and it is the same
    /// number the reader will produce — so parquet keeps the O(1) count.
    csv_source: bool,
}

fn wrap_with(lf: LazyFrame, csv_source: bool) -> Df {
    Rc::new(PolarsFrame { lf, csv_source })
}

/// Wrap a Polars `LazyFrame` as a backend-agnostic Helix DataFrame handle. The
/// single point where a Polars frame becomes a `Df` — used by the Parquet/VCF
/// readers and the Python bridge. CSV goes through [`wrap_csv_scan`].
pub fn wrap_lazy(lf: LazyFrame) -> Df {
    wrap_with(lf, false)
}

/// Wrap a lazy **CSV scan**, marking the plan so `row_count` refuses the
/// parser-free shortcut (see [`PolarsFrame::csv_source`]).
fn wrap_csv_scan(lf: LazyFrame) -> Df {
    wrap_with(lf, true)
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
    r.map_err(|e| {
        let (msg, hint) = tidy(&e.to_string());
        let err = HelixError::new(format!("{ctx}: {msg}"), line, col);
        match hint {
            Some(h) => err.hint(h),
            None => err,
        }
    })
}

/// Turn an engine error into a Helix one: the first paragraph, and a Helix-vocabulary hint.
///
/// This module's own doc comment promises that "no `polars::` type ever escapes
/// `backend/polars.rs`" — but the TEXT was escaping wholesale, and an adversarial sweep of
/// 1438 newcomer programs found six that print a Polars query plan or a block of Polars
/// keyword arguments into the user's terminal:
///
/// ```text
///   You might want to try:
///   - increasing `infer_schema_length` (e.g. `infer_schema_length=10000`),
///   - specifying correct dtype with the `schema_overrides` argument
///   - setting `ignore_errors` to `True`,
/// ```
///
/// Every one of those is a Polars kwarg with no Helix spelling, so the advice is not merely
/// noisy — it is un-actionable, and it tells the reader to reach for an API this language does
/// not have. `Consider setting 'truncate_ragged_lines=true'` is the same shape.
///
/// The rule is deliberately blunt: **keep the first paragraph, drop the rest.** Polars puts
/// the actual failure first and its suggestions after a blank line, so the boundary is
/// reliable and needs no per-message parsing that would rot at the next upgrade. A Helix hint
/// replaces the advice where the failure is one a Helix user can act on.
fn tidy(raw: &str) -> (String, Option<&'static str>) {
    let first = raw.split("\n\n").next().unwrap_or(raw).trim();
    // Polars wraps some inner errors in markdown fences; a stray ``` in a terminal is noise.
    let first = first.trim_matches('`').trim();
    // One line where the engine used several — a rendered error already carries the source
    // line and caret beneath it, so a multi-line message reads as two errors.
    let msg = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let low = raw.to_lowercase();
    let hint = if low.contains("not properly escaped") || low.contains("as dtype") {
        Some("a field is unterminated or has the wrong type for its column — check the quoting around that row.")
    } else if low.contains("more fields than defined") || low.contains("fewer fields") {
        Some("a row has a different number of fields than the header — check for a stray or missing separator.")
    } else {
        None
    };
    (msg, hint)
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
                // `//` has no faithful column lowering (Polars `/` on ints is
                // float division); compute it on arrays/scalars, then build the frame.
                BinOp::FloorDiv => {
                    return Err(HelixError::new(
                        "integer division `//` isn't supported inside a DataFrame query",
                        0,
                        0,
                    )
                    .hint("compute `//` on arrays or scalars, then build the DataFrame."));
                }
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
    // NOTE: this only reads the header and the schema-inference window. Whether the
    // FILE parses is not knowable here without reading all of it, which is the whole
    // point of a lazy scan — so the honesty is enforced at `row_count` instead.
    Ok(wrap_csv_scan(lf))
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

impl PolarsFrame {
    /// Wrap a plan derived from this one, carrying the CSV-source mark forward:
    /// `read_csv(f).select(a).count()` reads the same bytes through the same parser
    /// as `read_csv(f).count()`, so it must be held to the same honesty.
    fn derive(&self, lf: LazyFrame) -> Df {
        wrap_with(lf, self.csv_source)
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
        Ok(self.derive(self.lf.clone().filter(e)))
    }

    fn select(&self, names: &[String], _line: usize, _col: usize) -> Result<Df, HelixError> {
        let exprs: Vec<Expr> = names.iter().map(|n| pcol(n.as_str())).collect();
        Ok(self.derive(self.lf.clone().select(exprs)))
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
        Ok(self.derive(self.lf.clone().with_columns(exprs)))
    }

    fn sort(&self, names: &[String], _line: usize, _col: usize) -> Result<Df, HelixError> {
        let exprs: Vec<Expr> = names.iter().map(|n| pcol(n.as_str())).collect();
        Ok(self.derive(self.lf.clone().sort_by_exprs(exprs, SortMultipleOptions::default())))
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
        // CSV-sourced on either side ⇒ CSV-sourced: the joined plan still reads that
        // file through the parser, so its count must still be the parser's count.
        let joined = self.lf.clone().join(
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
        );
        Ok(wrap_with(joined, self.csv_source || rf.csv_source))
    }

    fn head(&self, n: usize) -> Df {
        // Clamp rather than truncate via `as u32` (which wrapped large counts).
        self.derive(self.lf.clone().limit(n.min(u32::MAX as usize) as u32))
    }

    fn vstack(&self, bottom: &Df, line: usize, col: usize) -> Result<Df, HelixError> {
        let bf = match bottom.as_any().downcast_ref::<PolarsFrame>() {
            Some(pf) => pf,
            None => {
                return Err(HelixError::new(
                    "cannot stack DataFrames from different backends",
                    line,
                    col,
                ))
            }
        };
        // Require identical columns (names + order) for a predictable row-append —
        // a mismatch is a clean error rather than a surprising null-filled diagonal.
        let top_cols = schema_names(&self.lf, line, col)?;
        let bot_cols = schema_names(&bf.lf, line, col)?;
        if top_cols != bot_cols {
            return Err(HelixError::new(
                "cannot stack DataFrames with different columns",
                line,
                col,
            )
            .hint(format!(
                "top has [{}], bottom has [{}]",
                top_cols.join(", "),
                bot_cols.join(", ")
            )));
        }
        let stacked = pl(
            concat([self.lf.clone(), bf.lf.clone()], UnionArgs::default()),
            "could not stack DataFrames",
            line,
            col,
        )?;
        Ok(wrap_with(stacked, self.csv_source || bf.csv_source))
    }

    fn unique_by(&self, subset: &[String], _line: usize, _col: usize) -> Result<Df, HelixError> {
        // Stable keep → deterministic row order (parity-safe), unlike the unordered
        // group-by. Whole-row distinct keeps the first occurrence (standard); a key
        // subset keeps the last (upsert: the newest row per key wins).
        let (sub, keep) = if subset.is_empty() {
            (None, UniqueKeepStrategy::First)
        } else {
            let names: std::sync::Arc<[PlSmallStr]> =
                subset.iter().map(|s| PlSmallStr::from_str(s)).collect();
            (Some(Selector::ByName { names, strict: true }), UniqueKeepStrategy::Last)
        };
        Ok(self.derive(self.lf.clone().unique_stable(sub, keep)))
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
        Ok(self.derive(self.lf.clone().group_by_stable(key_exprs).agg([agg_expr])))
    }

    /// Row count via a `len()` pushdown — no column is materialized.
    ///
    /// **The count must come from the same read as the data.** A bare `select(len())`
    /// over a CSV scan does not: Polars answers it by scanning bytes for record
    /// separators and never invoking the field parser, so `read_csv(bad).count()`
    /// returned `1` with exit 0 for a file on which `print(df)`, `df.column("a")`
    /// and `df.to_table()` all errored. A count that lies with a zero exit code is
    /// worse than any error message, so for CSV-sourced plans we pin one real column
    /// into the projection. Polars still streams (the extra output is one scalar, so
    /// memory stays O(1)); it just can no longer skip the parser. The count itself is
    /// unchanged for every file that parses.
    ///
    /// Cost, measured end-to-end (`helix` process wall time, min of 7) on a 113 MB /
    /// 3M-row / 4-column CSV: **55 ms → 280 ms**. It is paid only where the shortcut
    /// was reachable: `where`/`sort`/`group`/`unique` counts already parsed and are
    /// unchanged, `df.column("a").sum()` on the same file is unchanged at ~0.5 s, and
    /// `read_parquet(f).count()` stays at 10 ms — a Parquet row count *is* the number
    /// of rows the reader will produce, so there is nothing there to reconcile.
    fn row_count(&self, line: usize, col: usize) -> Result<usize, HelixError> {
        let mut exprs = vec![len().alias("n")];
        if self.csv_source {
            // Any single column forces the scan through the field parser; the first is
            // the deterministic choice. `null_count` (not `len`) because the optimizer
            // folds a column `len` straight back into the parser-free height.
            if let Some(first) = schema_names(&self.lf, line, col)?.first() {
                exprs.push(pcol(first.as_str()).null_count().alias("__helix_parsed"));
            }
        }
        let df = pl(
            self.lf.clone().select(exprs).collect(),
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
            // Through `tidy` like every other engine error. This one does not go through
            // `pl` — it returns a `String` across the seam rather than a `HelixError` — so
            // it was the last place a Polars advice block ("Consider setting
            // 'truncate_ragged_lines=true'", an option Helix does not have) still reached
            // the user, on `print(df)` of a ragged CSV.
            Err(e) => Err(tidy(&e.to_string()).0),
        }
    }
}

//! DataFrame engine — a thin, Helix-flavored layer over Polars (Arrow-backed,
//! columnar, lazy). The interesting part is `to_polars`: it translates a Helix
//! expression AST (e.g. `age > 40`) into a Polars expression, so that
//! `patients.where(age > 40)` runs as a native Polars filter rather than a
//! row-by-row interpreter loop. Bare identifiers that name a column become
//! `pcol("age")`; other identifiers resolve against Helix variables as literals.
//!
//! This is what makes ADR 0003 real: the same `where`/`map`/`sort` verbs span
//! `Array` and `DataFrame`, with the DataFrame path lowering to Arrow kernels.

use polars::prelude::*;
// Aliased because our source-position parameters are named `col`, which would
// otherwise shadow Polars' `pcol()` column constructor.
use polars::prelude::col as pcol;

use crate::ast::{BinOp, Expr as Ast, UnOp};
use crate::error::HelixError;
use crate::value::Value;

/// Map a Polars error into a friendly Helix error at a source position.
pub fn pl<T>(r: PolarsResult<T>, ctx: &str, line: usize, col: usize) -> Result<T, HelixError> {
    r.map_err(|e| HelixError::new(format!("{}: {}", ctx, e), line, col))
}

/// Convert a Helix scalar value into a Polars literal expression.
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

/// Translate a Helix AST expression into a Polars expression, resolving bare
/// names against the DataFrame's columns first, then Helix variables.
pub fn to_polars(
    e: &Ast,
    columns: &[String],
    resolve_var: &dyn Fn(&str) -> Option<Value>,
) -> Result<Expr, HelixError> {
    match e {
        Ast::Int(i) => Ok(lit(*i)),
        Ast::Float(f) => Ok(lit(*f)),
        Ast::Str(s) => Ok(lit(s.clone())),
        Ast::Bool(b) => Ok(lit(*b)),
        Ast::Missing => Ok(lit(NULL)),
        Ast::Ident { name, line, col } => {
            if columns.iter().any(|c| c == name) {
                Ok(pcol(name.as_str()))
            } else if let Some(v) = resolve_var(name) {
                value_to_lit(&v, *line, *col)
            } else {
                Err(HelixError::new(
                    format!("no column or variable named `{}`", name),
                    *line,
                    *col,
                )
                .hint(format!("available columns: {}", columns.join(", "))))
            }
        }
        Ast::Unary {
            op, expr, ..
        } => {
            let inner = to_polars(expr, columns, resolve_var)?;
            Ok(match op {
                UnOp::Neg => lit(0) - inner,
                UnOp::Not => inner.not(),
            })
        }
        Ast::Binary {
            op, left, right, ..
        } => {
            let l = to_polars(left, columns, resolve_var)?;
            let r = to_polars(right, columns, resolve_var)?;
            Ok(match op {
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
            })
        }
        _ => Err(HelixError::new(
            "this expression isn't supported inside a DataFrame query yet",
            0,
            0,
        )
        .hint("DataFrame queries support column names, literals, arithmetic, and comparisons.")),
    }
}

// ---------- DataFrame operations (all lazy: plan-building only) ----------
//
// `where`/`select`/`sort`/`group` just extend the LazyFrame query plan — they
// don't touch data. Polars fuses the whole plan and executes it multi-threaded
// (and, with the streaming engine, out-of-core) at the single `collect()` point.

/// Column names from the plan's schema — cheap (header/metadata only, no scan).
pub fn column_names(lf: &LazyFrame, line: usize, col: usize) -> Result<Vec<String>, HelixError> {
    let mut lf = lf.clone();
    let schema = pl(lf.collect_schema(), "could not read DataFrame schema", line, col)?;
    Ok(schema.iter_names().map(|s| s.to_string()).collect())
}

pub fn read_csv(path: &str, line: usize, col: usize) -> Result<LazyFrame, HelixError> {
    pl(
        LazyCsvReader::new(path.into())
            .with_has_header(true)
            .finish(),
        &format!("could not open CSV `{}`", path),
        line,
        col,
    )
}

pub fn read_parquet(path: &str, line: usize, col: usize) -> Result<LazyFrame, HelixError> {
    pl(
        LazyFrame::scan_parquet(path.into(), ScanArgsParquet::default()),
        &format!("could not open Parquet `{}`", path),
        line,
        col,
    )
}

pub fn filter(lf: &LazyFrame, predicate: Expr) -> LazyFrame {
    lf.clone().filter(predicate)
}

pub fn select(lf: &LazyFrame, names: &[String]) -> LazyFrame {
    let exprs: Vec<Expr> = names.iter().map(|n| pcol(n.as_str())).collect();
    lf.clone().select(exprs)
}

/// Add or replace columns from `name = expr` pairs (`df.with({bmi: weight / height})`).
/// An expression aliased to an existing column name replaces that column.
pub fn with_columns(lf: &LazyFrame, exprs: Vec<Expr>) -> LazyFrame {
    lf.clone().with_columns(exprs)
}

pub fn sort(lf: &LazyFrame, names: &[String]) -> LazyFrame {
    let exprs: Vec<Expr> = names.iter().map(|n| pcol(n.as_str())).collect();
    lf.clone()
        .sort_by_exprs(exprs, SortMultipleOptions::default())
}

/// Join two frames on one or more shared key columns (`a.join(b, id)`). `how` is
/// one of `inner` (the default), `left`, `right`, or `outer`. The key columns
/// must exist in both frames; non-key columns from `right` are appended, with a
/// `_right` suffix on any name that collides with a left column.
pub fn join(
    left: &LazyFrame,
    right: &LazyFrame,
    keys: &[String],
    how: &str,
    line: usize,
    col: usize,
) -> Result<LazyFrame, HelixError> {
    let join_type = match how {
        "inner" => JoinType::Inner,
        "left" => JoinType::Left,
        "right" => JoinType::Right,
        "outer" | "full" => JoinType::Full,
        _ => {
            return Err(HelixError::new(format!("unknown join type `{}`", how), line, col)
                .hint("use \"inner\", \"left\", \"right\", or \"outer\"."))
        }
    };
    // Validate keys against both schemas up front, so a typo reads as a clean Helix
    // error ("no column ... in the left frame") rather than Polars' lazy-plan dump at
    // collect time — matching the eager checking the column verbs get from `to_polars`.
    let left_cols = column_names(left, line, col)?;
    let right_cols = column_names(right, line, col)?;
    for k in keys {
        if !left_cols.contains(k) {
            return Err(HelixError::new(
                format!("no column `{}` in the left frame", k),
                line,
                col,
            )
            .hint(format!("left columns: {}", left_cols.join(", "))));
        }
        if !right_cols.contains(k) {
            return Err(HelixError::new(
                format!("no column `{}` in the right frame", k),
                line,
                col,
            )
            .hint(format!("right columns: {}", right_cols.join(", "))));
        }
    }
    let on: Vec<Expr> = keys.iter().map(|k| pcol(k.as_str())).collect();
    Ok(left.clone().join(
        right.clone(),
        on.clone(),
        on,
        JoinArgs::new(join_type).with_suffix(Some("_right".into())),
    ))
}

pub fn head(lf: &LazyFrame, n: usize) -> LazyFrame {
    // Clamp rather than truncate via `as u32` (which wrapped large counts).
    lf.clone().limit(n.min(u32::MAX as usize) as u32)
}

/// Stream the lazy plan to a Parquet file via Polars' sink — bounded memory,
/// no full materialization (the out-of-core write path for big results).
pub fn write_parquet(lf: &LazyFrame, path: &str, line: usize, col: usize) -> Result<(), HelixError> {
    let dest = SinkDestination::File {
        target: SinkTarget::Path(path.into()),
    };
    let format = FileWriteFormat::Parquet(std::sync::Arc::new(ParquetWriteOptions::default()));
    let plan = pl(
        lf.clone().sink(dest, format, UnifiedSinkArgs::default()),
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

/// Row count via a `len()` pushdown — avoids materializing the columns.
pub fn row_count(lf: &LazyFrame, line: usize, col: usize) -> Result<usize, HelixError> {
    let df = pl(
        lf.clone().select([len().alias("n")]).collect(),
        "could not count rows",
        line,
        col,
    )?;
    let n = pl(df.column("n").and_then(|c| c.get(0)), "could not count rows", line, col)?;
    Ok(n.try_extract::<u64>().unwrap_or(0) as usize)
}

/// Materialize a lazy frame **once** into memory and re-wrap it as lazy, so every
/// later query against it reuses the in-memory result instead of re-scanning the
/// source file. Eager by design: the work happens at the `.cache()` call, and the
/// result is an immutable value — so there is no stale-state risk and no hidden
/// recomputation, just one honest materialization the user asked for.
pub fn cache(lf: &LazyFrame, line: usize, col: usize) -> Result<LazyFrame, HelixError> {
    let df = pl(lf.clone().collect(), "could not materialize for `cache`", line, col)?;
    Ok(df.lazy())
}

/// One grouped aggregation: `group(keys).<agg>(value_col)`. Lazy.
pub fn group_agg(
    lf: &LazyFrame,
    keys: &[String],
    agg: &str,
    value_col: &str,
    line: usize,
    col_pos: usize,
) -> Result<LazyFrame, HelixError> {
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
            return Err(HelixError::new(
                format!("`{}` is not a grouped aggregation", agg),
                line,
                col_pos,
            )
            .hint("try mean, sum, min, max, count, or std."))
        }
    };
    Ok(lf.clone().group_by(key_exprs).agg([agg_expr]))
}

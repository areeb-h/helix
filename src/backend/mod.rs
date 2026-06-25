//! The DataFrame **backend seam** (ADR 0012). Helix's DataFrame verbs are defined
//! against a small typed column-expression IR (`ColExpr`) and an object-safe
//! `DataHandle` trait — **not** against any one engine. The default (and today the
//! only) backend is Polars (`backend::polars`); a homegrown columnar engine and an
//! optional DuckDB backend are future `impl DataHandle`s behind this same seam.
//!
//! Why a seam: Polars' Rust API is officially unstable (0.x, breaking every few
//! months, no upgrade guides). Pinning the *language* to it is the real risk.
//! Routing every verb through `DataHandle` confines all `polars::` types to
//! `backend/polars.rs`, so an API break — or an engine swap — touches one file,
//! never the interpreter, VM, type checker, or `Value`.
//!
//! The front half of the verb→engine lowering (`ast_to_colexpr`) and the shared,
//! engine-agnostic diagnostics (`validate_join_keys`) live here so every backend
//! produces identical Helix error messages. The back half (`ColExpr` → engine
//! expression) is each backend's own business.

pub mod polars;

use std::rc::Rc;

use crate::ast::{BinOp, Expr as Ast, UnOp};
use crate::error::HelixError;
use crate::value::Value;

/// A backend-agnostic, reference-counted DataFrame handle. Cloning is O(1) (an
/// `Rc` bump); the concrete engine lives behind the trait object.
pub type Df = Rc<dyn DataHandle>;

/// Backend-agnostic column data for **constructing** a frame from native readers
/// (VCF/GFF/BED). A reader emits `Vec<(String, ColData)>` and never touches an
/// engine type; [`build_frame`] turns it into a [`Df`]. This keeps the
/// "polars types live in one file" seam intact for the construction path too.
pub enum ColData {
    /// A non-null string column.
    Str(Vec<String>),
    /// A nullable string column (`None` → null).
    StrOpt(Vec<Option<String>>),
    /// A non-null 64-bit integer column.
    Int(Vec<i64>),
    /// A nullable integer column.
    IntOpt(Vec<Option<i64>>),
    /// A nullable float column.
    Float(Vec<Option<f64>>),
    /// A boolean column (no nulls — e.g. a VCF Flag: present → true).
    Bool(Vec<bool>),
}

/// Build an eager DataFrame from backend-agnostic [`ColData`] columns, routing any
/// engine error (mismatched lengths, a duplicate column name) through the seam as a
/// clean Helix error rather than leaking a raw engine message. Delegates to the
/// active backend's constructor.
pub fn build_frame(
    columns: Vec<(String, ColData)>,
    line: usize,
    col: usize,
) -> Result<Df, HelixError> {
    polars::build_frame(columns, line, col)
}

/// A column expression in a DataFrame query (`age > 40`, `weight / height`),
/// already **resolved**: bare names that match a column are `Col`; other names
/// were resolved against Helix variables to a `Lit`. Backend-agnostic — each
/// backend lowers it to its own expression type (a Polars `Expr`, or a fused
/// compute kernel).
#[derive(Debug, Clone)]
pub enum ColExpr {
    Col(String),
    Lit(Value),
    Unary(UnOp, Box<ColExpr>),
    Binary(BinOp, Box<ColExpr>, Box<ColExpr>),
}

/// The engine-facing DataFrame interface. One `impl` per backend; all engine
/// types stay inside that impl's module. Methods thread `line`/`col` so engine
/// errors surface at the Helix source position, matching the rest of the runtime.
pub trait DataHandle {
    /// For downcasting to a concrete backend — e.g. `join` needs both operands to
    /// be the same engine. Each impl returns `self`.
    fn as_any(&self) -> &dyn std::any::Any;

    fn column_names(&self, line: usize, col: usize) -> Result<Vec<String>, HelixError>;
    fn filter(&self, pred: &ColExpr, line: usize, col: usize) -> Result<Df, HelixError>;
    fn select(&self, names: &[String], line: usize, col: usize) -> Result<Df, HelixError>;
    fn with_columns(
        &self,
        cols: &[(String, ColExpr)],
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError>;
    fn sort(&self, names: &[String], line: usize, col: usize) -> Result<Df, HelixError>;
    fn join(
        &self,
        right: &Df,
        keys: &[String],
        how: &str,
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError>;
    fn head(&self, n: usize) -> Df;
    fn group_agg(
        &self,
        keys: &[String],
        agg: &str,
        value_col: &str,
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError>;
    fn row_count(&self, line: usize, col: usize) -> Result<usize, HelixError>;
    fn column_values(&self, name: &str, line: usize, col: usize) -> Result<Vec<Value>, HelixError>;
    fn cache(&self, line: usize, col: usize) -> Result<Df, HelixError>;
    fn write_parquet(&self, path: &str, line: usize, col: usize) -> Result<(), HelixError>;
    /// Render the (materialized) frame for `print`. `Err` carries the engine's
    /// message, which `Display` (which can't propagate errors) wraps into a
    /// placeholder.
    fn collect_string(&self) -> Result<String, String>;
}

/// Translate a Helix AST expression into the backend-agnostic [`ColExpr`],
/// resolving bare names against the frame's `columns` first, then Helix variables
/// (for predicates like `where(age > threshold)`). This is the verb→engine seam's
/// front half — it owns the friendly "no column or variable" diagnostic, so every
/// backend reports identical errors.
pub fn ast_to_colexpr(
    e: &Ast,
    columns: &[String],
    resolve_var: &dyn Fn(&str) -> Option<Value>,
) -> Result<ColExpr, HelixError> {
    match e {
        Ast::Int(i) => Ok(ColExpr::Lit(Value::Int(*i))),
        Ast::Float(f) => Ok(ColExpr::Lit(Value::Float(*f))),
        Ast::Str(s) => Ok(ColExpr::Lit(Value::Str(Rc::new(s.clone())))),
        Ast::Bool(b) => Ok(ColExpr::Lit(Value::Bool(*b))),
        Ast::Missing => Ok(ColExpr::Lit(Value::Missing)),
        Ast::Ident { name, line, col } => {
            if columns.iter().any(|c| c == name) {
                Ok(ColExpr::Col(name.clone()))
            } else if let Some(v) = resolve_var(name) {
                // A variable used in a query must be a scalar — reject e.g. an
                // Array up front, with the same message the engine would give.
                validate_scalar(&v, *line, *col)?;
                Ok(ColExpr::Lit(v))
            } else {
                Err(HelixError::new(
                    format!("no column or variable named `{}`", name),
                    *line,
                    *col,
                )
                .hint(format!("available columns: {}", columns.join(", "))))
            }
        }
        Ast::Unary { op, expr, .. } => {
            let inner = ast_to_colexpr(expr, columns, resolve_var)?;
            Ok(ColExpr::Unary(op.clone(), Box::new(inner)))
        }
        Ast::Binary {
            op, left, right, ..
        } => {
            let l = ast_to_colexpr(left, columns, resolve_var)?;
            let r = ast_to_colexpr(right, columns, resolve_var)?;
            Ok(ColExpr::Binary(op.clone(), Box::new(l), Box::new(r)))
        }
        _ => Err(HelixError::new(
            "this expression isn't supported inside a DataFrame query yet",
            0,
            0,
        )
        .hint("DataFrame queries support column names, literals, arithmetic, and comparisons.")),
    }
}

/// Reject a non-scalar value used as a literal inside a DataFrame query.
fn validate_scalar(v: &Value, line: usize, col: usize) -> Result<(), HelixError> {
    match v {
        Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_) | Value::Missing => Ok(()),
        other => Err(HelixError::new(
            format!(
                "cannot use a value of type {} inside a DataFrame query",
                other.type_name()
            ),
            line,
            col,
        )),
    }
}

/// Validate that every named column exists in `handle`'s schema (shared by every
/// backend), with the same friendly "no column …" diagnostic the `column` verb
/// gives — so `select`/`sort`/`group`/aggregations report a clean error eagerly
/// rather than leaking an engine's lazy-plan dump at collect time.
pub fn validate_columns_exist(
    handle: &Df,
    names: &[String],
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    let cols = handle.column_names(line, col)?;
    for n in names {
        if !cols.iter().any(|c| c == n) {
            return Err(
                HelixError::new(format!("no column `{}` in the DataFrame", n), line, col)
                    .hint(format!("columns: {}", cols.join(", "))),
            );
        }
    }
    Ok(())
}

/// Validate join keys against both frames' schemas (shared by every backend) so a
/// typo reads as a clean Helix error rather than an engine's lazy-plan dump.
pub fn validate_join_keys(
    left_cols: &[String],
    right_cols: &[String],
    keys: &[String],
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    for k in keys {
        if !left_cols.iter().any(|c| c == k) {
            return Err(
                HelixError::new(format!("no column `{}` in the left frame", k), line, col)
                    .hint(format!("left columns: {}", left_cols.join(", "))),
            );
        }
        if !right_cols.iter().any(|c| c == k) {
            return Err(
                HelixError::new(format!("no column `{}` in the right frame", k), line, col)
                    .hint(format!("right columns: {}", right_cols.join(", "))),
            );
        }
    }
    Ok(())
}

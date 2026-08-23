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

#[cfg(feature = "native-df")]
pub mod native;
pub mod timefmt;
#[cfg(feature = "dataframes")]
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
#[cfg_attr(not(feature = "dataframes"), allow(dead_code))]
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
    // Engine routing (ADR 0033): polars is the default while it remains the
    // oracle; the native engine serves builds without it, and a dual-engine dev
    // build can pick native explicitly (differential runs only).
    #[cfg(all(feature = "dataframes", feature = "native-df"))]
    {
        if native_selected() {
            return native::build_frame(columns, line, col);
        }
        polars::build_frame(columns, line, col)
    }
    #[cfg(all(feature = "dataframes", not(feature = "native-df")))]
    {
        polars::build_frame(columns, line, col)
    }
    #[cfg(all(not(feature = "dataframes"), feature = "native-df"))]
    {
        native::build_frame(columns, line, col)
    }
    #[cfg(all(not(feature = "dataframes"), not(feature = "native-df")))]
    {
        let _ = columns;
        Err(no_dataframes(line, col))
    }
}

/// Dual-engine dev builds only: `HELIX_DF_ENGINE=native` opts a run into the
/// native engine so the differential harness can drive both from one binary.
/// Single-engine builds never consult the environment (reproducibility).
#[cfg(all(feature = "dataframes", feature = "native-df"))]
pub(crate) fn native_selected() -> bool {
    std::env::var("HELIX_DF_ENGINE").map(|v| v == "native").unwrap_or(false)
}

/// The error every DataFrame constructor answers with in a build without the
/// engine — same shape as the http feature's: name the capability, say how to
/// get it. The verbs stay in the registry/checker/describe in every build.
#[cfg(all(not(feature = "dataframes"), not(feature = "native-df")))]
pub fn no_dataframes(line: usize, col: usize) -> HelixError {
    HelixError::new("this build has no DataFrame support", line, col)
        .hint("build without `--no-default-features`, or with `--features dataframes`.")
}

/// A column expression in a DataFrame query (`age > 40`, `weight / height`),
/// already **resolved**: bare names that match a column are `Col`; other names
/// were resolved against Helix variables to a `Lit`. Backend-agnostic — each
/// backend lowers it to its own expression type (a Polars `Expr`, or a fused
/// compute kernel).
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "dataframes"), allow(dead_code))]
pub enum ColExpr {
    Col(String),
    Lit(Value),
    Unary(UnOp, Box<ColExpr>),
    Binary(BinOp, Box<ColExpr>, Box<ColExpr>),
    /// `expr.is_missing()` — the EXPLICIT null test, and the only sanctioned way to
    /// select missing rows. `@v == missing` is not it and never will be: under ADR 0001
    /// `missing == missing` is `missing`, so that predicate selects nothing — on arrays
    /// and in queries alike, deliberately kept in agreement.
    IsMissing(Box<ColExpr>),
}

/// The engine-facing DataFrame interface. One `impl` per backend; all engine
/// types stay inside that impl's module. Methods thread `line`/`col` so engine
/// errors surface at the Helix source position, matching the rest of the runtime.
pub trait DataHandle {
    /// For downcasting to a concrete backend — e.g. `join` needs both operands to
    /// be the same engine. Each impl returns `self`.
    #[cfg_attr(not(feature = "dataframes"), allow(dead_code))]
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
    /// Vertically concatenate `bottom`'s rows under `self` (row append). Both frames
    /// must have the same columns (names and order); a schema mismatch is a clean
    /// error, never a silent null-fill.
    fn vstack(&self, bottom: &Df, line: usize, col: usize) -> Result<Df, HelixError>;
    /// Drop duplicate rows. With an empty `subset`, distinct *whole* rows keeping
    /// the first occurrence; with key columns, one row per key combination keeping
    /// the LAST occurrence (upsert — a re-appended key supersedes the older row).
    /// Stable, so the result is deterministic and the two engines never diverge.
    fn unique_by(&self, subset: &[String], line: usize, col: usize) -> Result<Df, HelixError>;
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
    /// Write the frame as delimited text (CSV when `sep` is `b','`, TSV for `b'\t'`).
    fn write_csv(&self, path: &str, sep: u8, line: usize, col: usize) -> Result<(), HelixError>;
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
        // `@name` is *always* a column — never falls back to a variable, so a column
        // and a same-named local can never be confused (the point of the sigil).
        Ast::Column { name, line, col } => {
            if columns.iter().any(|c| c == name) {
                Ok(ColExpr::Col(name.clone()))
            } else {
                Err(HelixError::new(
                    format!("no column named `{}`", name),
                    *line,
                    *col,
                )
                .hint(format!("available columns: {}", columns.join(", "))))
            }
        }
        // A BINDING IN SCOPE WINS OVER A COLUMN OF THE SAME NAME (ADR 0028). It used to be
        // the other way round, which made a library's parameter names reserved words in data
        // it has never seen:
        //
        //     fn above(frame, cutoff) = frame.where(@value > cutoff).count()
        //
        // returned 2 on columns {value, other} and 3 on {value, cutoff} — `cutoff` bound to
        // the caller's COLUMN, so the predicate became column-vs-column. Same function, same
        // argument, different answer, exit 0, and all three engines agree because all three
        // are equally wrong.
        //
        // The hazard does not disappear, it MOVES — and that is the whole argument. A query
        // author whose local shadows a column can see both names in one scope; a library
        // author cannot see the caller's schema at all. Trading an invisible, undefendable
        // capture for a local, visible one is the trade. `@name` still pins the column side
        // explicitly, which is what an author who writes `@value > cutoff` already means.
        Ast::Ident { name, line, col } => {
            if let Some(v) = resolve_var(name) {
                // A variable used in a query must be a scalar — reject e.g. an
                // Array up front, with the same message the engine would give.
                validate_scalar(&v, *line, *col)?;
                Ok(ColExpr::Lit(v))
            } else if columns.iter().any(|c| c == name) {
                Ok(ColExpr::Col(name.clone()))
            } else {
                Err(HelixError::new(
                    format!("no column or variable named `{}`", name),
                    *line,
                    *col,
                )
                .hint(format!("available columns: {}", columns.join(", "))))
            }
        }
        // `expr.is_missing()` — the universal method, admitted inside queries so intent
        // about missing data is EXPRESSIBLE without bending `==`: `where(@v == missing)`
        // silently selects nothing (correctly — `missing == missing` is `missing`, and
        // queries agree with arrays on that), which left no honest spelling at all. Now
        // `where(@v.is_missing())` and `where(not @v.is_missing())` are the spellings,
        // matching what `[..].map(it.is_missing())` already means on arrays.
        Ast::Method { recv, name, args, .. } if name == "is_missing" && args.is_empty() => {
            let inner = ast_to_colexpr(recv, columns, resolve_var)?;
            Ok(ColExpr::IsMissing(Box::new(inner)))
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

/// Validate that a `where`/`filter` predicate is a CONDITION, before any backend sees it.
///
/// THIS EXISTS BECAUSE `read_csv(f).where(1)` ABORTED THE PROCESS. Not errored — aborted:
/// exit 134, `Aborted (core dumped)`, on all three engines, uncatchable by `try` (the build
/// is `panic = "abort"`, so `catch_unwind` is a no-op), after `helix check` printed `ok`. It
/// also spilled an absolute cargo-registry path and a `polars-stream-0.54.4` source line into
/// the user's terminal. That is a direct falsification of ADR-0024's never-abort promise,
/// reachable from a one-character typo on the flagship data path.
///
/// WHY NOTHING CAUGHT IT. Two blind spots lined up. The unwrap-budget ratchet counts
/// panicking calls under `src/` only, so a panic inside a dependency is invisible to it. And
/// every DataFrame fixture in `tests/corpus/` builds its frame with `dataframe({…})` — the
/// EAGER half of the dispatch, which returns a clean error for the very same predicate. The
/// abort lives only on the lazy CSV-scan path, where Polars pushes the predicate into a scan
/// node that panics on a non-boolean. A regression test written the way all five existing
/// fixtures are written passes while the bug remains, so the fixture must call `read_csv`.
///
/// WHY HERE AND NOT IN `backend/polars.rs`. ADR-0012 puts Helix's totality at the seam, and
/// this check needs nothing engine-specific: a filter predicate that is provably not a
/// condition is wrong for every backend, so validating it beside [`validate_columns_exist`]
/// means no future backend can forget. It also has the side effect of making the two
/// dispatch halves agree — the eager path's message was Polars' own
/// ``filter predicate must be of type `Boolean`, got `i32` ``, which names a dtype no Helix
/// program can spell.
///
/// WHAT IT PROVES, AND WHAT IT DOES NOT. `Col` is deliberately UNKNOWN: `df.where(@flag)` on
/// a boolean column is legitimate, and a bare column's type needs the schema. Everything a
/// bare column reaches is therefore left to the backend, which errors cleanly on it today
/// (verified: `where(@a)` on an Int column exits 1, `where(@a + 1)` exits 1). What is
/// rejected here is the family that has no schema question at all — a literal, arithmetic, a
/// negation of one — which is exactly the family that aborted: `1`, `0`, `-1`, `1 + 1`,
/// `not 1`, `1 and true`.
pub fn validate_predicate(pred: &ColExpr, line: usize, col: usize) -> Result<(), HelixError> {
    /// `Some(true)` = provably a condition, `Some(false)` = provably not one, `None` =
    /// depends on the frame's schema, so it is the backend's business.
    fn shape(e: &ColExpr) -> Option<bool> {
        match e {
            // A bare column may be a Boolean column; only the schema knows.
            ColExpr::Col(_) => None,
            ColExpr::Lit(Value::Bool(_)) => Some(true),
            ColExpr::Lit(_) => Some(false),
            // Comparisons always yield a condition, whatever their operands are.
            ColExpr::Binary(
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge,
                ..
            ) => Some(true),
            // `and`/`or` are conditions only if both sides are. `1 and true` is the shape
            // that aborted, and it is caught here rather than by the literal arm.
            ColExpr::Binary(BinOp::And | BinOp::Or, l, r) => match (shape(l), shape(r)) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            },
            // Arithmetic, bitwise and the rest are values, never conditions.
            ColExpr::Binary(..) => Some(false),
            // `not` preserves its operand's verdict: `not 1` is as wrong as `1`.
            ColExpr::Unary(UnOp::Not, inner) => shape(inner),
            ColExpr::Unary(..) => Some(false),
            // A null test is a condition whatever its operand holds.
            ColExpr::IsMissing(_) => Some(true),
        }
    }
    if shape(pred) != Some(false) {
        return Ok(());
    }
    // Name what the user actually wrote, in Helix's vocabulary — never the engine's.
    // `a value of type X` rather than `a X` on purpose: it matches the phrasing already used
    // elsewhere ("a value of type Missing cannot be indexed") and sidesteps the article bug
    // that "a Int" would be.
    let what = match pred {
        ColExpr::Lit(v) => format!("a value of type {}", v.type_name()),
        ColExpr::Unary(UnOp::Not, _) => "`not` applied to a value, not to a condition".to_string(),
        ColExpr::Unary(..) => "an arithmetic expression".to_string(),
        ColExpr::Binary(BinOp::And | BinOp::Or, ..) => {
            "an `and`/`or` over something that is not a condition".to_string()
        }
        ColExpr::Binary(..) => "an arithmetic expression".to_string(),
        ColExpr::Col(_) => unreachable!("a bare column is never provably non-boolean"),
        ColExpr::IsMissing(_) => unreachable!("a null test is always a condition"),
    };
    Err(
        HelixError::new(format!("a filter predicate must be a condition, but this is {what}"), line, col)
            .hint("compare a column, e.g. `.where(@age > 40)`, or name a boolean column, e.g. `.where(@is_adult)`."),
    )
}

/// Validate join keys against both frames' schemas (shared by every backend) so a
/// typo reads as a clean Helix error rather than an engine's lazy-plan dump.
#[cfg_attr(not(feature = "dataframes"), allow(dead_code))]
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

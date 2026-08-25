//! The **Polars backend** — the default `DataHandle`. ALL `polars::` types are
//! confined to this file (ADR 0012): a Polars API break, or swapping engines,
//! touches only here. Operations are lazy (they extend the `LazyFrame` query
//! plan); Polars fuses the whole plan and executes it multi-threaded — and, with
//! the streaming engine, out-of-core — at the single `collect()` materialization
//! point (`row_count`/`column_values`/`write_parquet`).
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

/// Marks an error Helix itself raised from inside a polars UDF, so it comes back
/// out as the language's own sentence rather than an engine one. `\u{1}` cannot
/// occur in Helix source or in any polars message, so the framing is unambiguous.
const HELIX_UDF_ERR: &str = "\u{1}helix\u{1}";

/// Build the sentinel-wrapped error a guard UDF returns: message, hint (possibly
/// empty) and the 0-based row, joined by the same separator. `usize::MAX` means
/// "no row applies" (a whole-column refusal, like a non-numeric operand).
fn udf_error(msg: &str, hint: &str, row: usize) -> PolarsError {
    let row = if row == usize::MAX { String::new() } else { row.to_string() };
    PolarsError::ComputeError(format!("{HELIX_UDF_ERR}{msg}\u{1}{hint}\u{1}{row}").into())
}

/// Map a Polars error into a friendly Helix error at a source position.
fn pl<T>(r: PolarsResult<T>, ctx: &str, line: usize, col: usize) -> Result<T, HelixError> {
    r.map_err(|e| {
        let raw = e.to_string();
        // A Helix error raised inside a guard UDF passes straight through. It is
        // already the language's own message, so prefixing it with an engine context
        // ("could not read DataFrame schema: division by zero") would be both wrong
        // and confusing: `tidy()` exists to translate POLARS' words, not ours.
        if let Some(rest) = raw.strip_prefix(HELIX_UDF_ERR) {
            let mut parts = rest.split('\u{1}');
            let msg = parts.next().unwrap_or("").to_string();
            let hint = parts.next().unwrap_or("").to_string();
            let row = parts.next().unwrap_or("");
            let err = HelixError::new(msg, line, col);
            // The row ADDS to the scalar advice rather than replacing it, matching
            // `backend::native::eval::at_row`. Both engines print the same help for
            // the same error, and the help survives into the frame case — which is
            // where the compare error's "guard it first with `is_nan(x)`" is most
            // needed and least guessable.
            let hint = match (hint.is_empty(), row.is_empty()) {
                (true, true) => String::new(),
                (true, false) => format!("at row {row} of the frame."),
                (false, true) => hint,
                (false, false) => format!("{hint} (at row {row} of the frame.)"),
            };
            return if hint.is_empty() { err } else { err.hint(hint) };
        }
        let (msg, hint) = tidy(&raw);
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

/// A polars dtype named the way Helix names types, for an error a user will read.
fn dtype_type_name(dt: &DataType) -> &'static str {
    match dt {
        DataType::Boolean => "Bool",
        DataType::String => "String",
        d if d.is_integer() => "Int",
        d if d.is_float() => "Float",
        _ => "value",
    }
}

/// The scalar kernel's own division/modulo-by-zero errors, reproduced exactly.
/// Byte-identical to `interp::ops::eval_binary` on purpose: one semantics means one
/// message, and a frame that said it differently would be a divergence of its own.
fn zero_divisor_error(op: &BinOp, line: usize, col: usize) -> HelixError {
    match op {
        BinOp::Div => HelixError::new("division by zero", line, col)
            .hint("guard the denominator, e.g. `if d != 0` or check your data."),
        BinOp::FloorDiv => HelixError::new("integer division by zero", line, col)
            .hint("guard the divisor, e.g. `if d != 0`."),
        _ => HelixError::new("modulo by zero", line, col),
    }
}

/// `/`, `%` and `//` with the LANGUAGE's exact semantics, as an elementwise UDF.
///
/// Two independent reasons this cannot be `l / r`:
///
/// 1. **Polars is not IEEE-faithful for a scalar divisor.** `@b / 10` over
///    `[41, 38, 55, 29]` answered `[4.1000000000000005, 3.8000000000000003, 5.5,
///    2.9000000000000004]`: it rewrites division-by-a-constant into multiplication
///    by the reciprocal, and `41.0 * 0.1` is not `41.0 / 10.0`. It triggers only at
///    two rows or more, so a one-row test — the kind anyone writes — shows
///    agreement. Every division by a constant in every frame query was silently one
///    ULP away from the same expression on scalars.
/// 2. **Division by zero must be an error naming the row** (ADR 0036 policy 1),
///    where polars gives three different silent answers: `missing` for Int `/0`,
///    `inf` for Float `/0`, `NaN` for `0.0 / 0.0`.
///
/// The arithmetic is `interp::ops::eval_binary`'s, transcribed — including
/// `wrapping_rem_euclid`/`wrapping_div_euclid`, which exist so `i64::MIN % -1` and
/// `i64::MIN // -1` wrap rather than abort the process.
///
/// ROW NUMBERS ARE GLOBAL because polars invokes an elementwise UDF **once per
/// column**, not once per morsel — measured at 4, 100k and 1M rows and pinned by
/// `udf_invocation_shape`, so a future polars that starts chunking is caught by a
/// test rather than by a user reading a wrong row number.
/// `%` and `//` keep Int when BOTH operands are integer columns, exactly as the
/// scalar kernel does; `/` is true division and is always Float. Decided from the
/// REAL dtypes rather than guessed from the expression, so a column counts.
fn out_is_int(op: BinOp, a: &DataType, b: &DataType) -> bool {
    matches!(op, BinOp::Mod | BinOp::FloorDiv) && a.is_integer() && b.is_integer()
}

fn guarded_arith(l: Expr, r: Expr, op: BinOp) -> Expr {
    l.map_many(
        move |cols: &mut [Column]| -> PolarsResult<Column> {
            // A LITERAL operand arrives as a length-1 column (polars' scalar
            // broadcast), so zipping it against a full column would silently produce
            // a one-row result. Broadcast it to the common length first.
            let n = cols[0].len().max(cols[1].len());
            let bc = |c: &Column| -> Column {
                if c.len() == n { c.clone() } else { c.new_from_index(0, n) }
            };
            let (a, b) = (bc(&cols[0]), bc(&cols[1]));
            let (a, b) = (&a, &b);
            let name = cols[0].name().clone();
            let out_int = out_is_int(op, a.dtype(), b.dtype());
            // A non-numeric operand is the scalar kernel's error, not a polars cast
            // failure — and never the silent nulling `Expr::cast` would give.
            for c in [a, b] {
                if !c.dtype().is_primitive_numeric() {
                    return Err(udf_error(
                        &format!(
                            "operator `{}` needs numbers, but got {}",
                            op.symbol(),
                            crate::value::with_article(dtype_type_name(c.dtype()))
                        ),
                        "",
                        usize::MAX,
                    ));
                }
            }
            // The message depends on the OPERAND TYPES, exactly as the scalar kernel's
            // does: `//` says "integer division by zero" only when both sides are Int,
            // and "division by zero" on the float path. Getting this wrong is how the
            // guard briefly introduced a divergence of its own.
            let zero = |row: usize| -> PolarsError {
                match op {
                    BinOp::Div => udf_error(
                        "division by zero",
                        "guard the denominator, e.g. `if d != 0` or check your data.",
                        row,
                    ),
                    BinOp::FloorDiv if out_int => udf_error(
                        "integer division by zero",
                        "guard the divisor, e.g. `if d != 0`.",
                        row,
                    ),
                    BinOp::FloorDiv => udf_error("division by zero", "", row),
                    _ => udf_error("modulo by zero", "", row),
                }
            };
            if out_int {
                let (ai, bi) = (a.cast(&DataType::Int64)?, b.cast(&DataType::Int64)?);
                let (ai, bi) = (ai.i64()?, bi.i64()?);
                let mut out: Vec<Option<i64>> = Vec::with_capacity(ai.len());
                for (i, (x, y)) in ai.iter().zip(bi.iter()).enumerate() {
                    match (x, y) {
                        (Some(x), Some(y)) => {
                            if y == 0 {
                                return Err(zero(i));
                            }
                            out.push(Some(if matches!(op, BinOp::Mod) {
                                x.wrapping_rem_euclid(y)
                            } else {
                                x.wrapping_div_euclid(y)
                            }));
                        }
                        // Missing propagates elementwise (ADR 0001) — and a MISSING
                        // divisor is not a zero divisor, so it must not raise.
                        _ => out.push(None),
                    }
                }
                Ok(Column::new(name, out))
            } else {
                let (af, bf) = (a.cast(&DataType::Float64)?, b.cast(&DataType::Float64)?);
                let (af, bf) = (af.f64()?, bf.f64()?);
                let mut out: Vec<Option<f64>> = Vec::with_capacity(af.len());
                for (i, (x, y)) in af.iter().zip(bf.iter()).enumerate() {
                    match (x, y) {
                        (Some(x), Some(y)) => {
                            // `y == 0.0` is true for -0.0 and false for NaN, which is
                            // exactly what the scalar kernel's `if b == 0.0` does.
                            if y == 0.0 {
                                return Err(zero(i));
                            }
                            out.push(Some(match op {
                                BinOp::Div => x / y,
                                BinOp::Mod => x.rem_euclid(y),
                                _ => x.div_euclid(y),
                            }));
                        }
                        _ => out.push(None),
                    }
                }
                Ok(Column::new(name, out))
            }
        },
        &[r],
        move |_schema: &Schema, fields: &[Field]| {
            let dt = if out_is_int(op, fields[0].dtype(), fields[1].dtype()) {
                DataType::Int64
            } else {
                DataType::Float64
            };
            Ok(Field::new(fields[0].name().clone(), dt))
        },
    )
}

/// Does evaluating this predicate risk raising on a row a previous filter removed?
///
/// True exactly when an ordering comparison has a float operand — the only predicate
/// shape that raises per-row (ADR 0036 policy 5). Everything else composes freely and
/// keeps polars' fusion.
fn predicate_can_raise(e: &ColExpr, fields: &[(String, DataType)]) -> bool {
    match e {
        ColExpr::Binary(BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge, l, r) => {
            may_be_float(l, fields) || may_be_float(r, fields)
        }
        ColExpr::Binary(_, l, r) => {
            predicate_can_raise(l, fields) || predicate_can_raise(r, fields)
        }
        ColExpr::Unary(_, inner) => predicate_can_raise(inner, fields),
        _ => false,
    }
}

/// Could this expression yield a Float, and therefore possibly a NaN?
///
/// Conservative: an unknown column answers `true`, because guarding something that
/// cannot be a NaN is merely slower, while failing to guard one that can is the wrong
/// answer. Int, String, Bool and Date columns answer `false` and keep polars' own
/// comparison.
fn may_be_float(e: &ColExpr, fields: &[(String, DataType)]) -> bool {
    match e {
        ColExpr::Lit(Value::Float(_)) => true,
        ColExpr::Lit(_) => false,
        ColExpr::Col(name) => fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| d.is_float())
            .unwrap_or(true),
        // Any arithmetic touching a float is a float — and `/` always is.
        ColExpr::Binary(BinOp::Div, ..) => true,
        ColExpr::Binary(_, l, r) => may_be_float(l, fields) || may_be_float(r, fields),
        ColExpr::Unary(_, inner) => may_be_float(inner, fields),
        // A predicate answers Bool.
        ColExpr::IsMissing(_) | ColExpr::FloatPred(..) => false,
    }
}

/// `< > <= >=` with the language's NaN rule: an unordered comparison RAISES.
///
/// IEEE-754 defines signalling comparison predicates that raise on unordered
/// operands, so this is a legitimate 754 option rather than an invention, and it is
/// what the scalar kernel has always done (`interp::ops::compare`). The polars
/// backend answered `true` for `NaN > 2.0` and silently KEPT the row: a wrong
/// dataset, not a wrong format, on the default backend, with exit 0.
///
/// Integer columns cannot hold a NaN, so they take a plain comparison inside the
/// closure and pay only the UDF's per-column overhead — measured, not assumed.
fn guarded_compare(l: Expr, r: Expr, op: BinOp) -> Expr {
    l.map_many(
        move |cols: &mut [Column]| -> PolarsResult<Column> {
            let n = cols[0].len().max(cols[1].len());
            let bc = |c: &Column| -> Column {
                if c.len() == n { c.clone() } else { c.new_from_index(0, n) }
            };
            let (a, b) = (bc(&cols[0]), bc(&cols[1]));
            let name = cols[0].name().clone();
            let cmp = |o: std::cmp::Ordering| -> bool {
                use std::cmp::Ordering::*;
                match op {
                    BinOp::Lt => o == Less,
                    BinOp::Gt => o == Greater,
                    BinOp::Le => o != Greater,
                    _ => o != Less,
                }
            };
            // `may_be_float` gates installation, so this only runs where a NaN is
            // possible. Non-float columns never reach here and keep polars' own
            // comparison — which also keeps String ordering lexical, as the scalar
            // kernel has it, instead of casting it to a column of nulls.
            let (af, bf) = (a.cast(&DataType::Float64)?, b.cast(&DataType::Float64)?);
            let (af, bf) = (af.f64()?, bf.f64()?);
            let mut out: Vec<Option<bool>> = Vec::with_capacity(n);
            for (i, (x, y)) in af.iter().zip(bf.iter()).enumerate() {
                match (x, y) {
                    (Some(x), Some(y)) => match x.partial_cmp(&y) {
                        Some(o) => out.push(Some(cmp(o))),
                        // Unordered: the language refuses rather than guessing, and
                        // names the escape hatch that C9 made available on a column.
                        None => {
                            // Message and advice from the kernel's own constructor,
                            // so the three engines cannot phrase one error three ways.
                            let e = crate::interp::ops::nan_compare_error(0, 0);
                            return Err(udf_error(
                                &e.message,
                                e.hint.as_deref().unwrap_or(""),
                                i,
                            ));
                        }
                    },
                    // `missing` propagates through a comparison (ADR 0001); it is not
                    // unordered, it is absent.
                    _ => out.push(None),
                }
            }
            Ok(Column::new(name, out))
        },
        &[r],
        move |_schema: &Schema, fields: &[Field]| {
            Ok(Field::new(fields[0].name().clone(), DataType::Boolean))
        },
    )
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
/// Lower a Helix column expression to a polars expression.
///
/// `line`/`col` are the SOURCE position of the verb this expression came from.
/// They used to be absent, and every error raised in here was reported at `0, 0` —
/// pointing at nothing — while both callers had the real position sitting unused in
/// `_line`/`_col`. Threading them is what lets a refusal inside a query point at the
/// query.
fn lower(e: &ColExpr, fields: &[(String, DataType)], line: usize, col: usize) -> Result<Expr, HelixError> {
    Ok(match e {
        ColExpr::Col(name) => pcol(name.as_str()),
        ColExpr::Lit(v) => value_to_lit(v, line, col)?,
        ColExpr::Unary(op, inner) => {
            let i = lower(inner, fields, line, col)?;
            match op {
                UnOp::Neg => lit(0) - i,
                UnOp::Not => i.not(),
            }
        }
        // Arrow's validity bitmap IS Helix's `missing`, so the null test lowers exactly.
        ColExpr::IsMissing(inner) => lower(inner, fields, line, col)?.is_null(),
        // A classification, not a comparison — so no NaN guard is needed, and none
        // must be added: `is_nan(@v)` has to stay answerable ON a NaN.
        //
        // DTYPE-GATED, for the fourth time in this release: polars' `is_nan` is
        // undefined for `str` and RAISES, so asking it of a String column turned
        // `df.drop_nan()` into an error on the default backend. A column that cannot
        // hold a NaN answers the question statically — `false` for `is_nan`, `true`
        // for `is_finite` — which is exactly what the native engine answers for a
        // non-numeric cell, so the two agree by construction rather than by luck.
        ColExpr::FloatPred(kind, inner) => {
            if !may_be_float(inner, fields) {
                return Ok(match kind {
                    super::FloatPredKind::IsNan => lit(false),
                    super::FloatPredKind::IsFinite => lit(true),
                });
            }
            let e = lower(inner, fields, line, col)?;
            match kind {
                super::FloatPredKind::IsNan => e.is_nan(),
                super::FloatPredKind::IsFinite => e.is_finite(),
            }
        }
        ColExpr::Binary(op, l, r) => {
            // A LITERAL zero divisor is decidable without touching a row, so it is
            // refused where it was written, with no `at row` hint — the same shape
            // the scalar kernel gives (ADR 0036 policy 1).
            // NOT `//`: its message depends on whether BOTH operands are Int
            // ("integer division by zero" vs "division by zero"), and the left one is
            // a column whose dtype is not known here. `/` and `%` say the same thing
            // either way, so they can still be refused at the source position.
            if matches!(op, BinOp::Div | BinOp::Mod)
                && matches!(
                    &**r,
                    ColExpr::Lit(Value::Int(0)) | ColExpr::Lit(Value::Float(0.0))
                )
            {
                return Err(zero_divisor_error(op, line, col));
            }
            let float_operands = may_be_float(l, fields) || may_be_float(r, fields);
            let l = lower(l, fields, line, col)?;
            let r = lower(r, fields, line, col)?;
            match op {
                BinOp::Add => l + r,
                BinOp::Sub => l - r,
                BinOp::Mul => l * r,
                BinOp::Div => guarded_arith(l, r, BinOp::Div),
                BinOp::Mod => guarded_arith(l, r, BinOp::Mod),
                BinOp::FloorDiv => guarded_arith(l, r, BinOp::FloorDiv),
                BinOp::Pow => l.pow(r),
                // `==`/`!=` are IEEE at every depth: a NaN is equal to nothing,
                // INCLUDING itself. Polars reported NaN as self-equal in an
                // expression, so `where(@v == @v)` kept the NaN row where arrays and
                // the native engine dropped it. Pure expressions — no UDF, no guard
                // cost — and `is_nan` answers `false` for a non-float column, so this
                // is safe for every dtype.
                BinOp::Eq if float_operands => {
                    l.clone().eq(r.clone()).and(l.is_nan().not()).and(r.is_nan().not())
                }
                BinOp::Ne if float_operands => {
                    l.clone().neq(r.clone()).or(l.is_nan()).or(r.is_nan())
                }
                // No float operand means no NaN is possible, so plain equality is
                // already IEEE-correct — and the rewrite must NOT be applied, because
                // polars' `is_nan` is not defined for `str` and raises. (Probing it on
                // an Int column said "works"; String is where it does not, and the
                // gate is what the bio examples found.)
                BinOp::Eq => l.eq(r),
                BinOp::Ne => l.neq(r),
                // Ordering RAISES on an unordered pair (ADR 0036 policy 5) — but the
                // guard is installed only where a NaN is possible. An Int, String or
                // Bool comparison keeps polars' own operator and pays nothing.
                BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                    if float_operands {
                        guarded_compare(l, r, *op)
                    } else {
                        match op {
                            BinOp::Lt => l.lt(r),
                            BinOp::Gt => l.gt(r),
                            BinOp::Le => l.lt_eq(r),
                            _ => l.gt_eq(r),
                        }
                    }
                }
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
                        line,
                        col,
                    )
                    .hint("compute bitwise expressions on arrays or scalars, then build the DataFrame."));
                }
            }
        }
    })
}

/// Column names AND dtypes from a plan's schema — cheap (header/metadata only, no
/// scan). This is what lets the NaN comparison guard be installed ONLY where a NaN is
/// possible: an Int, String or Bool column cannot hold one, so it keeps polars' own
/// comparison and pays nothing.
fn schema_fields(
    lf: &LazyFrame,
    line: usize,
    col: usize,
) -> Result<Vec<(String, DataType)>, HelixError> {
    let mut lf = lf.clone();
    let schema = pl(lf.collect_schema(), "could not read DataFrame schema", line, col)?;
    Ok(schema.iter().map(|(n, d)| (n.to_string(), d.clone())).collect())
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
/// The tz-aware instant as UTC text + ` UTC` — see the match arms above for
/// why polars cannot render this itself in our build.
fn utc_instant_str(v: i64, unit: &polars::prelude::TimeUnit) -> String {
    use polars::prelude::TimeUnit as TU;
    let (per_sec, width) = match unit {
        TU::Milliseconds => (1_000, 3),
        TU::Microseconds => (1_000_000, 6),
        TU::Nanoseconds => (1_000_000_000, 9),
    };
    let mut s = crate::backend::timefmt::timestamp_str(v, per_sec, width);
    s.push_str(" UTC");
    s
}

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
        // A tz-aware datetime is the one AnyValue whose Display PANICS in this
        // build (polars' `timezones` feature is off; its formatter demands the
        // tz database) — reproduced from a foreign parquet file with
        // isAdjustedToUTC timestamps, which is an ADR 0024 abort. The value IS
        // a UTC instant, so render it as UTC text ourselves, byte-identical to
        // the native engine's rendering of the same file.
        AnyValue::Datetime(v, unit, Some(_)) => {
            Value::Str(Rc::new(utc_instant_str(*v, unit)))
        }
        AnyValue::DatetimeOwned(v, unit, Some(_)) => {
            Value::Str(Rc::new(utc_instant_str(*v, unit)))
        }
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

    fn filter(&self, pred: &ColExpr, line: usize, col: usize) -> Result<Df, HelixError> {
        let fields = schema_fields(&self.lf, line, col)?;
        let e = lower(pred, &fields, line, col)?;
        // `.where(a).where(b)` is SEQUENTIAL: the frame you filtered is the frame you
        // filter again. Native is eager and gets this for free; polars fuses adjacent
        // filters into one conjunction and evaluates both over every row — which is
        // invisible until a predicate can RAISE, and then it is very visible:
        // `.where(not is_nan(@v)).where(@v > 2.0)` raised on the rows the first filter
        // had already removed, so the guard the compare error tells you to write did
        // not work on the default backend.
        //
        // A cache node in front of a raising predicate stops the fusion. Measured on
        // 3M rows: 1.00x on a limited query, 1.04x on a streaming write, 1.09x on a
        // full scan — so sequential semantics is affordable and does not need to be
        // traded away. Pinned by `a_guarded_filter_does_not_see_removed_rows`, because
        // this rests on polars' optimizer treating a cache node as a barrier, which is
        // behaviour rather than contract.
        let needs_barrier = predicate_can_raise(pred, &fields);
        let base = if needs_barrier { self.lf.clone().cache() } else { self.lf.clone() };
        Ok(self.derive(base.filter(e)))
    }

    fn select(&self, names: &[String], _line: usize, _col: usize) -> Result<Df, HelixError> {
        // Refuse a duplicate up front, in the native backend's words — polars'
        // own error recommends `.alias("new_name")`, an API Helix doesn't have.
        for (i, name) in names.iter().enumerate() {
            if names[..i].contains(name) {
                return Err(HelixError::new(
                    format!("duplicate column `{name}`"),
                    _line,
                    _col,
                ));
            }
        }
        let exprs: Vec<Expr> = names.iter().map(|n| pcol(n.as_str())).collect();
        Ok(self.derive(self.lf.clone().select(exprs)))
    }

    /// Add or replace columns from `name = expr` pairs (`df.with({bmi: weight /
    /// height})`). An expression aliased to an existing column name replaces it.
    fn with_columns(
        &self,
        cols: &[(String, ColExpr)],
        line: usize,
        col: usize,
    ) -> Result<Df, HelixError> {
        let fields = schema_fields(&self.lf, line, col)?;
        let mut exprs = Vec::with_capacity(cols.len());
        for (name, ce) in cols {
            exprs.push(lower(ce, &fields, line, col)?.alias(name.as_str()));
        }
        Ok(self.derive(self.lf.clone().with_columns(exprs)))
    }

    /// Sort by one or more columns — a **stable** sort, by contract.
    ///
    /// `SortMultipleOptions::default()` is `maintain_order: false`: an unstable sort,
    /// meaning tie order is *unspecified*. Attempts to catch it actually varying
    /// (300k rows, int and string keys, two reads of one frame in-process and across
    /// 12 runs) found it stable in practice on the pinned Polars — so this is NOT a
    /// measured bug fix, and no failing repro exists; see docs/v0.2.1-fix-plan.md.
    /// It is pinned anyway because the exposure is structural: every `.column()`
    /// re-executes the lazy plan, so if an unspecified tie order ever *did* vary
    /// (a Polars bump, a different machine), reading two columns out of one sorted
    /// frame would pair values from two different orderings — and ties are the common
    /// case (categories, chromosomes, group keys). `maintain_order: true` makes the
    /// stability ADR 0025 (ordering) assumes a guarantee instead of an accident.
    fn sort(&self, names: &[String], line: usize, col: usize) -> Result<Df, HelixError> {
        // A FLOAT column sorts by `ops::float_key`, not by polars' own float order.
        // Polars places every NaN last (right) but canonicalises `-0.0` and `0.0` to
        // equal (wrong — ADR 0025 orders them, and the native engine does). There is
        // no flag for that pair; a key expression is the only way to get both.
        //
        // Nulls stay null through the map, so `nulls_last: false` still puts
        // `missing` first — do NOT set a null flag here, there is no `nan_first` knob
        // and the two concerns are separate.
        let fields = schema_fields(&self.lf, line, col)?;
        let mut exprs: Vec<Expr> = Vec::with_capacity(names.len());
        for n in names {
            let is_float = fields
                .iter()
                .find(|(name, _)| name == n)
                .map(|(_, d)| d.is_float())
                .unwrap_or(false);
            if is_float {
                exprs.push(
                    pcol(n.as_str())
                        .cast(DataType::Float64)
                        .map(
                            |c: Column| -> PolarsResult<Column> {
                                let f = c.f64()?;
                                let keys: Vec<Option<u64>> = f
                                    .iter()
                                    .map(|v| v.map(crate::interp::ops::float_key))
                                    .collect();
                                Ok(Column::new(c.name().clone(), keys))
                            },
                            |_s: &Schema, fl: &Field| {
                                Ok(Field::new(fl.name().clone(), DataType::UInt64))
                            },
                        )
                        .alias(format!("__helix_sortkey_{n}")),
                );
            } else {
                exprs.push(pcol(n.as_str()));
            }
        }
        Ok(self.derive(self.lf.clone().sort_by_exprs(
            exprs,
            SortMultipleOptions::default().with_maintain_order(true),
        )))
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
        // Coalesce the key columns for *every* join type. Without this, a `full`
        // (outer) join leaves both `key` and `key_right` with nulls split across
        // them — a different, surprising shape from inner/left/right. Coalescing
        // gives one key column uniformly (standard SQL FULL-OUTER semantics).
        let mut jargs = JoinArgs::new(join_type)
            .with_suffix(Some("_right".into()))
            .with_coalesce(JoinCoalesce::CoalesceColumns);
        // The default (`MaintainOrderJoin::None`) makes join output order a per-
        // EXECUTION coin flip — and because `.column()` re-executes the lazy plan,
        // two column reads of one joined-then-grouped frame could pair keys from one
        // ordering with values from another (the sort-tearing class, realized: ~490
        // of 500 rows silently mispaired in the stabilization sweep's repro).
        // `LeftRight` pins reading order for every join type; the same ADR 0025
        // doctrine as `sort`'s maintain_order above.
        jargs.maintain_order = MaintainOrderJoin::LeftRight;
        let joined = self.lf.clone().join(rf.lf.clone(), on.clone(), on, jargs);
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
        let value_is_float = schema_fields(&self.lf, line, col)?
            .iter()
            .find(|(n, _)| n == value_col)
            .map(|(_, d)| d.is_float())
            .unwrap_or(false);
        let agg_expr = match agg {
            // `count` counts ROWS, `missing` included — matching `[1.0, 3.0, missing].count()`
            // and `df.column("v").count()`, which both answer 3. Polars' `count()` excludes
            // nulls, which is what made an all-`missing` group report 0; `len()` is the
            // row count and is the one that matches Helix.
            "count" => c.len(),
            // Every other grouped aggregation PROPAGATES `missing`, matching the array
            // and whole-column paths (`[1.0, 3.0, missing].sum()` is `missing`). Polars
            // skips nulls, which silently turned an unknown into a number — and made an
            // all-`missing` group indistinguishable from one that really sums to zero.
            //
            // The `when(...).then(NULL).otherwise(agg)` shape is also what makes the
            // float reductions DETERMINISTIC, and that is not a coincidence to be
            // rediscovered later: Polars only runs its partitioned (non-deterministic
            // merge order) group-by when every aggregation passes `can_pre_agg`, whose
            // `Ternary` arm rejects any branch that itself contains an aggregation.
            // So this expression cannot take the partitioned path. See ADR-notes in
            // `docs/v0.2.1-fix-plan.md`; the regression test asserts the determinism
            // directly rather than trusting that rule to survive a Polars bump.
            "mean" | "sum" | "min" | "max" | "std" => {
                let inner = match agg {
                    "mean" => c.clone().mean(),
                    "sum" => c.clone().sum(),
                    "min" => c.clone().min(),
                    "max" => c.clone().max(),
                    _ => c.clone().std(1),
                };
                // A NaN propagates as NaN (ADR 0036 policy 4). polars' `min`/`max`
                // SKIP it, which is the pandas `skipna` default ADR 0025:132 names as
                // a red line; its `sum`/`mean` already propagate through arithmetic.
                // One guard covers all five so the five cannot drift apart.
                //
                // Gated on a FLOAT column: `is_nan` is undefined for `str` in polars
                // and raises, which is how the equality rewrite broke two bio examples
                // in C10. An Int column cannot hold a NaN, so it needs nothing.
                let inner = if value_is_float {
                    when(c.clone().is_nan().any(true))
                        .then(lit(f64::NAN))
                        .otherwise(inner)
                } else {
                    inner
                };
                when(c.null_count().gt(lit(0u32)))
                    .then(lit(NULL))
                    .otherwise(inner)
            }
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
        Ok(self.derive(
            self.lf
                .clone()
                .group_by_stable(key_exprs)
                .agg([agg_expr.alias(value_col)]),
        ))
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

}

/// The two properties of polars' UDF machinery that `guarded_arith` is built on.
///
/// Neither is documented by polars, both were established by measurement, and both
/// would fail SILENTLY and wrongly if a future version changed them — the first
/// would turn a Helix error into engine noise, the second would make every reported
/// row number wrong without anything erroring. So they are pinned here rather than
/// trusted.
#[cfg(test)]
mod udf_contract {
    use super::*;

    /// A Helix error raised inside a UDF must survive polars' error handling
    /// verbatim, so `pl()` can hand back the language's own sentence instead of an
    /// engine one. Measured: polars adds NO wrapping at all — the string that comes
    /// out is the string the closure put in.
    #[test]
    fn a_helix_error_survives_a_udf_verbatim() {
        let df = df!["a" => [1i64, 2, 3]].unwrap();
        let raising = pcol("a").map_many(
            |_cols: &mut [Column]| -> PolarsResult<Column> {
                Err(udf_error("division by zero", "guard the denominator.", 7))
            },
            &[],
            |_schema: &Schema, fields: &[Field]| Ok(fields[0].clone()),
        );
        let err = match df.lazy().with_columns([raising.alias("q")]).collect() {
            Ok(_) => panic!("the UDF must have raised"),
            Err(e) => e.to_string(),
        };
        assert!(err.starts_with(HELIX_UDF_ERR), "the sentinel did not survive: {err:?}");

        // And the whole round trip, exactly as a user would receive it.
        let helix: Result<(), HelixError> = pl(Err(PolarsError::ComputeError(err.into())), "ctx", 3, 9);
        let helix = helix.unwrap_err();
        assert_eq!(helix.message, "division by zero", "engine context leaked into our message");
        // The row ADDS to the kernel's advice rather than replacing it — both halves
        // must survive. Dropping the advice is how the frame case ended up printing a
        // row number and nothing about how to fix the problem.
        let hint = helix.hint.as_deref().unwrap_or("");
        assert!(hint.contains("guard the denominator."), "the advice was dropped: {hint}");
        assert!(hint.contains("at row 7 of the frame."), "the row was dropped: {hint}");
        assert_eq!((helix.line, helix.col), (3, 9));
    }

    /// Polars invokes an ELEMENTWISE UDF once per column, not once per morsel.
    ///
    /// This is what makes the row number in `at row N of the frame.` a GLOBAL row,
    /// and deterministic. If a future polars starts chunking, the index the closure
    /// computes becomes chunk-local — every reported row silently wrong, and which
    /// chunk raises first no longer decided. There is no polars API to ask, so the
    /// only honest thing is to measure it and fail loudly when it changes.
    #[test]
    fn udf_invocation_shape() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        for n in [4usize, 100_000, 1_000_000] {
            let calls = Arc::new(AtomicUsize::new(0));
            let widest = Arc::new(AtomicUsize::new(0));
            let (c2, w2) = (calls.clone(), widest.clone());
            let vals: Vec<i64> = (0..n as i64).collect();
            let df = df!["a" => vals].unwrap();
            let counted = pcol("a").map_many(
                move |cols: &mut [Column]| -> PolarsResult<Column> {
                    c2.fetch_add(1, Ordering::SeqCst);
                    w2.fetch_max(cols[0].len(), Ordering::SeqCst);
                    Ok(cols[0].clone())
                },
                &[],
                |_schema: &Schema, fields: &[Field]| Ok(fields[0].clone()),
            );
            let out = df.lazy().with_columns([counted.alias("q")]).collect().unwrap();
            assert_eq!(out.height(), n);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "polars now calls an elementwise UDF more than once at n={n} — \
                 `guarded_arith`'s row numbers are chunk-local and WRONG until it \
                 threads a row-index column (see ADR 0036 policy 1)"
            );
            assert_eq!(
                widest.load(Ordering::SeqCst),
                n,
                "the UDF saw a partial column at n={n} — row numbers are no longer global"
            );
        }
    }
}

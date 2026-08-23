//! Builtins: statistics, regression, and model-quality scores — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

pub(super) fn call(name: &str, args: Vec<Value>, line: usize, col: usize) -> Called {
    if !matches!(name, "correlation" | "t_test" | "linear_regression" | "multiple_regression" | "least_squares" | "mse" | "rmse" | "mae" | "r2_score" | "aic" | "bic" | "accuracy" | "precision" | "recall" | "f1_score" | "confusion_matrix") {
        return Called::Not(args);
    }
    Called::Done(dispatch(name, args, line, col))
}

fn dispatch(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
                "correlation" => {
                    arity(name, &args, 2, line, col)?;
                    // `missing` in either series propagates (ADR-0001); a non-array, a
                    // length mismatch, or a non-numeric element is a clean error.
                    let xs = num_array(name, &args[0], line, col)?;
                    let ys = num_array(name, &args[1], line, col)?;
                    let (xs, ys) = match (xs, ys) {
                        (Some(xs), Some(ys)) => (xs, ys),
                        _ => return Ok(Value::Missing),
                    };
                    if xs.len() != ys.len() {
                        return Err(HelixError::new(
                            format!(
                                "`correlation` needs two equal-length arrays, got {} and {}",
                                xs.len(),
                                ys.len()
                            ),
                            line,
                            col,
                        ));
                    }
                    if xs.is_empty() {
                        return Err(HelixError::new(
                            "cannot compute `correlation` of empty arrays",
                            line,
                            col,
                        ));
                    }
                    match crate::stats::pearson(&xs, &ys) {
                        Some(r) => Ok(Value::Float(r)),
                        None => Err(HelixError::new(
                            "correlation is undefined: one of the series has zero variance",
                            line,
                            col,
                        )
                        .hint("a constant series has no spread to correlate.")),
                    }
                }
                "t_test" => {
                    arity(name, &args, 2, line, col)?;
                    // Welch's two-sample t-test → {statistic, df, p_value}. `missing` in
                    // either sample propagates; each needs at least two values.
                    let xs = num_array(name, &args[0], line, col)?;
                    let ys = num_array(name, &args[1], line, col)?;
                    let (xs, ys) = match (xs, ys) {
                        (Some(xs), Some(ys)) => (xs, ys),
                        _ => return Ok(Value::Missing),
                    };
                    match crate::stats::welch_t_test(&xs, &ys) {
                        Some((t, df, p)) => {
                            let fields = vec![
                                (Symbol::intern("statistic"), Value::Float(t)),
                                (Symbol::intern("df"), Value::Float(df)),
                                (Symbol::intern("p_value"), Value::Float(p)),
                            ];
                            Ok(Value::Record(Rc::new(fields)))
                        }
                        None => Err(HelixError::new(
                            "t-test is undefined: each sample needs at least two values with spread",
                            line,
                            col,
                        )
                        .hint("two constant samples have no variance to compare.")),
                    }
                }
                "linear_regression" => {
                    arity(name, &args, 2, line, col)?;
                    // OLS fit of `y ~ x` → {slope, intercept, r_squared, slope_std_error,
                    // slope_p_value}. `missing` in either series propagates.
                    let xs = num_array(name, &args[0], line, col)?;
                    let ys = num_array(name, &args[1], line, col)?;
                    let (xs, ys) = match (xs, ys) {
                        (Some(xs), Some(ys)) => (xs, ys),
                        _ => return Ok(Value::Missing),
                    };
                    if xs.len() != ys.len() {
                        return Err(HelixError::new(
                            format!(
                                "`linear_regression` needs two equal-length arrays, got {} and {}",
                                xs.len(),
                                ys.len()
                            ),
                            line,
                            col,
                        ));
                    }
                    let floats = |xs: Vec<f64>| Value::float_array(xs);
                    match crate::stats::linear_regression(&xs, &ys) {
                        Some(f) => {
                            let fields = vec![
                                (Symbol::intern("slope"), Value::Float(f.slope)),
                                (Symbol::intern("intercept"), Value::Float(f.intercept)),
                                (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                                (Symbol::intern("slope_std_error"), Value::Float(f.slope_std_error)),
                                (Symbol::intern("slope_p_value"), Value::Float(f.slope_p_value)),
                                (Symbol::intern("rss"), Value::Float(f.rss)),
                                (Symbol::intern("predictions"), floats(f.predictions)),
                                (Symbol::intern("residuals"), floats(f.residuals)),
                            ];
                            Ok(Value::Record(Rc::new(fields)))
                        }
                        None => Err(HelixError::new(
                            "linear regression is undefined: need at least three points and variance in both x and y",
                            line,
                            col,
                        )
                        .hint("a constant predictor or response has no line to fit.")),
                    }
                }
                "multiple_regression" => {
                    // OLS fit of `y` on several predictor columns. args: (predictors, y)
                    // with an optional 3rd boolean `intercept` (default true). `missing`
                    // anywhere propagates. coefficients/std_errors/p_values are
                    // parameter-indexed arrays (with an intercept, index 0 is it).
                    if args.len() < 2 || args.len() > 3 {
                        return Err(HelixError::new(
                            format!("`multiple_regression` takes (predictors, y[, intercept]), got {}", args.len()),
                            line,
                            col,
                        ));
                    }
                    let with_intercept = match args.get(2) {
                        None => true,
                        Some(Value::Bool(b)) => *b,
                        Some(other) => {
                            return Err(type_err("multiple_regression", "a boolean `intercept` flag", other, line, col));
                        }
                    };
                    let preds = num_arrays(name, &args[0], line, col)?;
                    let y = num_array(name, &args[1], line, col)?;
                    let (preds, y) = match (preds, y) {
                        (Some(preds), Some(y)) => (preds, y),
                        _ => return Ok(Value::Missing),
                    };
                    let floats = |xs: Vec<f64>| Value::float_array(xs);
                    match crate::stats::multiple_regression(&preds, &y, with_intercept) {
                        Some(f) => {
                            let fields = vec![
                                (Symbol::intern("coefficients"), floats(f.coefficients)),
                                (Symbol::intern("std_errors"), floats(f.std_errors)),
                                (Symbol::intern("p_values"), floats(f.p_values)),
                                (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                                (Symbol::intern("adj_r_squared"), Value::Float(f.adj_r_squared)),
                                (Symbol::intern("rss"), Value::Float(f.rss)),
                                (Symbol::intern("predictions"), floats(f.predictions)),
                                (Symbol::intern("residuals"), floats(f.residuals)),
                            ];
                            Ok(Value::Record(Rc::new(fields)))
                        }
                        None => Err(HelixError::new(
                            "multiple regression is undefined: need more observations than predictors, equal-length non-collinear predictors, and variance in y",
                            line,
                            col,
                        )
                        .hint("e.g. `multiple_regression([x1, x2], y)` with enough rows.")),
                    }
                }
                "least_squares" => {
                    // OLS without inference (no std_errors/p_values) — the fast fit for
                    // model selection. args: (predictors, y[, intercept]).
                    if args.len() < 2 || args.len() > 3 {
                        return Err(HelixError::new(
                            format!("`least_squares` takes (predictors, y[, intercept]), got {}", args.len()),
                            line,
                            col,
                        ));
                    }
                    let with_intercept = match args.get(2) {
                        None => true,
                        Some(Value::Bool(b)) => *b,
                        Some(other) => {
                            return Err(type_err("least_squares", "a boolean `intercept` flag", other, line, col));
                        }
                    };
                    let preds = num_arrays(name, &args[0], line, col)?;
                    let y = num_array(name, &args[1], line, col)?;
                    let (preds, y) = match (preds, y) {
                        (Some(preds), Some(y)) => (preds, y),
                        _ => return Ok(Value::Missing),
                    };
                    match crate::stats::least_squares(&preds, &y, with_intercept) {
                        Some(f) => Ok(Value::Record(Rc::new(vec![
                            (Symbol::intern("coefficients"), Value::float_array(f.coefficients)),
                            (Symbol::intern("rss"), Value::Float(f.rss)),
                            (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                            (Symbol::intern("predictions"), Value::float_array(f.predictions)),
                            (Symbol::intern("residuals"), Value::float_array(f.residuals)),
                        ]))),
                        None => Err(HelixError::new(
                            "least squares is undefined: need at least as many rows as parameters, equal-length non-collinear predictors",
                            line,
                            col,
                        )
                        .hint("e.g. `least_squares([x1, x2], y)`.")),
                    }
                }
                "mse" | "rmse" | "mae" | "r2_score" => {
                    arity(name, &args, 2, line, col)?;
                    let a = num_array(name, &args[0], line, col)?;
                    let b = num_array(name, &args[1], line, col)?;
                    let (a, b) = match (a, b) {
                        (Some(a), Some(b)) => (a, b),
                        _ => return Ok(Value::Missing),
                    };
                    if a.len() != b.len() {
                        return Err(HelixError::new(
                            format!("`{name}` needs two equal-length arrays, got {} and {}", a.len(), b.len()),
                            line,
                            col,
                        ));
                    }
                    if a.is_empty() {
                        return Err(HelixError::new(format!("cannot compute `{name}` of empty arrays"), line, col));
                    }
                    let nf = a.len() as f64;
                    match name {
                        "mse" => Ok(Value::Float(a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / nf)),
                        "rmse" => Ok(Value::Float((a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / nf).sqrt())),
                        "mae" => Ok(Value::Float(a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f64>() / nf)),
                        // r2_score(y_true, y_pred) = 1 - SS_res/SS_tot — y_true first, to
                        // match scikit-learn and the sibling metrics (mse/mae take y_true,
                        // y_pred). SS_tot is the variance of the *true* values.
                        _ => {
                            let (actual, pred) = (&a, &b);
                            let mean = actual.iter().sum::<f64>() / nf;
                            let ss_tot: f64 = actual.iter().map(|v| (v - mean).powi(2)).sum();
                            if ss_tot == 0.0 {
                                return Err(HelixError::new(
                                    "`r2_score` is undefined: the actual values have zero variance",
                                    line,
                                    col,
                                ));
                            }
                            let ss_res: f64 = actual.iter().zip(pred).map(|(y, p)| (y - p).powi(2)).sum();
                            Ok(Value::Float(1.0 - ss_res / ss_tot))
                        }
                    }
                }
                // Information criteria for model selection (Gaussian-likelihood form):
                // aic(rss, n, k) = n*ln(rss/n) + 2k ; bic(rss, n, k) = n*ln(rss/n) + k*ln(n).
                "aic" | "bic" => {
                    arity(name, &args, 3, line, col)?;
                    let rss = args[0].as_f64().ok_or_else(|| type_err(name, "a number (rss)", &args[0], line, col))?;
                    let nn = as_int(&args[1], name, line, col)?;
                    let kk = as_int(&args[2], name, line, col)?;
                    if nn <= 0 {
                        return Err(HelixError::new(format!("`{name}` needs n > 0"), line, col));
                    }
                    if rss < 0.0 {
                        return Err(HelixError::new(format!("`{name}` needs rss >= 0"), line, col));
                    }
                    let (nf, kf) = (nn as f64, kk as f64);
                    let log_like = nf * (rss / nf).ln(); // ∝ -2·logL up to a constant
                    Ok(Value::Float(if name == "aic" {
                        log_like + 2.0 * kf
                    } else {
                        log_like + kf * nf.ln()
                    }))
                }
                // Classification metrics — free functions taking (y_true, y_pred), mirroring
                // the regression metrics above. `accuracy` is multiclass-safe; the rest are
                // binary against a positive class (3rd arg `pos_label`, default `1`).
                "accuracy" => {
                    arity(name, &args, 2, line, col)?;
                    let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                    let correct = a.iter().zip(&b).filter(|(x, y)| values_equal(x, y)).count();
                    Ok(Value::Float(correct as f64 / a.len() as f64))
                }
                "precision" | "recall" | "f1_score" => {
                    if args.len() != 2 && args.len() != 3 {
                        return Err(HelixError::new(
                            format!("`{name}` takes (y_true, y_pred) or (y_true, y_pred, pos_label), got {} arguments", args.len()),
                            line,
                            col,
                        ));
                    }
                    let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                    let pos = if args.len() == 3 { args[2].clone() } else { Value::Int(1) };
                    let (tp, fp, fa_neg, _) = binary_counts(&a, &b, &pos);
                    // sklearn convention: an undefined ratio (no predicted/actual positives)
                    // is reported as 0.0 rather than raising.
                    let precision = if tp + fp == 0 { 0.0 } else { tp as f64 / (tp + fp) as f64 };
                    let recall = if tp + fa_neg == 0 { 0.0 } else { tp as f64 / (tp + fa_neg) as f64 };
                    Ok(Value::Float(match name {
                        "precision" => precision,
                        "recall" => recall,
                        _ if precision + recall == 0.0 => 0.0,
                        _ => 2.0 * precision * recall / (precision + recall),
                    }))
                }
                "confusion_matrix" => {
                    if args.len() != 2 && args.len() != 3 {
                        return Err(HelixError::new(
                            format!("`confusion_matrix` takes (y_true, y_pred) or (y_true, y_pred, pos_label), got {} arguments", args.len()),
                            line,
                            col,
                        ));
                    }
                    let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                    let pos = if args.len() == 3 { args[2].clone() } else { Value::Int(1) };
                    let (tp, fp, fa_neg, tn) = binary_counts(&a, &b, &pos);
                    Ok(Value::Record(Rc::new(vec![
                        (Symbol::intern("tp"), Value::Int(tp)),
                        (Symbol::intern("fp"), Value::Int(fp)),
                        (Symbol::intern("fn"), Value::Int(fa_neg)),
                        (Symbol::intern("tn"), Value::Int(tn)),
                    ])))
                }
        _ => Err(HelixError::new(
            format!("internal: `{name}` routed to the wrong builtin module"),
            line,
            col,
        )),
    }
}

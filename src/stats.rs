//! Pure statistical algorithms over `&[f64]` — the numeric core of Helix's
//! descriptive and bivariate statistics. These functions assume their inputs are
//! finite and non-empty (the callers in `interp::methods` / `interp::builtins`
//! enforce that and handle `missing` propagation); they never allocate a `Value`
//! or touch a source position, so they stay trivially unit-testable and reusable.
//!
//! Helix uses **population** statistics (variance/standard deviation divide by `n`,
//! not `n - 1`), so `var(xs) == std(xs).powi(2)` holds exactly and the array verbs
//! agree with the Polars group aggregations. Sample (`n - 1`) variants can be added
//! later as explicitly named functions if the inferential-statistics layer needs
//! them. Summation routes through Neumaier compensation to bound rounding error.

use crate::interp::neumaier_sum;

/// Arithmetic mean. Precondition: `xs` is non-empty.
pub fn mean(xs: &[f64]) -> f64 {
    neumaier_sum(xs) / xs.len() as f64
}

/// Population variance (divides by `n`). Precondition: `xs` is non-empty.
pub fn variance(xs: &[f64]) -> f64 {
    let m = mean(xs);
    let sq: Vec<f64> = xs.iter().map(|x| (x - m).powi(2)).collect();
    neumaier_sum(&sq) / xs.len() as f64
}

/// Population standard deviation. Precondition: `xs` is non-empty.
pub fn std(xs: &[f64]) -> f64 {
    variance(xs).sqrt()
}

/// The `p`-quantile (`p` in `[0, 1]`) by linear interpolation between order
/// statistics — the "type 7" method (R's default, NumPy's `linear`). `p = 0`/`0.5`/
/// `1` give the min, median, and max. Precondition: `xs` is non-empty; `p` is
/// clamped to `[0, 1]`.
pub fn quantile(xs: &[f64], p: f64) -> f64 {
    let mut s: Vec<f64> = xs.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    quantile_sorted(&s, p)
}

/// As [`quantile`], but for an already-ascending slice (lets a caller sort once and
/// take several quantiles, e.g. `summary`). Precondition: `s` is sorted and non-empty.
pub fn quantile_sorted(s: &[f64], p: f64) -> f64 {
    // Defensive: Helix callers already guard emptiness (`empty_guard`), but an empty
    // slice here would underflow `s.len() - 1` below — never panic on it.
    if s.is_empty() {
        return f64::NAN;
    }
    let p = p.clamp(0.0, 1.0);
    if s.len() == 1 {
        return s[0];
    }
    let h = (s.len() - 1) as f64 * p; // fractional rank in [0, n-1]
    let lo = h.floor() as usize;
    let hi = h.ceil() as usize;
    let frac = h - lo as f64;
    s[lo] + (s[hi] - s[lo]) * frac
}

/// The median (the 0.5-quantile). Precondition: `xs` is non-empty.
pub fn median(xs: &[f64]) -> f64 {
    quantile(xs, 0.5)
}

/// Pearson product-moment correlation coefficient. Returns `None` when either
/// series has zero variance (a constant series), where correlation is undefined.
/// Precondition: `xs` and `ys` have equal, non-empty length.
pub fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let mx = mean(xs);
    let my = mean(ys);
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    for (&x, &y) in xs.iter().zip(ys) {
        let dx = x - mx;
        let dy = y - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    let denom = (sxx * syy).sqrt();
    if denom == 0.0 {
        None
    } else {
        // Clamp to [-1, 1] to absorb rounding past the bound.
        Some((sxy / denom).clamp(-1.0, 1.0))
    }
}

/// Sample variance (divides by `n - 1`, Bessel's correction). Used by inferential
/// statistics, where the data is a sample of a larger population. Precondition:
/// `xs` has at least two elements.
pub fn sample_variance(xs: &[f64]) -> f64 {
    let m = mean(xs);
    let sq: Vec<f64> = xs.iter().map(|x| (x - m).powi(2)).collect();
    neumaier_sum(&sq) / (xs.len() as f64 - 1.0)
}

/// Variance with an explicit **delta degrees of freedom** (`ddof`): the sum of squared deviations
/// divided by `n - ddof`. `ddof = 0` is the population variance (Helix's default — see the module
/// note); `ddof = 1` is the sample variance (Bessel's correction). Powers `var(ddof)` / `std(ddof)`.
/// Precondition: `n > ddof` (the caller checks and raises a precise error otherwise).
pub fn variance_ddof(xs: &[f64], ddof: usize) -> f64 {
    let m = mean(xs);
    let sq: Vec<f64> = xs.iter().map(|x| (x - m).powi(2)).collect();
    neumaier_sum(&sq) / (xs.len() as f64 - ddof as f64)
}

// ---- Special functions -----------------------------------------------------
// Standard numerical algorithms (Numerical Recipes) for log-gamma and the regularized
// incomplete beta, plus `libm`'s error function — the machinery the normal and
// Student's-t distribution functions need.

/// The error function, to full double precision.
///
/// This was the Abramowitz & Stegun 7.1.26 rational approximation, whose stated
/// maximum absolute error is ~1.5e-7 — the only math builtin in Helix not computed
/// to double precision, while `exp`/`ln`/`sqrt`/`sin`/… are all 0 ULP against glibc.
/// Near zero A&S 7.1.26 leaves a fixed ~1e-9 pedestal, so its *relative* error is
/// unbounded (89% at x = 1e-9) and the `x == 0.0` special case it needed made `erf`
/// discontinuous at the origin.
///
/// `libm` is the Rust port of musl's libm (the Sun/FreeBSD `s_erf.c` lineage),
/// accurate to <1 ULP. It is **already compiled into this binary** — cranelift and
/// faer both depend on it — so this is a new edge to an existing crate, not new
/// supply-chain surface.
pub fn erf(x: f64) -> f64 {
    libm::erf(x)
}

/// The complementary error function, `1 - erf(x)`, computed without cancellation.
///
/// Internal (not a Helix builtin): it exists so [`normal_cdf`] has a numerically
/// stable left tail. Computing `1 - erf(x)` directly would defeat the purpose.
pub fn erfc(x: f64) -> f64 {
    libm::erfc(x)
}

/// Standard normal cumulative distribution function, `P(Z <= x)`.
///
/// Routed through `erfc`, **not** `0.5 * (1 + erf(x / √2))`. That formula is
/// catastrophically cancelling in the left tail no matter how accurate `erf` is:
/// with a *perfect* `erf` it still gives a relative error of 2.3e-06 at x = -7 and
/// returns exactly `0.0` for x <= -9, where the true value is 1.1e-19. The left
/// tail IS the p-value use case, so fixing `erf` alone would not have fixed it.
pub fn normal_cdf(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

/// Standard normal probability density at `x`.
pub fn normal_pdf(x: f64) -> f64 {
    const INV_SQRT_2PI: f64 = 0.398_942_280_401_432_7; // 1 / sqrt(2*pi)
    INV_SQRT_2PI * (-0.5 * x * x).exp()
}

/// Natural log of the gamma function for `x > 0` (Lanczos approximation, g = 5).
fn gammaln(x: f64) -> f64 {
    const COF: [f64; 6] = [
        76.180_091_729_471_46,
        -86.505_320_329_416_77,
        24.014_098_240_830_91,
        -1.231_739_572_450_155,
        0.120_865_097_386_617_9e-2,
        -0.539_523_938_495_3e-5,
    ];
    let mut tmp = x + 5.5;
    tmp -= (x + 0.5) * tmp.ln();
    let mut ser = 1.000_000_000_190_015;
    let mut y = x;
    for c in COF {
        y += 1.0;
        ser += c / y;
    }
    -tmp + (2.506_628_274_631_000_5 * ser / x).ln()
}

/// Continued-fraction expansion for the incomplete beta function (Numerical Recipes
/// `betacf`), evaluated by the modified Lentz method.
fn betacf(a: f64, b: f64, x: f64) -> f64 {
    const MAXIT: usize = 200;
    const EPS: f64 = 3.0e-12;
    const FPMIN: f64 = 1.0e-300;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < FPMIN {
        d = FPMIN;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=MAXIT {
        let m = m as f64;
        let m2 = 2.0 * m;
        // Even step.
        let aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        h *= d * c;
        // Odd step.
        let aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    h
}

/// The regularized incomplete beta function `I_x(a, b)` (Numerical Recipes `betai`).
/// `x` is clamped to `[0, 1]`. This is the workhorse behind the Student's-t CDF.
pub fn betai(a: f64, b: f64, x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    if x == 0.0 || x == 1.0 {
        return x;
    }
    let bt = (gammaln(a + b) - gammaln(a) - gammaln(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        bt * betacf(a, b, x) / a
    } else {
        1.0 - bt * betacf(b, a, 1.0 - x) / b
    }
}

/// Welch's two-sample t-test (does not assume equal variances — R's `t.test`
/// default). Returns `(t statistic, Welch–Satterthwaite degrees of freedom, two-sided
/// p-value)`, or `None` when a sample has fewer than two values or the combined
/// standard error is zero (both samples constant), where the test is undefined.
pub fn welch_t_test(xs: &[f64], ys: &[f64]) -> Option<(f64, f64, f64)> {
    let (n1, n2) = (xs.len() as f64, ys.len() as f64);
    if xs.len() < 2 || ys.len() < 2 {
        return None;
    }
    let v1 = sample_variance(xs);
    let v2 = sample_variance(ys);
    let se2 = v1 / n1 + v2 / n2;
    if se2 == 0.0 {
        return None;
    }
    let t = (mean(xs) - mean(ys)) / se2.sqrt();
    // Welch–Satterthwaite degrees of freedom.
    let df =
        se2 * se2 / ((v1 / n1).powi(2) / (n1 - 1.0) + (v2 / n2).powi(2) / (n2 - 1.0));
    // Two-sided p-value from the Student's-t survival function.
    let p = betai(df / 2.0, 0.5, df / (df + t * t));
    Some((t, df, p))
}

/// The result of an ordinary least-squares simple linear regression `y ~ x`:
/// the fitted line plus the coefficient of determination and inference on the slope.
pub struct LinFit {
    pub slope: f64,
    pub intercept: f64,
    pub r_squared: f64,
    pub slope_std_error: f64,
    pub slope_p_value: f64,
    /// Residual sum of squares — exposed for model selection (MDL/AIC/BIC).
    pub rss: f64,
    /// Fitted values `ŷ = intercept + slope·x`, aligned with the inputs.
    pub predictions: Vec<f64>,
    /// `y − ŷ`, aligned with the inputs.
    pub residuals: Vec<f64>,
}

/// Ordinary least-squares fit of `y = intercept + slope * x`, with the slope's
/// standard error and two-sided p-value (Student's t on `n - 2` degrees of freedom).
/// Returns `None` unless there are at least three points and both `x` and `y` have
/// nonzero variance — the conditions under which the slope and its inference are
/// defined. Precondition: `xs` and `ys` have equal length.
pub fn linear_regression(xs: &[f64], ys: &[f64]) -> Option<LinFit> {
    let n = xs.len();
    if n < 3 {
        return None; // need n - 2 >= 1 residual degrees of freedom for inference
    }
    let mx = mean(xs);
    let my = mean(ys);
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    for (&x, &y) in xs.iter().zip(ys) {
        let dx = x - mx;
        let dy = y - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx == 0.0 || syy == 0.0 {
        return None; // a constant predictor or response has no relationship to fit
    }
    let slope = sxy / sxx;
    let intercept = my - slope * mx;
    let r_squared = (sxy * sxy) / (sxx * syy);
    // Residual sum of squares and the slope's standard error.
    let rss = syy - slope * sxy; // == syy * (1 - r_squared)
    let resid_var = rss / (n as f64 - 2.0);
    let slope_std_error = (resid_var / sxx).sqrt();
    // Two-sided p-value for H0: slope = 0, via the t survival function.
    let slope_p_value = if slope_std_error == 0.0 {
        0.0 // a perfect fit: the slope is infinitely well determined
    } else {
        let t = slope / slope_std_error;
        let df = n as f64 - 2.0;
        betai(df / 2.0, 0.5, df / (df + t * t))
    };
    let predictions: Vec<f64> = xs.iter().map(|&x| intercept + slope * x).collect();
    let residuals: Vec<f64> = ys.iter().zip(&predictions).map(|(&y, &p)| y - p).collect();
    Some(LinFit {
        slope,
        intercept,
        r_squared,
        slope_std_error,
        slope_p_value,
        rss,
        predictions,
        residuals,
    })
}

/// The result of an ordinary least-squares multiple regression `y ~ x1 + x2 + ...`.
/// The vectors are parallel and parameter-indexed: index 0 is the intercept, then one
/// entry per predictor in the order supplied.
pub struct MultiFit {
    pub coefficients: Vec<f64>,
    pub std_errors: Vec<f64>,
    pub p_values: Vec<f64>,
    pub r_squared: f64,
    pub adj_r_squared: f64,
    /// Residual sum of squares — exposed for model selection (MDL/AIC/BIC).
    pub rss: f64,
    /// Fitted values `ŷ = X·β`, aligned with the inputs.
    pub predictions: Vec<f64>,
    /// `y − ŷ`, aligned with the inputs.
    pub residuals: Vec<f64>,
}

/// Invert a square matrix by Gauss-Jordan elimination with partial pivoting. Returns
/// `None` if the matrix is singular (to within a small pivot tolerance).
fn invert(m: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = m.len();
    // Augment with the identity: [m | I].
    let mut a: Vec<Vec<f64>> = m
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut r = row.clone();
            r.extend((0..n).map(|j| if i == j { 1.0 } else { 0.0 }));
            r
        })
        .collect();
    for col in 0..n {
        // Partial pivot: move the largest-magnitude entry in this column to the diagonal.
        let mut piv = col;
        for r in (col + 1)..n {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None; // singular (e.g. collinear predictors)
        }
        a.swap(col, piv);
        let d = a[col][col];
        a[col].iter_mut().for_each(|x| *x /= d);
        let pivot_row = a[col].clone();
        for (r, row) in a.iter_mut().enumerate() {
            if r != col {
                let f = row[col];
                for (x, p) in row.iter_mut().zip(&pivot_row) {
                    *x -= f * p;
                }
            }
        }
    }
    Some(a.iter().map(|row| row[n..].to_vec()).collect())
}

/// Ordinary least-squares multiple regression of `y` on the given predictor columns
/// (each of length `n`), fitting `y = b0 + b1*x1 + ... + bp*xp` via the normal
/// equations `(XᵀX) b = Xᵀy`. Returns the coefficients (intercept first), their
/// standard errors and two-sided p-values (t on `n - k` degrees of freedom, `k` the
/// parameter count), and the R² / adjusted-R². Returns `None` when there is no
/// predictor, fewer observations than parameters plus one, a length mismatch, a
/// constant response, or collinear predictors (a singular `XᵀX`).
/// `intercept` controls whether a constant term is auto-added. With
/// `intercept = false` the fit passes through the origin and the coefficients map
/// one-to-one to the supplied predictors (so a user-supplied constant column won't
/// collide with an auto-intercept).
/// The shared core of every OLS fit: validate, build and solve the normal
/// equations `(XᵀX)β = Xᵀy`, and compute fitted values / residuals / RSS / TSS.
/// Returns `None` on no predictors, a length mismatch, fewer rows than parameters,
/// or a singular `XᵀX` (collinear predictors). `intercept` prepends an all-ones
/// column. Both [`least_squares`] and [`multiple_regression`] build on this.
struct OlsCore {
    beta: Vec<f64>,
    inv: Vec<Vec<f64>>,
    predictions: Vec<f64>,
    residuals: Vec<f64>,
    rss: f64,
    tss: f64,
    k: usize,
    n: usize,
}

fn ols_core(predictors: &[Vec<f64>], y: &[f64], intercept: bool) -> Option<OlsCore> {
    let n = y.len();
    let p = predictors.len();
    let k = if intercept { p + 1 } else { p };
    if p == 0 || k == 0 || n < k || predictors.iter().any(|c| c.len() != n) {
        return None;
    }
    // X[i][j]: with an intercept, column 0 is all-ones and 1..=p are predictors;
    // without, columns 0..p are the predictors directly.
    let xij = |i: usize, j: usize| {
        if intercept {
            if j == 0 { 1.0 } else { predictors[j - 1][i] }
        } else {
            predictors[j][i]
        }
    };
    let mut xtx = vec![vec![0.0; k]; k];
    let mut xty = vec![0.0; k];
    for a in 0..k {
        for (b, slot) in xtx[a].iter_mut().enumerate() {
            *slot = (0..n).map(|i| xij(i, a) * xij(i, b)).sum();
        }
        xty[a] = (0..n).map(|i| xij(i, a) * y[i]).sum();
    }
    let inv = invert(&xtx)?;
    let beta: Vec<f64> = (0..k).map(|a| (0..k).map(|b| inv[a][b] * xty[b]).sum()).collect();
    let predictions: Vec<f64> =
        (0..n).map(|i| (0..k).map(|j| beta[j] * xij(i, j)).sum()).collect();
    let residuals: Vec<f64> = (0..n).map(|i| y[i] - predictions[i]).collect();
    let rss: f64 = residuals.iter().map(|r| r * r).sum();
    let ybar = mean(y);
    let tss: f64 = y.iter().map(|v| (v - ybar).powi(2)).sum();
    Some(OlsCore { beta, inv, predictions, residuals, rss, tss, k, n })
}

pub fn multiple_regression(
    predictors: &[Vec<f64>],
    y: &[f64],
    intercept: bool,
) -> Option<MultiFit> {
    let c = ols_core(predictors, y, intercept)?;
    // Inference needs residual degrees of freedom and a non-constant response.
    if c.n <= c.k || c.tss == 0.0 {
        return None;
    }
    let (beta, inv, rss, n, k) = (c.beta, c.inv, c.rss, c.n, c.k);
    let r_squared = 1.0 - rss / c.tss;
    let df = (n - k) as f64;
    let adj_r_squared = 1.0 - (1.0 - r_squared) * (n - 1) as f64 / df;
    let resid_var = rss / df;
    let mut std_errors = Vec::with_capacity(k);
    let mut p_values = Vec::with_capacity(k);
    for j in 0..k {
        let se = (resid_var * inv[j][j]).sqrt();
        let pv = if se == 0.0 {
            0.0 // a perfect fit determines every coefficient exactly
        } else {
            let t = beta[j] / se;
            betai(df / 2.0, 0.5, df / (df + t * t))
        };
        std_errors.push(se);
        p_values.push(pv);
    }
    Some(MultiFit {
        coefficients: beta,
        std_errors,
        p_values,
        r_squared,
        adj_r_squared,
        rss,
        predictions: c.predictions,
        residuals: c.residuals,
    })
}

/// A lightweight OLS fit: coefficients + fit quality, **without** the inferential
/// statistics (standard errors / p-values). The expensive part of
/// [`multiple_regression`] for a search loop is the per-coefficient `betai`
/// continued-fraction in the p-values — pure waste when you only score models by
/// RSS. `least_squares` skips it, so structure-search / model-selection loops that
/// call it thousands of times run substantially faster. `intercept` behaves as in
/// `multiple_regression`. Allows `n == k` (an exact fit), unlike the inferential
/// version which needs residual degrees of freedom.
pub struct LsqFit {
    pub coefficients: Vec<f64>,
    pub rss: f64,
    pub r_squared: f64,
    pub predictions: Vec<f64>,
    pub residuals: Vec<f64>,
}

pub fn least_squares(predictors: &[Vec<f64>], y: &[f64], intercept: bool) -> Option<LsqFit> {
    let c = ols_core(predictors, y, intercept)?;
    let r_squared = if c.tss == 0.0 { 0.0 } else { 1.0 - c.rss / c.tss };
    Some(LsqFit {
        coefficients: c.beta,
        rss: c.rss,
        r_squared,
        predictions: c.predictions,
        residuals: c.residuals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn mean_variance_std_are_population() {
        let xs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        assert!(approx(mean(&xs), 5.0));
        assert!(approx(variance(&xs), 4.0)); // population variance
        assert!(approx(std(&xs), 2.0));
        // The defining identity: var == std^2.
        assert!(approx(variance(&xs), std(&xs).powi(2)));
    }

    #[test]
    fn median_handles_even_and_odd_lengths() {
        assert!(approx(median(&[1.0, 2.0, 3.0]), 2.0));
        assert!(approx(median(&[1.0, 2.0, 3.0, 4.0]), 2.5));
        assert!(approx(median(&[5.0]), 5.0));
    }

    #[test]
    fn quantile_matches_type7_endpoints_and_interior() {
        let xs = [0.0, 1.0, 2.0, 3.0, 4.0];
        assert!(approx(quantile(&xs, 0.0), 0.0));
        assert!(approx(quantile(&xs, 1.0), 4.0));
        assert!(approx(quantile(&xs, 0.5), 2.0));
        assert!(approx(quantile(&xs, 0.25), 1.0));
        // Interior interpolation: rank 0.1*(n-1)=0.4 between 0 and 1.
        assert!(approx(quantile(&xs, 0.1), 0.4));
    }

    #[test]
    fn pearson_is_one_for_a_line_and_none_for_a_constant() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        let up = [2.0, 4.0, 6.0, 8.0];
        let down = [8.0, 6.0, 4.0, 2.0];
        assert!(approx(pearson(&xs, &up).unwrap(), 1.0));
        assert!(approx(pearson(&xs, &down).unwrap(), -1.0));
        assert_eq!(pearson(&xs, &[3.0, 3.0, 3.0, 3.0]), None); // constant series
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn erf_and_normal_match_known_values() {
        assert!(close(erf(0.0), 0.0, 1e-7));
        assert!(close(erf(1.0), 0.842_700_79, 1e-6));
        assert!(close(erf(-1.0), -0.842_700_79, 1e-6)); // odd function
        assert!(close(normal_cdf(0.0), 0.5, 1e-7));
        assert!(close(normal_cdf(1.96), 0.975, 1e-3)); // the canonical 95% bound
        assert!(close(normal_cdf(-1.96), 0.025, 1e-3));
        assert!(close(normal_pdf(0.0), 0.398_942_28, 1e-7)); // 1/sqrt(2*pi)
    }

    #[test]
    fn betai_is_a_cdf_and_symmetric() {
        assert_eq!(betai(2.0, 3.0, 0.0), 0.0);
        assert_eq!(betai(2.0, 3.0, 1.0), 1.0);
        // The symmetric beta is 0.5 at the midpoint.
        assert!(close(betai(3.0, 3.0, 0.5), 0.5, 1e-9));
        // Reflection identity I_x(a,b) = 1 - I_{1-x}(b,a).
        assert!(close(betai(2.0, 5.0, 0.3), 1.0 - betai(5.0, 2.0, 0.7), 1e-9));
    }

    #[test]
    fn welch_t_test_matches_a_reference_case() {
        // x = 1..5, y = 2,4,6,8,10. R's t.test(x, y) gives t = -1.8974, df = 5.8822,
        // p = 0.1085 (Welch). Match the statistic and df tightly; p within tolerance.
        let x = [1.0, 2.0, 3.0, 4.0, 5.0];
        let y = [2.0, 4.0, 6.0, 8.0, 10.0];
        let (t, df, p) = welch_t_test(&x, &y).unwrap();
        assert!(close(t, -1.897_37, 1e-4), "t = {t}");
        assert!(close(df, 5.882_35, 1e-4), "df = {df}");
        assert!(close(p, 0.1085, 2e-3), "p = {p}");
        // Identical samples → zero statistic and a p-value of 1 (no difference).
        let (t0, _, p0) = welch_t_test(&x, &x).unwrap();
        assert!(close(t0, 0.0, 1e-12) && close(p0, 1.0, 1e-9));
        // Too few values, or both samples constant → undefined.
        assert_eq!(welch_t_test(&[1.0], &y), None);
        assert_eq!(welch_t_test(&[2.0, 2.0], &[2.0, 2.0]), None);
    }

    #[test]
    fn linear_regression_matches_a_textbook_fit() {
        // x = 1..5, y = 2,4,5,4,5. R's lm gives intercept 2.2, slope 0.6, R^2 0.6,
        // and a slope p-value of 0.1240.
        let x = [1.0, 2.0, 3.0, 4.0, 5.0];
        let y = [2.0, 4.0, 5.0, 4.0, 5.0];
        let f = linear_regression(&x, &y).unwrap();
        assert!(close(f.slope, 0.6, 1e-9), "slope = {}", f.slope);
        assert!(close(f.intercept, 2.2, 1e-9), "intercept = {}", f.intercept);
        assert!(close(f.r_squared, 0.6, 1e-9), "r2 = {}", f.r_squared);
        assert!(close(f.slope_std_error, 0.282_842_7, 1e-6), "se = {}", f.slope_std_error);
        assert!(close(f.slope_p_value, 0.1240, 2e-3), "p = {}", f.slope_p_value);
        // A perfect line: slope 2, intercept 0, R^2 = 1, p-value 0.
        let perfect = linear_regression(&x, &[2.0, 4.0, 6.0, 8.0, 10.0]).unwrap();
        assert!(close(perfect.slope, 2.0, 1e-9) && close(perfect.r_squared, 1.0, 1e-12));
        assert!(close(perfect.slope_p_value, 0.0, 1e-12));
        // Too few points, or no variance in x / y → undefined.
        assert!(linear_regression(&[1.0, 2.0], &[1.0, 2.0]).is_none());
        assert!(linear_regression(&[2.0, 2.0, 2.0], &y).is_none());
    }

    #[test]
    fn multiple_regression_recovers_a_known_plane() {
        // y = 1 + 2*x1 + 3*x2 exactly → coefficients [1, 2, 3], R^2 = 1.
        let x1 = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let x2 = vec![2.0, 1.0, 4.0, 3.0, 5.0];
        let y: Vec<f64> = (0..5).map(|i| 1.0 + 2.0 * x1[i] + 3.0 * x2[i]).collect();
        let f = multiple_regression(&[x1, x2], &y, true).unwrap();
        assert!(close(f.coefficients[0], 1.0, 1e-6), "b0 = {}", f.coefficients[0]);
        assert!(close(f.coefficients[1], 2.0, 1e-6), "b1 = {}", f.coefficients[1]);
        assert!(close(f.coefficients[2], 3.0, 1e-6), "b2 = {}", f.coefficients[2]);
        assert!(close(f.r_squared, 1.0, 1e-9), "r2 = {}", f.r_squared);
    }

    #[test]
    fn multiple_regression_with_one_predictor_equals_simple() {
        // A single-predictor multiple regression must match `linear_regression`.
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y = vec![2.0, 4.0, 5.0, 4.0, 5.0];
        let m = multiple_regression(std::slice::from_ref(&x), &y, true).unwrap();
        let s = linear_regression(&x, &y).unwrap();
        assert!(close(m.coefficients[0], s.intercept, 1e-9));
        assert!(close(m.coefficients[1], s.slope, 1e-9));
        assert!(close(m.r_squared, s.r_squared, 1e-9));
    }

    #[test]
    fn multiple_regression_rejects_degenerate_inputs() {
        let x = vec![1.0, 2.0, 3.0, 4.0];
        // Collinear predictors (x2 = 2*x1) → singular XᵀX.
        let x2: Vec<f64> = x.iter().map(|v| 2.0 * v).collect();
        let y = vec![1.0, 3.0, 2.0, 5.0];
        assert!(multiple_regression(&[x.clone(), x2], &y, true).is_none());
        // Fewer observations than parameters + 1.
        assert!(multiple_regression(&[vec![1.0, 2.0], vec![3.0, 1.0]], &[1.0, 2.0], true).is_none());
        // No predictors at all.
        assert!(multiple_regression(&[], &y, true).is_none());
    }
}

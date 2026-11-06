//! Pure statistical algorithms over `&[f64]` — the numeric core of Helix's
//! descriptive and bivariate statistics. These functions assume their inputs are
//! finite and non-empty (the callers in `interp::methods` / `interp::builtins`
//! enforce that and handle `missing` propagation); they never allocate a `Value`
//! or touch a source position, so they stay trivially unit-testable and reusable.
//!
//! Helix's `std`/`var`/`cov` are the **sample** estimates by default (divide by `n - 1`,
//! Bessel's correction — ADR 0049, the pandas/R/Excel default), with `ddof` selecting the
//! population's (`ddof = 0`); the grouped frame `std` has always been the sample one, so
//! the language has one `std`. `var(xs) == std(xs).powi(2)` holds exactly either way. A
//! spread over fewer than `ddof + 1` values is undefined and the callers answer `missing`.
//! Summation routes through Neumaier compensation to bound rounding error.

use crate::interp::neumaier_sum;

/// Arithmetic mean. Precondition: `xs` is non-empty.
pub fn mean(xs: &[f64]) -> f64 {
    neumaier_sum(xs) / xs.len() as f64
}

/// Population variance (divides by `n`). Precondition: `xs` is non-empty.
pub fn population_variance(xs: &[f64]) -> f64 {
    let m = mean(xs);
    let sq: Vec<f64> = xs.iter().map(|x| (x - m).powi(2)).collect();
    neumaier_sum(&sq) / xs.len() as f64
}

/// Population standard deviation. Precondition: `xs` is non-empty.
pub fn population_std(xs: &[f64]) -> f64 {
    population_variance(xs).sqrt()
}

/// Standard deviation with an explicit `ddof` — `1` is the sample estimate, Helix's default
/// (ADR 0049); `0` the population's. Precondition: `n > ddof`.
pub fn std_ddof(xs: &[f64], ddof: usize) -> f64 {
    variance_ddof(xs, ddof).sqrt()
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

/// Covariance of two equal-length series: `sum((x - mx)(y - my)) / (n - ddof)` — population
/// with `ddof = 0`, the sample estimator with `ddof = 1`. Precondition: equal, non-empty
/// length and `ddof < n`.
pub fn covariance(xs: &[f64], ys: &[f64], ddof: usize) -> f64 {
    let mx = mean(xs);
    let my = mean(ys);
    let mut sxy = 0.0;
    for (&x, &y) in xs.iter().zip(ys) {
        sxy += (x - mx) * (y - my);
    }
    sxy / (xs.len() - ddof) as f64
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
/// divided by `n - ddof`. `ddof = 1` is the sample variance (Bessel's correction — Helix's
/// default, ADR 0049); `ddof = 0` is the population's. Powers `var(ddof)` / `std(ddof)`.
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
    let constant = |v: &[f64]| v.iter().all(|&a| a == v[0]);
    if constant(xs) || constant(ys) {
        return None; // a constant predictor or response has no relationship to fit
    }
    // The fit itself is the one `multiple_regression` makes (Householder QR with refinement,
    // see `ols_core`), so the two never disagree on the same data. The textbook
    // `slope = sxy / sxx`, `intercept = ȳ − slope·x̄` it replaced lost a digit or two: 12.8
    // digits of NIST Norris's certified intercept, where 14.1 are there to be had.
    let c = ols_core(&[xs.to_vec()], ys, true)?;
    let r_squared = c.r_squared();
    let (intercept, slope, rss) = (c.beta[0], c.beta[1], c.rss);
    let resid_var = rss / (n as f64 - 2.0);
    let slope_std_error = (resid_var * c.inv_diag[1]).sqrt();
    // Two-sided p-value for H0: slope = 0, via the t survival function.
    let slope_p_value = if slope_std_error == 0.0 {
        0.0 // a perfect fit: the slope is infinitely well determined
    } else {
        let t = slope / slope_std_error;
        let df = n as f64 - 2.0;
        betai(df / 2.0, 0.5, df / (df + t * t))
    };
    let (predictions, residuals) = (c.predictions, c.residuals);
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

/// Ordinary least-squares multiple regression of `y` on the given predictor columns
/// (each of length `n`), fitting `y = b0 + b1*x1 + ... + bp*xp` by least squares (see
/// [`ols_core`] for how). Returns the coefficients (intercept first), their
/// standard errors and two-sided p-values (t on `n - k` degrees of freedom, `k` the
/// parameter count), and the R² / adjusted-R². Returns `None` when there is no
/// predictor, fewer observations than parameters plus one, a length mismatch, a
/// constant response, or collinear predictors.
/// `intercept` controls whether a constant term is auto-added. With
/// `intercept = false` the fit passes through the origin and the coefficients map
/// one-to-one to the supplied predictors (so a user-supplied constant column won't
/// collide with an auto-intercept).
/// What [`ols_core`] hands both fits.
struct OlsCore {
    beta: Vec<f64>,
    /// The diagonal of `(XᵀX)⁻¹`, which is all a standard error needs.
    inv_diag: Vec<f64>,
    predictions: Vec<f64>,
    residuals: Vec<f64>,
    rss: f64,
    /// What rounding `rss` to a double dropped, so `tss − rss` can be formed exactly.
    rss_lo: f64,
    tss: f64,
    ess: f64,
    k: usize,
    n: usize,
    intercept: bool,
}

/// A compensated (error-free-transformation) accumulator: each product's rounding error is
/// recovered with a fused multiply-add and each sum's with TwoSum (Ogita, Rump & Oishi's
/// Dot2), so the result carries close to twice a double's precision before its last
/// rounding. `mul_add` is a correctly rounded fused operation on every platform, so the
/// value is the same wherever it runs.
#[derive(Clone, Copy)]
struct Acc {
    s: f64,
    c: f64,
}

impl Acc {
    fn new(init: f64) -> Self {
        Acc { s: init, c: 0.0 }
    }
    #[inline]
    fn add_prod(&mut self, x: f64, b: f64) {
        let prod = x * b;
        let prod_err = x.mul_add(b, -prod);
        let t = self.s + prod;
        let bv = t - self.s;
        self.c += prod_err + ((self.s - (t - bv)) + (prod - bv));
        self.s = t;
    }
    fn value(self) -> f64 {
        self.s + self.c
    }
    /// The sum as a double-length pair `(hi, lo)`, `hi = fl(hi + lo)` (Fast2Sum).
    fn pair(self) -> (f64, f64) {
        let hi = self.s + self.c;
        (hi, self.c - (hi - self.s))
    }
}

impl OlsCore {
    fn r_squared(&self) -> f64 {
        // (tss − rss)/tss rather than 1 − rss/tss: one rounding instead of two, and rss's
        // low part included, so an exact R² of 0.6 prints as 0.6 and not 0.6000000000000001.
        if self.rss <= self.ess {
            ((self.tss - self.rss) - self.rss_lo) / self.tss
        } else {
            self.ess / self.tss
        }
    }
}

/// The shared core of every OLS fit: validate, factor `X = QR` by Householder reflections,
/// solve `Rβ = Qᵀy`, and compute fitted values / residuals / RSS / TSS. Returns `None` on no
/// predictors, a length mismatch, fewer rows than parameters, or a rank-deficient `X`
/// (collinear predictors). `intercept` prepends an all-ones column. Both [`least_squares`]
/// and [`multiple_regression`] build on this.
///
/// WHY QR AND NOT THE NORMAL EQUATIONS. This used to form `XᵀX` and invert it by
/// Gauss-Jordan elimination, which squares the condition number of the problem: an `X`
/// with condition 10⁸ becomes a 10¹⁶ system, past what a double can hold. Measured against
/// NIST's certified Statistical Reference Datasets (`examples/statistics/nist.helix`), that
/// gave 0 correct digits on Filip (a NaN standard error), 6 on the Wampler polynomials and
/// 7 on Longley, where exact arithmetic on the same doubles shows 14–15 are there to be
/// had. Householder QR works on `X` itself, so it loses digits in proportion to the
/// condition of `X`, not its square.
///
/// The rank test is RELATIVE: a column is collinear when its `R` diagonal is negligible
/// against the largest. The normal equations refused any pivot below an absolute `1e-12`,
/// so the same well-posed fit was "collinear" when `x` was measured in units of 10⁻⁷.
fn ols_core(predictors: &[Vec<f64>], y: &[f64], intercept: bool) -> Option<OlsCore> {
    let n = y.len();
    let p = predictors.len();
    let k = if intercept { p + 1 } else { p };
    if p == 0 || k == 0 || n < k || predictors.iter().any(|c| c.len() != n) {
        return None;
    }
    // X's columns: with an intercept, column 0 is all-ones and 1..=p are the predictors;
    // without, columns 0..p are the predictors directly.
    let xcols: Vec<Vec<f64>> = if intercept {
        std::iter::once(vec![1.0; n]).chain(predictors.iter().cloned()).collect()
    } else {
        predictors.to_vec()
    };
    if xcols.iter().flatten().chain(y).any(|v| !v.is_finite()) {
        return None;
    }
    // Column-major working copy; the reflectors overwrite it and `qty` becomes Qᵀy.
    let mut a: Vec<Vec<f64>> = xcols.clone();
    let mut qty = y.to_vec();
    let mut rdiag = vec![0.0; k];
    for j in 0..k {
        // ‖a[j][j..]‖, scaled so a column of huge or tiny values neither overflows nor
        // underflows on the way to the square root.
        let scale = a[j][j..].iter().fold(0.0_f64, |m, v| m.max(v.abs()));
        if scale == 0.0 {
            return None;
        }
        let norm = scale * a[j][j..].iter().map(|v| (v / scale).powi(2)).sum::<f64>().sqrt();
        // Reflect onto -sign(x₀)‖x‖ so v₀ = x₀ - α never cancels.
        let alpha = if a[j][j] > 0.0 { -norm } else { norm };
        a[j][j] -= alpha;
        let vtv: f64 = a[j][j..].iter().map(|v| v * v).sum();
        let (head, tail) = a.split_at_mut(j + 1);
        let v = &head[j][j..];
        for col in tail.iter_mut() {
            let s = 2.0 * v.iter().zip(&col[j..]).map(|(a, b)| a * b).sum::<f64>() / vtv;
            col[j..].iter_mut().zip(v).for_each(|(c, vi)| *c -= s * vi);
        }
        let s = 2.0 * v.iter().zip(&qty[j..]).map(|(a, b)| a * b).sum::<f64>() / vtv;
        qty[j..].iter_mut().zip(v).for_each(|(c, vi)| *c -= s * vi);
        rdiag[j] = alpha;
    }
    // R is rdiag on the diagonal and a[c][j] (j < c) above it.
    let r = |row: usize, col: usize| if row == col { rdiag[row] } else { a[col][row] };
    let rmax = rdiag.iter().fold(0.0_f64, |m, d| m.max(d.abs()));
    let tol = rmax * f64::EPSILON * (n.max(k) as f64);
    if rdiag.iter().any(|d| d.abs() <= tol) {
        return None;
    }
    // Qᵀv for any v, from the stored reflectors (column j's `a[j][j..]` is reflector j).
    let apply_qt = |v: &mut [f64]| {
        for (j, refl) in a.iter().enumerate() {
            let refl = &refl[j..];
            let vtv: f64 = refl.iter().map(|x| x * x).sum();
            let s = 2.0 * refl.iter().zip(&v[j..]).map(|(a, b)| a * b).sum::<f64>() / vtv;
            v[j..].iter_mut().zip(refl).for_each(|(c, vi)| *c -= s * vi);
        }
    };
    // Back-substitute R·x = rhs[..k].
    let back = |rhs: &[f64]| -> Vec<f64> {
        let mut x = vec![0.0; k];
        for row in (0..k).rev() {
            let s: f64 = ((row + 1)..k).map(|c| r(row, c) * x[c]).sum();
            x[row] = (rhs[row] - s) / r(row, row);
        }
        x
    };
    // Qv: the reflectors applied in reverse order.
    let apply_q = |v: &mut [f64]| {
        for (j, refl) in a.iter().enumerate().rev() {
            let refl = &refl[j..];
            let vtv: f64 = refl.iter().map(|x| x * x).sum();
            let s = 2.0 * refl.iter().zip(&v[j..]).map(|(a, b)| a * b).sum::<f64>() / vtv;
            v[j..].iter_mut().zip(refl).for_each(|(c, vi)| *c -= s * vi);
        }
    };
    // Solve Rᵀ·x = rhs (forward substitution).
    let forward_t = |rhs: &[f64]| -> Vec<f64> {
        let mut x = vec![0.0; k];
        for row in 0..k {
            let s: f64 = (0..row).map(|c| r(c, row) * x[c]).sum();
            x[row] = (rhs[row] - s) / r(row, row);
        }
        x
    };
    // y − Xβ (minus `less` when given), one compensated accumulator per row; X is walked a
    // column at a time so every access is contiguous.
    let residual = |beta: &[f64], less: Option<&[f64]>| -> Vec<f64> {
        let mut acc: Vec<Acc> = y.iter().map(|&v| Acc::new(v)).collect();
        for (col, bj) in xcols.iter().zip(beta) {
            acc.iter_mut().zip(col).for_each(|(a, x)| a.add_prod(-x, *bj));
        }
        if let Some(less) = less {
            acc.iter_mut().zip(less).for_each(|(a, r)| a.add_prod(-1.0, *r));
        }
        acc.into_iter().map(Acc::value).collect()
    };
    let mut beta = back(&qty);
    // ITERATIVE REFINEMENT of the augmented system [I X; Xᵀ 0]·[r; β] = [y; 0] (Björck,
    // 1967): the residual r is refined alongside β, which is what removes the error term
    // that grows with the SIZE of the residual — plain refinement of β alone leaves it, and
    // NIST's large-residual Wampler 3–5 stayed at 5.8–9.8 digits under it. Both equations'
    // residuals are formed in compensated arithmetic; the corrections reuse the QR factors:
    // h = R⁻ᵀg, d = Qᵀf, δβ = R⁻¹(d₁ − h), δr = Q·[h; d₂]. Stopped once δβ stops mattering.
    let mut res = residual(&beta, None);
    for _ in 0..5 {
        // f = y − r − Xβ and g = −Xᵀr.
        // −rᵢ joins the SAME compensated sum: rounding yᵢ − Σxᵢⱼβⱼ to a double first and
        // subtracting rᵢ after would reintroduce an error of one ulp of the residual itself,
        // the very term this refinement exists to remove.
        let mut f = residual(&beta, Some(&res));
        let g: Vec<f64> = xcols
            .iter()
            .map(|col| {
                let mut a = Acc::new(0.0);
                col.iter().zip(&res).for_each(|(x, r)| a.add_prod(-x, *r));
                a.value()
            })
            .collect();
        let h = forward_t(&g);
        apply_qt(&mut f);
        let d1: Vec<f64> = (0..k).map(|j| f[j] - h[j]).collect();
        let dbeta = back(&d1);
        let mut dres = f;
        dres[..k].copy_from_slice(&h);
        apply_q(&mut dres);
        let small = dbeta.iter().zip(&beta).all(|(d, b)| d.abs() <= f64::EPSILON * b.abs());
        beta.iter_mut().zip(&dbeta).for_each(|(b, d)| *b += d);
        res.iter_mut().zip(&dres).for_each(|(r, d)| *r += d);
        if small {
            break;
        }
    }
    // diag((XᵀX)⁻¹) = diag(R⁻¹R⁻ᵀ): the squared row norms of R⁻¹. Each column of R⁻¹ is
    // R⁻¹eₖ by back-substitution, and its squares add into the rows it touches.
    let mut inv_diag = vec![0.0; k];
    for col in 0..k {
        let mut column = vec![0.0; col + 1];
        column[col] = 1.0 / r(col, col);
        for row in (0..col).rev() {
            let s: f64 = ((row + 1)..=col).map(|m| r(row, m) * column[m]).sum();
            column[row] = -s / r(row, row);
        }
        inv_diag.iter_mut().zip(&column).for_each(|(d, c)| *d += c * c);
    }
    // The final residuals as double-length pairs, so RSS is summed from what the residuals
    // ARE rather than from their roundings: on `y = [2, 4, 5, 4, 5]` against x = 1..5 the
    // rounded residuals' squares summed to one ulp under 2.4 and printed R² as
    // 0.6000000000000001 where the exact answer is 0.6.
    let pairs: Vec<(f64, f64)> = {
        let mut acc: Vec<Acc> = y.iter().map(|&v| Acc::new(v)).collect();
        for (col, bj) in xcols.iter().zip(&beta) {
            acc.iter_mut().zip(col).for_each(|(a, x)| a.add_prod(-x, *bj));
        }
        acc.into_iter().map(Acc::pair).collect()
    };
    let residuals: Vec<f64> = pairs.iter().map(|p| p.0).collect();
    let predictions: Vec<f64> = y.iter().zip(&residuals).map(|(v, r)| v - r).collect();
    let (rss, rss_lo) = {
        let mut a = Acc::new(0.0);
        pairs.iter().for_each(|&(hi, lo)| {
            a.add_prod(hi, hi);
            a.add_prod(2.0 * hi, lo);
        });
        a.pair()
    };
    // R²'s baseline is the model with no predictors: the mean when the fit has an
    // intercept, and ZERO when it is forced through the origin — the uncentered total,
    // as R, SAS, Stata and NIST's certified values define it. Centering a no-intercept
    // fit measured it against a model it cannot express, which can make R² negative.
    let tss: f64 = if intercept {
        let ybar = mean(y);
        y.iter().map(|v| (v - ybar).powi(2)).sum()
    } else {
        y.iter().map(|v| v * v).sum()
    };
    // The explained sum of squares, measured from the same baseline as `tss`. R² is computed
    // from whichever of rss and ess is SMALLER, the one the other's rounding cannot swamp:
    // when the fit explains little (NIST's Wampler5, R² ≈ 0.002), 1 − rss/tss subtracts two
    // nearly equal numbers (13.7 digits; ess/tss gives 14.8), and when it explains nearly
    // everything (Filip, R² ≈ 0.997), ess carries the residuals' error (8.1; 1 − rss/tss
    // gives 11.1). See `OlsCore::r_squared`.
    let ess: f64 = if intercept {
        let ybar = mean(y);
        (0..n).map(|i| ((y[i] - ybar) - residuals[i]).powi(2)).sum()
    } else {
        predictions.iter().map(|v| v * v).sum()
    };
    Some(OlsCore { beta, inv_diag, predictions, residuals, rss, rss_lo, tss, ess, k, n, intercept })
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
    let r_squared = c.r_squared();
    let (beta, inv_diag, rss, n, k) = (c.beta, c.inv_diag, c.rss, c.n, c.k);
    let df = (n - k) as f64;
    // The baseline model has one parameter with an intercept and none without (R's
    // `summary.lm` convention, matching the uncentered R² above).
    let base = if c.intercept { n - 1 } else { n };
    let adj_r_squared = 1.0 - (1.0 - r_squared) * base as f64 / df;
    let resid_var = rss / df;
    let mut std_errors = Vec::with_capacity(k);
    let mut p_values = Vec::with_capacity(k);
    for j in 0..k {
        let se = (resid_var * inv_diag[j]).sqrt();
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
    let r_squared = if c.tss == 0.0 { 0.0 } else { c.r_squared() };
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
    fn population_and_sample_estimates() {
        let xs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        assert!(approx(mean(&xs), 5.0));
        assert!(approx(population_variance(&xs), 4.0)); // population variance
        assert!(approx(population_std(&xs), 2.0));
        // The defining identity: var == std^2, for either estimate.
        assert!(approx(population_variance(&xs), population_std(&xs).powi(2)));
        assert!(approx(variance_ddof(&xs, 1), 32.0 / 7.0)); // the sample estimate, the default
        assert!(approx(variance_ddof(&xs, 1), std_ddof(&xs, 1).powi(2)));
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

    /// NIST StRD Wampler1: y = 1 + x + x² + x³ + x⁴ + x⁵ exactly, for x = 0..=20, so every
    /// certified coefficient is exactly 1 and every input is an exact double. Solving the
    /// normal equations got 6 digits of it (the squared condition number); Householder QR
    /// with augmented-system refinement gets all of them.
    #[test]
    fn the_nist_wampler1_polynomial_is_fitted_to_every_digit() {
        let x: Vec<f64> = (0..=20).map(f64::from).collect();
        let y: Vec<f64> = x.iter().map(|v| (0..=5).map(|p| v.powi(p)).sum()).collect();
        let cols: Vec<Vec<f64>> = (1..=5).map(|p| x.iter().map(|v| v.powi(p)).collect()).collect();
        let f = least_squares(&cols, &y, true).expect("a full-rank fit");
        for (j, b) in f.coefficients.iter().enumerate() {
            assert!((b - 1.0).abs() <= 4.0 * f64::EPSILON, "B{j} = {b:e}, certified 1");
        }
    }

    /// The rank test is relative to the data, not an absolute pivot: the same well-posed fit
    /// with x measured in units of 10⁻⁷ was refused as "collinear" by the normal equations'
    /// `1e-12` cutoff on XᵀX.
    #[test]
    fn a_fit_does_not_depend_on_the_units_of_x() {
        let x = [1.0, 2.0, 3.0, 4.5];
        let y = [1.0, 2.1, 2.9, 4.2];
        let tiny: Vec<f64> = x.iter().map(|v| v * 1e-7).collect();
        let a = multiple_regression(&[x.to_vec()], &y, true).expect("fit in plain units");
        let b = multiple_regression(&[tiny], &y, true).expect("the same fit in units of 1e-7");
        assert!(close(a.coefficients[0], b.coefficients[0], 1e-12));
        assert!(close(a.coefficients[1], b.coefficients[1] * 1e-7, 1e-12));
        assert!(close(a.r_squared, b.r_squared, 1e-12));
    }

    /// Through the origin, R² is measured against the zero model — the uncentered total
    /// sum of squares, as R, SAS, Stata and NIST define it. NIST StRD NoInt1's certified
    /// R² is 0.999365492298663; centering gave a different statistic (0 digits of it).
    #[test]
    fn a_fit_through_the_origin_reports_the_uncentered_r_squared() {
        let x: Vec<f64> = (60..=70).map(f64::from).collect();
        let y: Vec<f64> = (130..=140).map(f64::from).collect();
        let f = multiple_regression(&[x], &y, false).expect("a full-rank fit");
        assert!(close(f.coefficients[0], 2.07438016528926, 1e-13), "{}", f.coefficients[0]);
        assert!(close(f.std_errors[0], 0.165289256198347e-01, 1e-13), "{}", f.std_errors[0]);
        assert!(close(f.r_squared, 0.999365492298663, 1e-13), "{}", f.r_squared);
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

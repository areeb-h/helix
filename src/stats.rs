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
}

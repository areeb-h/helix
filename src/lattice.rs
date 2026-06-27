//! Lattice basis reduction — the LLL (Lenstra–Lenstra–Lovász) algorithm.
//!
//! Used for shortest-vector problems and **integer-relation detection**: e.g.
//! recovering the integer exponent vector `p` of a monomial conservation law
//! `∏ qᵢ^pᵢ = const`. That law is linear in log space (`Σ pᵢ ln qᵢ = 0`), so the
//! relation among the `ln qᵢ` surfaces as a *short vector* once the (scaled,
//! integerized) data is embedded as a lattice and reduced.
//!
//! Floating-point (`f64`) LLL, with **correctness favored over speed**: the
//! Gram–Schmidt basis is recomputed from scratch after every swap (size reduction
//! never changes it). That is `O(n⁴·m)` for an `n×m` basis — instant for the small
//! lattices this is for (a handful of quantities), and free of the bugs a hand-rolled
//! incremental GS update invites. For exact integer results keep entries below 2⁵³.

#![allow(clippy::needless_range_loop)] // linear algebra reads clearest indexed

/// Cap so a pathological call can't allocate unboundedly (`O(n·m + n²)` storage,
/// `O(n⁴·m)` time). Far above any real integer-relation lattice.
const MAX_DIM: usize = 1024;

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Gram–Schmidt orthogonalization of the rows of `b`. Returns `(mu, bnorm)` where
/// `mu[i][j]` (for `j < i`) is the GS coefficient and `bnorm[i] = |b*ᵢ|²`. A zero
/// `bnorm[j]` (a linearly dependent row) yields a zero coefficient rather than a
/// division by zero; callers reject rank-deficient input up front.
fn gram_schmidt(b: &[Vec<f64>]) -> (Vec<Vec<f64>>, Vec<f64>) {
    let n = b.len();
    let mut bstar: Vec<Vec<f64>> = Vec::with_capacity(n);
    let mut mu = vec![vec![0.0_f64; n]; n];
    let mut bnorm = vec![0.0_f64; n];
    for i in 0..n {
        let mut row = b[i].clone();
        for j in 0..i {
            let c = if bnorm[j] > 0.0 { dot(&b[i], &bstar[j]) / bnorm[j] } else { 0.0 };
            mu[i][j] = c;
            for t in 0..row.len() {
                row[t] -= c * bstar[j][t];
            }
        }
        bnorm[i] = dot(&row, &row);
        bstar.push(row);
    }
    (mu, bnorm)
}

/// Size-reduce row `k` against row `l` (`l < k`), updating `mu[k][*]`. Size reduction
/// does not change the GS orthogonalization, so `bnorm`/`mu[*][<k]` stay valid.
fn reduce(k: usize, l: usize, b: &mut [Vec<f64>], mu: &mut [Vec<f64>]) {
    if mu[k][l].abs() > 0.5 {
        let q = mu[k][l].round();
        let m = b[l].len();
        for t in 0..m {
            b[k][t] -= q * b[l][t];
        }
        mu[k][l] -= q;
        for j in 0..l {
            let mlj = mu[l][j];
            mu[k][j] -= q * mlj;
        }
    }
}

/// LLL-reduce a basis of `n` row vectors in `R^m` (`n ≤ m`, rows linearly
/// independent). `delta ∈ (0.25, 1.0]` is the Lovász parameter (0.75 is standard;
/// larger ⇒ more reduced but slower). Returns the reduced basis (the *same lattice*
/// — a unimodular transform — with short, nearly-orthogonal vectors), or a message
/// for a malformed, oversized, or rank-deficient basis.
pub fn lll(mut b: Vec<Vec<f64>>, delta: f64) -> Result<Vec<Vec<f64>>, String> {
    let n = b.len();
    if n == 0 {
        return Err("`lll` needs at least one basis vector".to_string());
    }
    let m = b[0].len();
    if m == 0 || b.iter().any(|v| v.len() != m) {
        return Err("`lll` needs non-empty basis vectors all of the same length".to_string());
    }
    if m < n {
        return Err(format!(
            "`lll` needs at most as many vectors as their dimension (got {n} vectors in dimension {m})"
        ));
    }
    if n > MAX_DIM || m > MAX_DIM {
        return Err(format!("`lll` basis is too large (max {MAX_DIM} in each dimension)"));
    }
    if !(0.25 < delta && delta <= 1.0) {
        return Err("`lll` delta must be in (0.25, 1.0] (0.75 is standard)".to_string());
    }
    if b.iter().flatten().any(|x| !x.is_finite()) {
        return Err("`lll` basis entries must be finite numbers".to_string());
    }

    let (mut mu, mut bnorm) = gram_schmidt(&b);
    // Rank check: a GS norm negligible relative to the basis scale means the rows
    // are linearly dependent — LLL is undefined, so reject it clearly.
    // Rank check relative to *each row's own* norm: a GS residual negligible
    // against the row itself means the row is (numerically) in the span of the
    // earlier ones. Using the row's own norm (not a global max) is essential -
    // an integer-relation lattice's meaningful short directions have tiny GS
    // norms by design and must NOT be flagged as dependent.
    let dependent = (0..n).any(|i| {
        let rn = dot(&b[i], &b[i]);
        rn <= 0.0 || bnorm[i] < 1e-24 * rn
    });
    if dependent {
        return Err("`lll` basis vectors must be linearly independent".to_string());
    }

    // LLL terminates in O(n² log B) swaps; this cap is a defensive backstop against
    // a float-instability cycle, not a real limit.
    let max_iters = 1000 + 100 * n * n;
    let mut iters = 0usize;
    let mut k = 1usize;
    while k < n {
        iters += 1;
        if iters > max_iters {
            return Err("`lll` did not converge (numerically unstable basis?)".to_string());
        }
        reduce(k, k - 1, &mut b, &mut mu);
        if bnorm[k] >= (delta - mu[k][k - 1] * mu[k][k - 1]) * bnorm[k - 1] {
            for l in (0..k - 1).rev() {
                reduce(k, l, &mut b, &mut mu);
            }
            k += 1;
        } else {
            b.swap(k, k - 1);
            let (nmu, nbnorm) = gram_schmidt(&b);
            mu = nmu;
            bnorm = nbnorm;
            k = (k - 1).max(1);
        }
    }
    Ok(b)
}

// ===========================================================================
// Exact integer LLL — the same algorithm with no floating point at all.
//
// The `f64` `lll` above is exact only while every intermediate stays below 2⁵³;
// for a lattice scaled past that (large embeddings, high-precision relations) the
// Gram–Schmidt accumulates rounding and the reduction can miss the true short
// vector. `lll_exact` runs entirely in `BigInt` lattice entries with a `BigRational`
// Gram–Schmidt, so it is correct to the last bit for an integer lattice of *any*
// magnitude — at the cost of arbitrary-precision arithmetic. Same structure as
// `lll` (GS recomputed after each swap) so the two are easy to cross-check.
// ===========================================================================

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::Zero;

fn dot_rat(a: &[BigRational], b: &[BigRational]) -> BigRational {
    let mut s = BigRational::zero();
    for (x, y) in a.iter().zip(b) {
        s += x * y;
    }
    s
}

/// Exact Gram–Schmidt of integer rows: `mu[i][j]` (`j < i`) is the rational GS
/// coefficient and `bnorm[i] = |b*ᵢ|²` is the exact squared GS norm (zero exactly
/// iff row `i` lies in the span of the earlier rows).
fn gram_schmidt_exact(b: &[Vec<BigInt>]) -> (Vec<Vec<BigRational>>, Vec<BigRational>) {
    let n = b.len();
    let mut bstar: Vec<Vec<BigRational>> = Vec::with_capacity(n);
    let mut mu = vec![vec![BigRational::zero(); n]; n];
    let mut bnorm = vec![BigRational::zero(); n];
    for i in 0..n {
        let bi: Vec<BigRational> = b[i].iter().map(|x| BigRational::from(x.clone())).collect();
        let mut row = bi.clone();
        for j in 0..i {
            let c = if bnorm[j].is_zero() {
                BigRational::zero()
            } else {
                dot_rat(&bi, &bstar[j]) / &bnorm[j]
            };
            for t in 0..row.len() {
                row[t] = &row[t] - &c * &bstar[j][t];
            }
            mu[i][j] = c;
        }
        bnorm[i] = dot_rat(&row, &row);
        bstar.push(row);
    }
    (mu, bnorm)
}

/// Exact size reduction of row `k` against row `l` (`l < k`): subtract the nearest
/// integer multiple so `|mu[k][l]| ≤ 1/2`. Mirrors [`reduce`] in `BigInt`/`BigRational`.
fn reduce_exact(k: usize, l: usize, b: &mut [Vec<BigInt>], mu: &mut [Vec<BigRational>]) {
    let half = BigRational::new(BigInt::from(1), BigInt::from(2));
    if mu[k][l] > half || mu[k][l] < -half.clone() {
        let q = mu[k][l].round().to_integer(); // nearest integer (half away from zero)
        for t in 0..b[l].len() {
            let prod = &q * &b[l][t];
            b[k][t] = &b[k][t] - &prod;
        }
        let qr = BigRational::from(q);
        mu[k][l] = &mu[k][l] - &qr;
        for j in 0..l {
            let mlj = mu[l][j].clone();
            mu[k][j] = &mu[k][j] - &qr * &mlj;
        }
    }
}

/// Exact f64 → BigRational (no rounding): decode the IEEE-754 fields so a `delta`
/// like `0.99` becomes its precise binary value, keeping the Lovász test exact.
fn rational_from_f64(x: f64) -> BigRational {
    let bits = x.to_bits();
    let sign: i64 = if bits >> 63 == 0 { 1 } else { -1 };
    let raw_exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & 0xf_ffff_ffff_ffff;
    let (mantissa, exp) = if raw_exp == 0 {
        (frac, -1074) // subnormal
    } else {
        (frac | 0x10_0000_0000_0000, raw_exp - 1075) // normal: add the implicit bit
    };
    let mant = BigInt::from(sign * mantissa as i64);
    if exp >= 0 {
        BigRational::from(mant * num_traits::pow(BigInt::from(2), exp as usize))
    } else {
        BigRational::new(mant, num_traits::pow(BigInt::from(2), (-exp) as usize))
    }
}

/// Exact LLL over an **integer** lattice. Same contract as [`lll`] but every entry
/// is a `BigInt`, so the result is correct regardless of scale. `delta ∈ (0.25, 1.0]`.
pub fn lll_exact(mut b: Vec<Vec<BigInt>>, delta: f64) -> Result<Vec<Vec<BigInt>>, String> {
    let n = b.len();
    if n == 0 {
        return Err("`lll_exact` needs at least one basis vector".to_string());
    }
    let m = b[0].len();
    if m == 0 || b.iter().any(|v| v.len() != m) {
        return Err("`lll_exact` needs non-empty basis vectors all of the same length".to_string());
    }
    if m < n {
        return Err(format!(
            "`lll_exact` needs at most as many vectors as their dimension (got {n} vectors in dimension {m})"
        ));
    }
    // Tighter than the f64 `MAX_DIM`: exact Gram–Schmidt numerators grow with the
    // dimension (Hadamard bound), so a high-dimensional integer lattice can blow up
    // memory/time far worse than the float path. Integer-relation detection needs
    // only a handful of dimensions, so 256 is generous while bounding the worst case.
    const MAX_EXACT_DIM: usize = 256;
    if n > MAX_EXACT_DIM || m > MAX_EXACT_DIM {
        return Err(format!(
            "`lll_exact` basis is too large (max {MAX_EXACT_DIM} in each dimension)"
        ));
    }
    if !(0.25 < delta && delta <= 1.0) {
        return Err("`lll_exact` delta must be in (0.25, 1.0] (0.75 is standard)".to_string());
    }
    let delta = rational_from_f64(delta);

    let (mut mu, mut bnorm) = gram_schmidt_exact(&b);
    // Exact rank check: a GS norm is exactly zero iff the row is dependent.
    if bnorm.iter().any(|x| x.is_zero()) {
        return Err("`lll_exact` basis vectors must be linearly independent".to_string());
    }

    // Exact LLL terminates in O(n² log B) swaps; this is a defensive backstop only.
    let max_iters = 1000 + 100 * n * n;
    let mut iters = 0usize;
    let mut k = 1usize;
    while k < n {
        iters += 1;
        if iters > max_iters {
            return Err("`lll_exact` did not converge".to_string());
        }
        reduce_exact(k, k - 1, &mut b, &mut mu);
        let mkk = mu[k][k - 1].clone();
        let bound = (&delta - &mkk * &mkk) * &bnorm[k - 1];
        if bnorm[k] >= bound {
            for l in (0..k - 1).rev() {
                reduce_exact(k, l, &mut b, &mut mu);
            }
            k += 1;
        } else {
            b.swap(k, k - 1);
            let (nmu, nbnorm) = gram_schmidt_exact(&b);
            mu = nmu;
            bnorm = nbnorm;
            k = (k - 1).max(1);
        }
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Squared lattice volume (∏ |b*ᵢ|²) — invariant under LLL's unimodular reduction,
    /// so it's a clean check that the reduced basis spans the same lattice.
    fn vol_sq(b: &[Vec<f64>]) -> f64 {
        gram_schmidt(b).1.iter().product()
    }

    /// Verify the basis is genuinely LLL-reduced: size-reduced and Lovász-satisfied.
    fn is_reduced(b: &[Vec<f64>], delta: f64) -> bool {
        let (mu, bnorm) = gram_schmidt(b);
        let n = b.len();
        for i in 0..n {
            for j in 0..i {
                if mu[i][j].abs() > 0.5 + 1e-9 {
                    return false;
                }
            }
        }
        for k in 1..n {
            if bnorm[k] < (delta - mu[k][k - 1] * mu[k][k - 1]) * bnorm[k - 1] - 1e-9 {
                return false;
            }
        }
        true
    }

    #[test]
    fn classic_example_preserves_volume_and_reduces() {
        let b = vec![
            vec![1.0, 1.0, 1.0],
            vec![-1.0, 0.0, 2.0],
            vec![3.0, 5.0, 6.0],
        ];
        let v0 = vol_sq(&b); // |det| = 3, so vol_sq = 9
        let r = lll(b, 0.75).unwrap();
        assert!((vol_sq(&r) - v0).abs() < 1e-6, "volume changed");
        assert!(is_reduced(&r, 0.75));
    }

    #[test]
    fn recovers_integer_relation() {
        // ln(6) = ln(2) + ln(3): the relation [1, 1, -1] among (ln2, ln3, ln6).
        let (l2, l3, l6) = (2.0_f64.ln(), 3.0_f64.ln(), 6.0_f64.ln());
        let scale = 1e9;
        let b = vec![
            vec![1.0, 0.0, 0.0, (scale * l2).round()],
            vec![0.0, 1.0, 0.0, (scale * l3).round()],
            vec![0.0, 0.0, 1.0, (scale * l6).round()],
        ];
        let r = lll(b, 0.99).unwrap();
        // some reduced vector's first three entries are ±[1,1,-1] with a ~0 residual
        let found = r.iter().any(|v| {
            let rel: Vec<i64> = v[0..3].iter().map(|x| x.round() as i64).collect();
            (rel == vec![1, 1, -1] || rel == vec![-1, -1, 1]) && v[3].abs() < 2.0
        });
        assert!(found, "integer relation not recovered: {r:?}");
    }

    #[test]
    fn identity_is_already_reduced() {
        let r = lll(vec![vec![1.0, 0.0], vec![0.0, 1.0]], 0.75).unwrap();
        assert!(is_reduced(&r, 0.75));
        assert!((vol_sq(&r) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_dependent_oversized_and_bad_delta() {
        assert!(lll(vec![vec![1.0, 2.0], vec![2.0, 4.0]], 0.75).is_err()); // dependent
        assert!(lll(vec![vec![1.0, 0.0], vec![0.0, 1.0]], 1.5).is_err()); // bad delta
        assert!(lll(vec![vec![1.0], vec![2.0]], 0.75).is_err()); // m < n
    }

    // ----- exact integer LLL -----

    fn ivec(xs: &[i64]) -> Vec<BigInt> {
        xs.iter().map(|&x| BigInt::from(x)).collect()
    }

    /// The relation `[1,1,-1]` among (a, b, c=a+b) is the lattice vector
    /// `[1,1,-1, 0]` — exact LLL finds it with a residual of EXACTLY zero (the f64
    /// version only gets "small").
    #[test]
    fn exact_recovers_relation_to_the_bit() {
        let b = vec![
            ivec(&[1, 0, 0, 3]),
            ivec(&[0, 1, 0, 5]),
            ivec(&[0, 0, 1, 8]), // 3 + 5 = 8
        ];
        let r = lll_exact(b, 0.99).unwrap();
        let want = ivec(&[1, 1, -1, 0]);
        let want_neg = ivec(&[-1, -1, 1, 0]);
        assert!(r.iter().any(|v| *v == want || *v == want_neg), "relation not exact: {r:?}");
    }

    /// The payoff: entries past 2⁵³ where f64 GS would lose bits. `c = a + b` with
    /// a, b ≈ 10¹⁷ ≫ 2⁵³ ≈ 9·10¹⁵; exact LLL still gives a zero-residual relation.
    #[test]
    fn exact_handles_entries_beyond_f64_precision() {
        let a: BigInt = "100000000000000003".parse().unwrap();
        let b_: BigInt = "200000000000000007".parse().unwrap();
        let c = &a + &b_; // exact; not representable in f64
        let basis = vec![
            vec![BigInt::from(1), BigInt::from(0), BigInt::from(0), a],
            vec![BigInt::from(0), BigInt::from(1), BigInt::from(0), b_],
            vec![BigInt::from(0), BigInt::from(0), BigInt::from(1), c],
        ];
        let r = lll_exact(basis, 0.99).unwrap();
        let want = ivec(&[1, 1, -1, 0]);
        let want_neg = ivec(&[-1, -1, 1, 0]);
        assert!(r.iter().any(|v| *v == want || *v == want_neg), "big-scale relation missed: {r:?}");
    }

    /// Exact LLL preserves the lattice (a unimodular transform) and is reduced —
    /// checked against the float `is_reduced`/`vol_sq` after a cast back.
    #[test]
    fn exact_classic_example_reduces() {
        use num_traits::ToPrimitive;
        let b = vec![ivec(&[1, 1, 1]), ivec(&[-1, 0, 2]), ivec(&[3, 5, 6])];
        let r = lll_exact(b, 0.75).unwrap();
        let rf: Vec<Vec<f64>> =
            r.iter().map(|row| row.iter().map(|x| x.to_f64().unwrap()).collect()).collect();
        assert!(is_reduced(&rf, 0.75));
        assert!((vol_sq(&rf) - 9.0).abs() < 1e-6); // |det| = 3
    }

    #[test]
    fn exact_rejects_dependent_and_bad_delta() {
        assert!(lll_exact(vec![ivec(&[1, 2]), ivec(&[2, 4])], 0.75).is_err()); // dependent
        assert!(lll_exact(vec![ivec(&[1, 0]), ivec(&[0, 1])], 1.5).is_err()); // bad delta
        assert!(lll_exact(vec![ivec(&[1]), ivec(&[2])], 0.75).is_err()); // m < n
    }
}

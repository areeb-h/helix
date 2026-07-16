// K8 — dense f64 matmul, naive ijk triple loop (Rust, std only).
//   rustc -O -C target-cpu=native k8_matmul.rs -o k8_matmul_rs
//
//   A[i][j] = ((i*j) % 100) * 0.5      B[i][j] = ((i+j) % 100) * 0.25
//   anchor  = C[0][0] + C[n-1][n-1] + C[n/2][n/2]   printed as %.6f
//
// Same algorithm, same ijk order, same materialized row-major arrays as the C file.
// Rust does not contract to FMA without explicit intrinsics, but it would not matter:
// every product and partial sum here is an exact multiple of 1/8 well inside f64's
// mantissa, so the anchor is accumulation-order- and contraction-independent.
//
// Indexing is via `a[i * n + k]` (bounds-checked) rather than get_unchecked — the C
// file is likewise plain indexing, and using unsafe here would be a thumb on the scale.
use std::env;

fn main() {
    let n: usize = env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);
    assert!(n > 0, "n must be positive");

    let mut a = vec![0.0f64; n * n];
    let mut b = vec![0.0f64; n * n];
    let mut c = vec![0.0f64; n * n];

    for i in 0..n {
        for j in 0..n {
            a[i * n + j] = ((i * j) % 100) as f64 * 0.5;
            b[i * n + j] = ((i + j) % 100) as f64 * 0.25;
        }
    }

    // naive ijk: inner loop strides B by n (cache-hostile), matching every sibling.
    for i in 0..n {
        for j in 0..n {
            let mut s = 0.0f64;
            for k in 0..n {
                s += a[i * n + k] * b[k * n + j];
            }
            c[i * n + j] = s;
        }
    }

    let anchor = c[0] + c[(n - 1) * n + (n - 1)] + c[(n / 2) * n + (n / 2)];
    println!("{:.6}", anchor);
}

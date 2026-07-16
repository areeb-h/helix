// K3 — Basel series, Rust. rustc -O -C target-cpu=native k3_basel.rs -o k3_basel
//
// Naive left-to-right f64 summation. Rust has no fast-math, so `s += ...` in a loop is
// never reassociated or vectorized — the accumulation stays serial, matching C exactly.
//
// `{:.17}` on an f64 is Rust's exact correctly-rounded 17-decimal expansion, which is
// byte-identical to C's "%.17f" for this value (verified by running).
fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000_000);
    let mut s: f64 = 0.0;
    for k in 1..=n {
        let x = k as f64;
        s += 1.0 / (x * x);
    }
    println!("{:.17}", s);
}

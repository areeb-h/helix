// K4 — all-pairs Manhattan distance sum, Rust.
//   rustc -O -C target-cpu=native k4_allpairs.rs -o k4_allpairs
// N comes from argv[1] (default 15000) so the compiler cannot precompute the sum.
fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .map(|s| s.parse().expect("N must be an integer"))
        .unwrap_or(15000);

    let pts: Vec<i64> = (0..n).map(|i| (i * 2654435761) % 1000).collect();

    let mut total: i64 = 0;
    for i in 0..n as usize {
        for j in (i + 1)..n as usize {
            total += (pts[i] - pts[j]).abs();
        }
    }
    println!("{}", total);
}

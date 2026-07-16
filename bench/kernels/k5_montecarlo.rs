// K5 — Monte Carlo pi, Rust. The xorshift64 reference stream (u64, native
// unsigned shifts). Anchor = raw hit count.
//   rustc -O -C target-cpu=native k5_montecarlo.rs -o k5_montecarlo
// `s << 13` on u64 would panic on overflow in debug; `wrapping_shl` is not
// needed because shifting bits OUT of a u64 is not an overflow in Rust — only a
// shift AMOUNT >= 64 is. The literal amounts here are all < 64.
// Rust/LLVM never contracts `x*x + y*y` into an FMA without explicit opt-in, so
// the rounding matches C-with-contraction-off.

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .map(|a| a.parse().expect("N must be a non-negative integer"))
        .unwrap_or(100_000_000);

    let mut s: u64 = 88172645463325252;
    let mut hits: u64 = 0;
    for _ in 0..n {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let x = (s >> 11) as f64 / 9007199254740992.0; // 2^53 — exact
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let y = (s >> 11) as f64 / 9007199254740992.0;
        if x * x + y * y < 1.0 {
            hits += 1;
        }
    }
    println!("{}", hits);
}

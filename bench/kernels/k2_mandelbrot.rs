// K2 — escape-time Mandelbrot, 1200x1200 grid, max 100 iterations, over
// x in [-2.1, 0.6] and y in [-1.2, 1.2]. Anchor = sum of every pixel's iteration count.
//
//   rustc -O -C target-cpu=native k2_mandelbrot.rs -o k2_mandelbrot
//
// No FMA contraction here: rustc/LLVM compiles Rust float arithmetic with
// fp-contract=off, so `2.0*zr*zi + ci` stays a separate mul and add even under
// -C target-cpu=native (confirmed: zero FMA instructions in the emitted binary).
// C is built with an explicit -ffp-contract=off to hold it to the same rounding
// sequence; see the note in k2_mandelbrot.c for what that does and does not buy.
fn main() {
    let g: i64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1200);
    let mut total: i64 = 0;
    for y in 0..g {
        let ci = -1.2 + y as f64 * 2.4 / g as f64;
        for x in 0..g {
            let cr = -2.1 + x as f64 * 2.7 / g as f64;
            let mut zr = 0.0f64;
            let mut zi = 0.0f64;
            let mut i: i64 = 0;
            while i < 100 && zr * zr + zi * zi <= 4.0 {
                let t = zr * zr - zi * zi + cr;
                zi = 2.0 * zr * zi + ci;
                zr = t;
                i += 1;
            }
            total += i;
        }
    }
    println!("{}", total);
}

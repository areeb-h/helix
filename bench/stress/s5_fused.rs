// S5 — fused polynomial pipeline, Rust. rustc -O s5_fused.rs -o s5_fused
fn poly(x: i64) -> i64 {
    ((x % 97) * 31 + (x % 13) * 7 + 3) % 1000
}

fn main() {
    let n: i64 = 50_000_000;
    let mut total: i64 = 0;
    for x in 0..n {
        let p = poly(x);
        if p % 2 == 0 {
            total += p;
        }
    }
    println!("{}", total);
}

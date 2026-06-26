// S4 — pattern matching, Rust. rustc -O s4_match.rs -o s4_match
fn weight(m: i64) -> i64 {
    match m {
        0 => 7,
        1 | 2 | 3 => 2,
        11 => 5,
        x if x > 7 => 3,
        _ => 1,
    }
}

fn main() {
    let n: i64 = 20_000_000;
    let mut s: i64 = 0;
    for k in 0..n {
        s += weight(k % 12);
    }
    println!("{}", s);
}

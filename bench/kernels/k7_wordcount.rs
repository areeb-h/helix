// K7 — word frequency over a generated corpus, Rust (std only).
// `HashMap<String, i64>` + the `entry` API: the idiom every Rust user writes. Its default
// hasher is SipHash-1-3 (DoS-resistant, slower than C's FNV), and `entry` allocates a
// String per word even on a hit — both are the cost of the idiom, and both are honest.
// rustc -O -C target-cpu=native k7_wordcount.rs
//
// Corpus = the k5 xorshift64 reference stream (seed 88172645463325252), one draw per
// word, bit-identical in all six languages. See k7_wordcount.c for why the previous
// `(i*2654435761) % 10000` generator was a non-test.
// `s << 13` on u64 is not an overflow in Rust — only a shift AMOUNT >= 64 is — so no
// `wrapping_shl` is needed. The `& 9007199254740991` is a no-op on u64 (s>>11 is already
// 53 bits); it is spelled out to match Helix, where it is load-bearing.
use std::collections::HashMap;

const DEFAULT_N: i64 = 5_000_000;
const MODULUS: i64 = 10_000;

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_N);

    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut s: u64 = 88172645463325252;
    for _ in 0..n {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let id = ((s >> 11) & 9007199254740991) as i64 % MODULUS;
        let w = format!("w{}", id);
        *counts.entry(w).or_insert(0) += 1;
    }

    // `unwrap_or(0)` is the N=0 case: an empty corpus prints 0.
    let max = counts.values().copied().max().unwrap_or(0);
    println!("{}", max);
}

// K6 — count the primes below N (default 10,000,000). Anchor 664579 = pi(10^7).
// Sieve of Eratosthenes over a byte array (Vec<bool> is one byte per element).
//   rustc -O -C target-cpu=native k6_sieve.rs -o k6_sieve
// N comes from argv so the compiler cannot precompute the answer.
//
// FAIRNESS — the Helix column does not compile a sieve at all: it calls a native
// `primes()` builtin (see k6_sieve.helix), which is this same byte-array algorithm
// in Rust. Helix's 0.020 s ties this column's 0.020 s for exactly that reason, NOT
// because Helix compiles this loop. The pure-Helix version is k6_sieve_trial.helix,
// at 97 s.

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000);
    if n <= 2 {
        println!("0");
        return;
    }
    let n = n as usize;

    // composite[p] marks p as composite. Counts primes p with 2 <= p < n.
    let mut composite = vec![false; n];
    let mut count: i64 = 0;
    for p in 2..n {
        if !composite[p] {
            count += 1;
            // When p*p >= n the loop body never runs.
            let mut m = p * p;
            while m < n {
                composite[m] = true;
                m += p;
            }
        }
    }

    println!("{}", count);
}

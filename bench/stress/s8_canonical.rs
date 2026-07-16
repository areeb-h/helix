// S8 — canonical k-mer spectrum, Rust (std only). 2-bit rolling code; the reverse
// complement is computed on the packed code (complement = base XOR 3, reversed),
// and canonical = min(code, revcomp_code) — which equals the lexicographically
// smaller string because the code is big-endian (first base = high bits).
// rustc -O s8_canonical.rs -o s8_canonical
use std::collections::HashMap;
use std::fs;

const K: usize = 10;

fn revcomp(mut code: u64, k: usize) -> u64 {
    let mut rc: u64 = 0;
    for _ in 0..k {
        rc = (rc << 2) | (3 - (code & 3)); // complement of a 2-bit base = 3 - base
        code >>= 2;
    }
    rc
}

fn main() {
    let raw = fs::read_to_string("data/genome.fa").unwrap();
    let mut seq: Vec<u8> = Vec::with_capacity(raw.len());
    for line in raw.lines() {
        if !line.starts_with('>') {
            seq.extend(line.bytes());
        }
    }

    let mask: u64 = (1u64 << (2 * K)) - 1;
    let mut code: u64 = 0;
    let mut counts: HashMap<u64, u32> = HashMap::new();
    for (i, &b) in seq.iter().enumerate() {
        let two = match b {
            b'A' => 0u64,
            b'C' => 1,
            b'G' => 2,
            b'T' => 3,
            _ => continue,
        };
        code = ((code << 2) | two) & mask;
        if i + 1 >= K {
            let rc = revcomp(code, K);
            let canon = code.min(rc);
            *counts.entry(canon).or_insert(0) += 1;
        }
    }

    let total: u64 = counts.values().map(|&c| c as u64).sum();
    let max = counts.values().copied().max().unwrap_or(0);
    println!("distinct={}", counts.len());
    println!("total={}", total);
    println!("max={}", max);
}

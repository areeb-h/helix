// S2 — FASTQ GC + Phred, Rust (std only). rustc -O s2_fastq.rs -o s2_fastq
use std::fs;

fn main() {
    let raw = fs::read_to_string("data/reads.fq").unwrap();
    let lines: Vec<&str> = raw.lines().collect();

    let mut n: u64 = 0;
    let mut bases: u64 = 0;
    let mut gcsum = 0.0f64;
    let mut qsum = 0.0f64;
    let mut hiq: u64 = 0;

    let mut i = 0;
    while i + 3 < lines.len() {
        let seq = lines[i + 1];
        let qual = lines[i + 3];
        i += 4;
        let l = seq.len() as f64;
        n += 1;
        bases += seq.len() as u64;
        let gc = seq.bytes().filter(|&b| b == b'G' || b == b'C').count() as f64;
        gcsum += gc / l;
        let qs007: u64 = qual.bytes().map(|b| (b as u64) - 33).sum();
        let mq = qs007 as f64 / l;
        qsum += mq;
        if mq >= 30.0 {
            hiq += 1;
        }
    }

    println!("reads={}", n);
    println!("bases={}", bases);
    println!("gc4={}", (gcsum / n as f64 * 10000.0 + 0.5).floor() as i64);
    println!("q4={}", (qsum / n as f64 * 10000.0 + 0.5).floor() as i64);
    println!("hiq={}", hiq);
}

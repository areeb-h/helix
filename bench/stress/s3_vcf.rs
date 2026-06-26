// S3 — VCF analysis, Rust (std only). rustc -O s3_vcf.rs -o s3_vcf
use std::collections::HashMap;
use std::fs;

fn main() {
    let raw = fs::read_to_string("data/variants.vcf").unwrap();
    let mut total = 0u64;
    let mut keep = 0u64;
    let mut counts: HashMap<String, u64> = HashMap::new();

    for line in raw.lines() {
        if line.starts_with('#') {
            continue;
        }
        total += 1;
        let c: Vec<&str> = line.split('\t').collect();
        let qual: f64 = c[5].parse().unwrap();
        if qual > 50.0 {
            keep += 1;
            for kv in c[7].split(';') {
                if let Some(rest) = kv.strip_prefix("GENE=") {
                    *counts.entry(rest.to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    println!("total={}", total);
    println!("pass={}", keep);
    for g in ["BRCA1", "BRCA2", "EGFR", "KRAS", "TP53"] {
        println!("{}={}", g, counts.get(g).copied().unwrap_or(0));
    }
}

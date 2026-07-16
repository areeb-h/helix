// S7 — local pairwise alignment, Rust (std only). Score-only Gotoh affine DP,
// replicating src/align.rs local mode. rustc -O s7_align.rs -o s7_align
use std::fs;

const NEG: i32 = i32::MIN / 4;
const MATCH: i32 = 1;
const MIS: i32 = -1;
const GEXT: i32 = -1;
const FG: i32 = -6; // gap_open + gap_extend

fn local_score(x: &[u8], y: &[u8]) -> i32 {
    let m = y.len();
    let mut prev_mm = vec![NEG; m + 1];
    let mut prev_xx = vec![NEG; m + 1];
    let mut prev_yy = vec![NEG; m + 1];
    prev_mm[0] = 0;
    for j in 1..=m {
        prev_yy[j] = (prev_mm[j - 1] + FG).max(prev_yy[j - 1] + GEXT);
    }
    let mut best = 0;
    let mut cur_mm = vec![NEG; m + 1];
    let mut cur_xx = vec![NEG; m + 1];
    let mut cur_yy = vec![NEG; m + 1];
    for &xi in x {
        cur_mm[0] = NEG;
        cur_xx[0] = (prev_mm[0] + FG).max(prev_xx[0] + GEXT);
        cur_yy[0] = NEG;
        for j in 1..=m {
            let s = if xi == y[j - 1] { MATCH } else { MIS };
            let d = prev_mm[j - 1].max(prev_xx[j - 1]).max(prev_yy[j - 1]);
            let mut mval = d + s;
            if mval < 0 {
                mval = 0;
            }
            cur_mm[j] = mval;
            if mval > best {
                best = mval;
            }
            cur_xx[j] = (prev_mm[j] + FG).max(prev_xx[j] + GEXT);
            cur_yy[j] = (cur_mm[j - 1] + FG).max(cur_yy[j - 1] + GEXT);
        }
        std::mem::swap(&mut prev_mm, &mut cur_mm);
        std::mem::swap(&mut prev_xx, &mut cur_xx);
        std::mem::swap(&mut prev_yy, &mut cur_yy);
    }
    best
}

fn read_fasta(path: &str) -> Vec<Vec<u8>> {
    let raw = fs::read_to_string(path).unwrap();
    let mut seqs = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for line in raw.lines() {
        if line.starts_with('>') {
            if !cur.is_empty() {
                seqs.push(std::mem::take(&mut cur));
            }
        } else {
            cur.extend(line.bytes());
        }
    }
    if !cur.is_empty() {
        seqs.push(cur);
    }
    seqs
}

fn main() {
    let ref_seqs = read_fasta("data/ref.fa");
    let reference = &ref_seqs[0];
    let mut total: i64 = 0;
    let mut hits: i64 = 0;
    let mut max = 0;
    for q in read_fasta("data/queries.fa") {
        let s = local_score(&q, reference);
        total += s as i64;
        if s >= 40 {
            hits += 1;
        }
        if s > max {
            max = s;
        }
    }
    println!("total={}", total);
    println!("hits={}", hits);
    println!("max={}", max);
}

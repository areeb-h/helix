//! A minimal homegrown columnar engine — just the hot inner loops of the verbs Helix
//! actually uses (filter, group-by aggregate, sort), rayon-parallel, no Arrow, no async,
//! no cloud. Two typed columns are enough to measure whether the *approach* can be
//! competitive with Polars; a production version would generalize the column type.

use rayon::prelude::*;

pub struct Frame {
    pub group: Vec<i64>,
    pub value: Vec<f64>,
}

impl Frame {
    pub fn nrows(&self) -> usize {
        self.value.len()
    }

    /// Keep rows where `value > threshold` (predicate filter), gathering both columns.
    pub fn filter_gt(&self, threshold: f64) -> Frame {
        let keep: Vec<u32> = (0..self.nrows() as u32)
            .into_par_iter()
            .filter(|&i| self.value[i as usize] > threshold)
            .collect();
        let group = keep.par_iter().map(|&i| self.group[i as usize]).collect();
        let value = keep.par_iter().map(|&i| self.value[i as usize]).collect();
        Frame { group, value }
    }

    /// Group by `group` (dense ids `0..ngroups`) and sum `value` — parallel fold into
    /// per-thread accumulators, then merge.
    pub fn group_sum(&self, ngroups: usize) -> (Vec<i64>, Vec<f64>) {
        let sums = (0..self.nrows())
            .into_par_iter()
            .fold(
                || vec![0.0f64; ngroups],
                |mut acc, i| {
                    acc[self.group[i] as usize] += self.value[i];
                    acc
                },
            )
            .reduce(
                || vec![0.0f64; ngroups],
                |mut a, b| {
                    for i in 0..ngroups {
                        a[i] += b[i];
                    }
                    a
                },
            );
        ((0..ngroups as i64).collect(), sums)
    }

    /// Sort all rows by `value` descending (parallel argsort + gather).
    pub fn sort_by_value_desc(&self) -> Frame {
        let mut idx: Vec<u32> = (0..self.nrows() as u32).collect();
        idx.par_sort_unstable_by(|&a, &b| {
            self.value[b as usize]
                .partial_cmp(&self.value[a as usize])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let group = idx.par_iter().map(|&i| self.group[i as usize]).collect();
        let value = idx.par_iter().map(|&i| self.value[i as usize]).collect();
        Frame { group, value }
    }
}

/// Deterministic pseudo-random data (xorshift) — no `rand` dependency.
pub fn gen(n: usize, ngroups: usize) -> (Vec<i64>, Vec<f64>) {
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut group = Vec::with_capacity(n);
    let mut value = Vec::with_capacity(n);
    for _ in 0..n {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        group.push((s % ngroups as u64) as i64);
        value.push((s >> 11) as f64 / (1u64 << 53) as f64); // [0, 1)
    }
    (group, value)
}

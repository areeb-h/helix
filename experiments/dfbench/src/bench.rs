//! Head-to-head: homegrown columnar engine vs Polars, on filter / group-by-sum / sort,
//! across data sizes. Prints best-of-N wall-clock for each, and the ratio.

mod engine;

use engine::{gen, Frame};
use polars::prelude::col as pcol;
use polars::prelude::*;
use std::time::Instant;

/// Best-of `iters` wall-clock in ms. The closure returns a checksum to defeat
/// dead-code elimination.
fn best_ms<F: FnMut() -> u64>(iters: usize, mut f: F) -> f64 {
    let mut best = f64::MAX;
    let mut sink = 0u64;
    for _ in 0..iters {
        let t = Instant::now();
        sink = sink.wrapping_add(f());
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms < best {
            best = ms;
        }
    }
    std::hint::black_box(sink);
    best
}

fn main() {
    let ngroups = 100usize;
    let iters = 5;
    println!(
        "{:>10}  {:>10}  {:>12}  {:>12}  {:>8}",
        "rows", "op", "homegrown", "polars", "hg/polars"
    );
    println!("{}", "-".repeat(60));

    for &n in &[100_000usize, 1_000_000, 5_000_000] {
        let (group, value) = gen(n, ngroups);
        let hg = Frame { group: group.clone(), value: value.clone() };
        // Polars columns are Arc-backed, so `df.clone()` per iteration is a cheap refcount.
        let df = DataFrame::new_infer_height(vec![
            Column::new("group".into(), &group),
            Column::new("value".into(), &value),
        ])
        .unwrap();

        // --- filter: value > 0.5 ---
        let h = best_ms(iters, || hg.filter_gt(0.5).nrows() as u64);
        let p = best_ms(iters, || {
            df.clone()
                .lazy()
                .filter(pcol("value").gt(lit(0.5)))
                .collect()
                .unwrap()
                .height() as u64
        });
        report(n, "filter", h, p);

        // --- group by group, sum value ---
        let h = best_ms(iters, || hg.group_sum(ngroups).1.len() as u64);
        let p = best_ms(iters, || {
            df.clone()
                .lazy()
                .group_by([pcol("group")])
                .agg([pcol("value").sum()])
                .collect()
                .unwrap()
                .height() as u64
        });
        report(n, "group_sum", h, p);

        // --- sort by value descending ---
        let h = best_ms(iters, || hg.sort_by_value_desc().value[0] as u64 + 1);
        let p = best_ms(iters, || {
            df.clone()
                .lazy()
                .sort_by_exprs(
                    [pcol("value")],
                    SortMultipleOptions::default().with_order_descending(true),
                )
                .collect()
                .unwrap()
                .height() as u64
        });
        report(n, "sort", h, p);
        println!();
    }
}

fn report(n: usize, op: &str, h: f64, p: f64) {
    println!(
        "{:>10}  {:>10}  {:>10.2}ms  {:>10.2}ms  {:>7.2}x",
        n, op, h, p, h / p
    );
}

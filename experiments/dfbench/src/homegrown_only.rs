//! Exercises only the homegrown engine (no Polars references), so the linker strips
//! Polars and the resulting binary measures the homegrown footprint.

mod engine;

use engine::{gen, Frame};

fn main() {
    let (group, value) = gen(1_000_000, 100);
    let f = Frame { group, value };
    let filtered = f.filter_gt(0.5).nrows();
    let groups = f.group_sum(100).1.len();
    let sorted = f.sort_by_value_desc().nrows();
    println!("{filtered} {groups} {sorted}");
}

// K1 — 50M-element i64 dot product, Rust.  rustc -O -C target-cpu=native k1_dot.rs -o k1_dot
//
// ### WHAT THIS KERNEL ACTUALLY MEASURES — read before quoting any number ###
// It is named for the dot product, but in the C ref the dot is ~4-5% of the
// runtime (0.017-0.022 s of 0.44 s); ~95% is building the two 400 MB arrays, and
// most of THAT is the kernel faulting in and zeroing fresh anonymous pages — not
// arithmetic, and NOT DRAM bandwidth. This kernel ranks ALLOCATOR PAGE
// BEHAVIOUR. k1_dot.c carries the full writeup (phase split, THP finding,
// method, box); it applies to this file too.
//
// WHY THIS FILE NEEDS A TUNABLE TO BE FAIR. Rust's default global allocator is
// the system malloc — here glibc's — which does not ask for huge pages. THP
// policy on the reference box is madvise-only, so glibc-backed arrays get 4 KB
// pages while Helix's mimalloc arenas get huge pages. Measured at N=50M, peak
// AnonHugePages: Rust 0 MB vs Helix 643-778 MB; minor faults: Rust 195,400 vs
// Helix 117,527-133,000. Equalizing with GLIBC_TUNABLES=glibc.malloc.hugetlb=1,
// no source change:
//     Rust   0.44 s -> 0.12 s   (minor faults 195,400 -> ~1,220)
//     C      0.44 s -> 0.10 s
//     Go     0.43 s -> 0.48 s   (unchanged — Go's allocator has no such knob)
//     Helix  0.22 s at 267% CPU (unchanged — it already had huge pages)
// As published this kernel shows Helix 2.0x faster than Rust; equalized, Rust is
// ~1.8x faster than Helix at ~1/5 the CPU (Rust 0.12 s x 108% = 0.13 core-s vs
// Helix 0.22 s x 267% = 0.59 core-s, so ~4.5x better per core). run.sh MUST
// export that tunable (it does today — keep it). Go has NO equivalent knob and
// stays small-page, so the Go column is not page-size-comparable — that must be
// disclosed, not hidden. (NumPy needs no tunable: it madvises its own huge pages
// already. See k1_dot.c item 5 for the measured per-ref effect.)
//
// Note this file could equalize itself in-source (e.g. a huge-page-requesting
// #[global_allocator], or Vec::with_capacity + madvise) — it deliberately does
// NOT: the point of the ref is idiomatic Rust with the default allocator, so the
// page-size gap is disclosed and equalized from the harness instead.
//
// wrapping_mul/wrapping_add make the two's-complement contract explicit (a
// release build already wraps, but spelling it out keeps the kernel honest and
// identical under `-C debug-assertions=on`).
fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .map(|s| s.parse().expect("N must be an integer"))
        .unwrap_or(50_000_000);
    assert!(n >= 0, "N must be >= 0");

    let a: Vec<i64> = (0..n).map(|j| j % 97).collect();
    let b: Vec<i64> = (0..n).map(|j| j % 89).collect();

    let mut total: i64 = 0;
    for j in 0..n as usize {
        total = total.wrapping_add(a[j].wrapping_mul(b[j]));
    }

    println!("{}", total);
}

// K1 — 50M-element i64 dot product, C.  gcc -O3 -march=native -fwrapv k1_dot.c -o k1_dot
//
// ############################################################################
// ### WHAT THIS KERNEL ACTUALLY MEASURES — read before quoting any number ###
// ### This file carries the full writeup; the other k1_* files point here.  ###
// ############################################################################
//
// 1. THE DOT PRODUCT IS ~4-5% OF THE RUNTIME. This kernel is named for an
//    operation it barely measures. A phase-instrumented COPY of this kernel
//    (same code, clock_gettime around each phase), N=50M, 5 runs:
//        reduce (the actual dot)  0.017-0.022 s   <- the named operation
//        warm rewrite of a,b      0.064-0.071 s   <- identical stores, pages resident
//        cold first-touch build   0.386-0.581 s   <- the same stores, fresh pages
//    This file's own whole run is 0.44 s (min-of-9), user 0.09 s / sys 0.39 s.
//    So the reduce is ~4-5% of the run and ~95% is building the arrays.
//
// 2. "MEMORY-BOUND / RANKS DRAM BANDWIDTH" IS FALSE — it ranks page faults.
//    The load-bearing evidence is this file's own user/sys split: system time is
//    81% of its CPU time (sys 0.39 s vs user 0.09 s). That is the kernel
//    faulting in and zeroing 800 MB of fresh anonymous pages, not this file's
//    arithmetic. Turning on huge pages (item 4) collapses sys 0.39 s -> 0.05 s
//    and the whole run 0.44 s -> 0.10 s, which a DRAM-bandwidth-bound kernel
//    could not do: the bytes moved are identical either way.
//    Corroborating, from the instrumented copy: the reduce streams 800 MB at
//    37-48 GB/s and the warm rewrite writes it at 11-13 GB/s, but the COLD build
//    of those SAME bytes manages only 1.4-2.1 GB/s. Cold-minus-warm says 82-88%
//    of the build is page-fault + page-zeroing, not memory traffic.
//
// 3. THEREFORE IT RANKS ALLOCATORS, NOT LANGUAGES. THP policy on the reference
//    box is madvise-only (/sys/kernel/mm/transparent_hugepage/enabled reads
//    "always [madvise] never"), so a region is backed by huge pages ONLY if its
//    allocator asks for them. Helix ships mimalloc (Cargo.toml:153,
//    src/main.rs:62-63), whose arenas ask; NumPy madvises its large allocations
//    and asks too; glibc malloc (C, Rust, CPython) and Go's allocator do not.
//    Measured at N=50M — peak AnonHugePages: Helix 643-778 MB, NumPy 776 MB,
//    C 0 MB, Rust 0 MB, Go 0 MB, CPython 0 MB. Minor faults: C 195,389,
//    Rust 195,400, Go 196,005, CPython 196,478, Helix 117,527-133,000,
//    NumPy 5,285. The refs were sorted into two page-size classes by their
//    allocators and the scoreboard read that as a language result. Helix's
//    standing over C/Rust/Go here is an allocator artifact, not codegen.
//
// 4. EQUALIZING PAGE SIZE INVERTS THE RESULT. GLIBC_TUNABLES=glibc.malloc.hugetlb=1
//    gives glibc's malloc the same huge pages with NO source change:
//        C      0.44 s -> 0.10 s   (sys 0.39 -> 0.05 s; minor faults 195,389 -> 1,211)
//        Rust   0.44 s -> 0.12 s
//        Go     0.43 s -> 0.48 s   (unchanged — Go's allocator has no such knob)
//        Helix  0.22 s at 267% CPU (unchanged — it already had huge pages)
//    As published this kernel shows Helix 2.0x FASTER than C. Page-size-equalized
//    it shows C 2.2x faster than Helix, and on ~1/6 the CPU: C 0.10 s x 103% =
//    0.10 core-s vs Helix 0.22 s x 267% = 0.59 core-s, so C is ~5.7x better per
//    core. The published form overstates Helix-vs-C by ~4.4x.
//
// 5. WHAT run.sh MUST DO. Export GLIBC_TUNABLES=glibc.malloc.hugetlb=1, or the
//    comparison is not page-size-fair. (run.sh does this today — keep it.)
//    Exporting it process-wide is correct and sufficient; measured effect per ref:
//      C, Rust    : decisive (0.44 s -> 0.10/0.12 s). This is the point of it.
//      CPython    : real but irrelevant (faults 196,478 -> 60,322; wall 10.34 ->
//                   10.04 s, noise — interpreter overhead swamps it). Harmless.
//      NumPy      : no effect; it already madvises its own huge pages.
//      Helix      : no effect; mimalloc already has them.
//      Go         : NO EFFECT AND NO FIX — see k1_dot.go. Go does not allocate
//                   through glibc malloc, so the tunable cannot reach it and the
//                   Go column ships small-page. It is therefore NOT
//                   page-size-comparable to the equalized C/Rust columns, and
//                   that must be disclosed wherever the Go number is shown.
//    (The alternative — setting THP policy to "always" system-wide — would
//    equalize Go too, but changes the machine out from under every other kernel
//    in the suite.)
//
// METHOD/BOX: AMD Ryzen 7 7700X, 6 cores visible, 12 GB, Linux
// 6.6.87.2-microsoft-standard-WSL2, THP=madvise, gcc 13.3.0, rustc 1.97.0,
// go 1.26.1, CPython 3.12.3, NumPy 2.4.3. Wall times are MIN-of-9 interleaved
// runs at N=50M on an otherwise-idle box. Treat them as approximate: this kernel
// is fault-bound, so its wall time tracks the kernel's free-page supply and it
// is genuinely noisy (the same numbers taken under load ran ~5-20% slower and
// the cold-build phase inflated several-fold). The fault counts, AnonHugePages
// and RSS figures are load-insensitive and are the load-bearing evidence.
//
// -fwrapv: signed overflow wraps two's-complement (defined), matching Go's
// int64, Rust's wrapping_*, and Helix's i64. At N=50M nothing actually
// overflows (max term 96*88=8448), so -fwrapv does not change the anchor here;
// it is what makes the WRAPPING CONTRACT hold identically at larger N.
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>

int main(int argc, char **argv) {
    int64_t n = (argc > 1) ? strtoll(argv[1], NULL, 10) : 50000000;
    if (n < 0) { fprintf(stderr, "N must be >= 0\n"); return 1; }

    int64_t *a = malloc((size_t)n * sizeof(int64_t));
    int64_t *b = malloc((size_t)n * sizeof(int64_t));
    if ((a == NULL || b == NULL) && n > 0) { fprintf(stderr, "oom\n"); return 1; }

    for (int64_t j = 0; j < n; j++) {
        a[j] = j % 97;
        b[j] = j % 89;
    }

    int64_t total = 0;
    for (int64_t j = 0; j < n; j++) {
        total += a[j] * b[j];
    }

    printf("%lld\n", (long long)total);
    free(a);
    free(b);
    return 0;
}

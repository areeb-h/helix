// K1 — 50M-element i64 dot product, Go.  go build k1_dot.go
//
// ### WHAT THIS KERNEL ACTUALLY MEASURES — read before quoting any number ###
// It is named for the dot product, but in the C ref the dot is ~4-5% of the
// runtime (0.017-0.022 s of 0.44 s); ~95% is building the two 400 MB arrays, and
// most of THAT is the kernel faulting in and zeroing fresh anonymous pages — not
// arithmetic, and NOT DRAM bandwidth. This kernel ranks ALLOCATOR PAGE
// BEHAVIOUR. k1_dot.c carries the full writeup (phase split, THP finding,
// method, box); it applies to this file too.
//
// ### THE GO COLUMN IS NOT PAGE-SIZE-FAIR AND run.sh CANNOT FIX IT ###
// THP policy on the reference box is madvise-only, so a region gets huge pages
// only if its allocator asks. Helix's mimalloc asks and NumPy madvises its large
// allocations; Go's runtime allocator does not. Measured at N=50M, peak
// AnonHugePages: Go 0 MB vs Helix 643-778 MB and NumPy 776 MB. Minor faults:
// Go 196,005 vs Helix 117,527-133,000 and NumPy 5,285.
// The harness's equalizer does not reach Go: measured, running this binary under
// GLIBC_TUNABLES=glibc.malloc.hugetlb=1 changes nothing (0.43 s -> 0.48 s, within
// noise; AnonHugePages still 0), which is expected since Go does not allocate
// through glibc malloc. No Go-side equivalent of that tunable was found, so the
// Go column ships small-page and disclosed. (Scope of that claim: what was
// MEASURED is that the glibc tunable has no effect on this binary. A Go runtime
// knob granting THP was searched for and not found, but that is a negative result
// and not an exhaustive proof — if such a knob exists, run.sh should adopt it and
// this note is wrong and should be corrected.)
//
// CONSEQUENCE: after run.sh equalizes C and Rust, the columns split into two
// page-size classes and Go alone is stuck in the small-page one:
//     C      0.44 s -> 0.10 s   with hugetlb=1
//     Rust   0.44 s -> 0.12 s   with hugetlb=1
//     Go     0.43 s              NOT equalizable — small pages
//     Helix  0.22 s at 267% CPU  huge pages via mimalloc
// So Go's ~0.43 s is NOT comparable to the equalized C/Rust ~0.10-0.12 s, and
// Helix beating Go here is a statement about allocators, not about Go's codegen.
// Any table showing this Go number must carry that caveat. (Setting THP policy
// to "always" system-wide would equalize Go too, but changes the machine out
// from under every other kernel in the suite.)
//
// int64 arithmetic wraps two's-complement by definition in Go, so no explicit
// wrapping helpers are needed.
package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	var n int64 = 50000000
	if len(os.Args) > 1 {
		v, err := strconv.ParseInt(os.Args[1], 10, 64)
		if err != nil || v < 0 {
			fmt.Fprintln(os.Stderr, "N must be a non-negative integer")
			os.Exit(1)
		}
		n = v
	}

	a := make([]int64, n)
	b := make([]int64, n)
	for j := int64(0); j < n; j++ {
		a[j] = j % 97
		b[j] = j % 89
	}

	var total int64
	for j := int64(0); j < n; j++ {
		total += a[j] * b[j]
	}

	fmt.Println(total)
}

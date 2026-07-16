# K1 — 50M-element i64 dot product, NumPy.  python3 k1_dot_numpy.py [N]
#
# ### WHAT THIS KERNEL ACTUALLY MEASURES — read before quoting any number ###
# It is named for the dot product, but in the C ref the dot is ~4-5% of the
# runtime (0.017-0.022 s of 0.44 s); ~95% is building the two 400 MB arrays, and
# most of THAT is the kernel faulting in and zeroing fresh anonymous pages — not
# arithmetic, and NOT DRAM bandwidth. This kernel ranks ALLOCATOR PAGE BEHAVIOUR.
# See k1_dot.c for the full measurement writeup (phase split, THP finding,
# equalized numbers, method, box); it applies to every k1 file. Short version:
# THP policy on the reference box is madvise-only, so a region gets huge pages
# only if its allocator asks; glibc (C/Rust) does not ask by default, and
# equalizing that with GLIBC_TUNABLES=glibc.malloc.hugetlb=1 makes C
# (0.44 s -> 0.10 s) FASTER than Helix (0.22 s), reversing the published result.
# run.sh must export that tunable (it does today — keep it).
#
# THIS FILE IS ALREADY IN THE HUGE-PAGE CLASS — NumPy asks on its own. NumPy
# madvises large allocations, so unlike the C/Rust/Go refs it needs no tunable to
# be page-size-fair with Helix's mimalloc. Measured at N=50M: peak AnonHugePages
# 776,192 kB and only 5,285 minor faults (vs C's 195,389 with 0 huge pages), wall
# 0.29 s at 190% CPU. NumPy's knob is NUMPY_MADVISE_HUGEPAGE and it defaults to
# ON: setting NUMPY_MADVISE_HUGEPAGE=0 drops AnonHugePages to 0, raises minor
# faults to 198,957 and costs 2.3x wall (0.67 s) — same bytes, same code, purely
# page size. CONSEQUENCE: Helix 0.22 s vs NumPy 0.29 s is a page-size-FAIR
# comparison (both sides already have huge pages), so unlike the C/Rust/Go
# columns Helix's ~1.3x here is a real result and not an allocator artifact. It
# is also much smaller than the ~2.0x this kernel shows against C as published.
#
# Genuinely expressible as array ops: build both int64 arrays vectorized, then
# np.dot. `np.dot` on int64 is a plain C accumulation loop (BLAS is float-only,
# so no BLAS advantage here) and materializes NO intermediate product array —
# structurally the same single streaming pass as the C loop.
#
# MEMORY PARITY (this is why `%=` is in-place). C/Rust/Go hold exactly two i64
# arrays = 800 MB (measured maxRSS 782-784 MB). The obvious NumPy spelling
#     j = np.arange(N); a = j % 97; b = j % 89
# keeps `j` live alongside a and b = THREE live 400 MB arrays: measured maxRSS
# 1,197,992 kB — 400 MB more than every other ref, on a kernel where touching
# fresh pages IS the workload. Neither `del j` nor fusing to `np.arange(N) % 97`
# fixes it (both MEASURED at maxRSS ~1,198,0xx kB: `del j` frees too late, and
# the fused form's arange temporary is live exactly when the third array exists).
# Allocating each array ONCE and reducing it in-place is the only spelling that
# reaches parity: measured maxRSS 807,340-807,672 kB against C's 782,848 kB —
# the ~24 MB residue is the interpreter itself, not array overhead.
#
# int64 arithmetic wraps silently in NumPy, matching C/-fwrapv, Go, Rust
# wrapping_*, and Helix. Integer addition is associative mod 2**64, so any
# summation order NumPy picks yields the identical anchor.
import sys
import numpy as np

N = int(sys.argv[1]) if len(sys.argv) > 1 else 50000000
if N < 0:
    sys.exit("N must be >= 0")

a = np.arange(N, dtype=np.int64)
a %= 97
b = np.arange(N, dtype=np.int64)
b %= 89

print(int(np.dot(a, b)))

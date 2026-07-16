# K1 — 50M-element i64 dot product, plain CPython.  python3 k1_dot_cpython.py [N]
#
# ### WHAT THIS KERNEL ACTUALLY MEASURES — read before quoting any number ###
# It is named for the dot product, but in the C ref the dot is ~4-5% of the
# runtime (0.017-0.022 s of 0.44 s); ~95% is building the two 400 MB arrays, and
# most of THAT is the kernel faulting in and zeroing fresh pages — not
# arithmetic, and NOT DRAM bandwidth (this ref is the exception: see below).
# The kernel ranks ALLOCATOR PAGE BEHAVIOUR. See k1_dot.c for the full writeup
# (phase split, THP finding, equalized numbers, method, box); it applies to every
# k1 file. Short version: THP policy on the reference box is madvise-only, so a
# region gets huge pages only if its allocator asks; Helix's mimalloc and NumPy
# ask, glibc (C/Rust) and Go do not, and equalizing glibc with
# GLIBC_TUNABLES=glibc.malloc.hugetlb=1 makes C (0.44 s -> 0.10 s) FASTER than
# Helix (0.22 s), reversing the published result. run.sh must export that tunable
# (it does today — keep it).
#
# THIS FILE IS THE ONE k1 WHERE PAGE SIZE DOES NOT MATTER. CPython's 400 MB
# array('q') buffers do go through glibc malloc, so this ref DOES respond to the
# tunable — measured at N=50M, GLIBC_TUNABLES=glibc.malloc.hugetlb=1 takes it
# from 0 to 290,816 kB of AnonHugePages and cuts minor faults 196,478 -> 60,322 —
# but the wall time does not move (10.34 s -> 10.04 s, within noise). Interpreter
# overhead dominates this ref so completely that ~136k saved page faults are lost
# in it. So the tunable is harmless here and the CPython column is legitimate
# either way; it is the compiled refs (k1_dot.c, k1_dot.rs) whose standing the
# tunable actually decides.
#
# THE PAIN, honestly: CPython has no i64. `array('q')` gives real 8-byte signed
# buffers (same 400 MB x 2 as every other language), but every `a[j]` UNBOXES to
# a fresh Python int object and every `+` allocates. So this pays ~50M boxing
# round-trips that C/Rust/Go/Helix never pay. That is the honest cost of the
# idiom, not a rigged baseline — it is what a competent Python programmer writes
# when they refuse to import numpy.
#
# MEMORY PARITY (this is why the initializers are generator expressions, not list
# comprehensions). C/Rust/Go hold exactly two i64 arrays = 800 MB (measured
# maxRSS 782-784 MB). Spelling the initializer `array('q', [j % 97 for j in ...])`
# with BRACKETS materializes the whole 50M-element list of boxed ints first and
# then copies it into the array, so peak RSS carries a transient ~400 MB list on
# top of the arrays: measured maxRSS 1,182,108 kB with 391,789 minor faults. The
# generator form streams elements into the array instead: measured maxRSS
# 791,552-791,936 kB with 196,474-196,481 minor faults — parity with C/Rust/Go's
# 782-784 MB. Wall time is unchanged within noise; this buys honesty, not speed.
#
# WRAPPING: Python ints are arbitrary-precision and never wrap, so the exact sum
# is reduced to i64 ONCE at the end. This is bit-identical to wrapping every add
# (mod 2**64 is a ring homomorphism: reducing after each add or once at the end
# gives the same residue), and each product is exact in i64 anyway
# (max 96*88 = 8448). So the anchor matches the wrapping languages exactly.
import sys
from array import array

N = int(sys.argv[1]) if len(sys.argv) > 1 else 50000000
if N < 0:
    sys.exit("N must be >= 0")


def to_i64(x):
    x &= (1 << 64) - 1
    return x - (1 << 64) if x >> 63 else x


a = array('q', (j % 97 for j in range(N)))
b = array('q', (j % 89 for j in range(N)))

total = 0
for j in range(N):
    total += a[j] * b[j]

print(to_i64(total))

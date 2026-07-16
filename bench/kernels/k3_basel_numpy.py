# K3 — Basel series, NumPy. python3 k3_basel_numpy.py [N]
#
# READ THIS BEFORE TRUSTING THE NUMBER — the honest caveats for this entry:
#
# 1. `terms.sum()` is WRONG for this kernel and is deliberately NOT used. np.sum (and
#    np.add.reduce) use PAIRWISE summation, which reassociates the additions and lands
#    on a different f64 than naive left-to-right. Measured at N=1e6:
#        naive / cumsum : 1.64493306684877005   <- the anchor
#        np.sum         : 1.64493306684872631   <- differs from the 13th digit
#    That divergence is not a bug; it is the whole point of the kernel. Float addition
#    is not associative, so "sum these N terms" is underspecified until you fix the
#    ORDER. np.sum's answer is in fact the more ACCURATE one (pairwise beats naive on
#    error growth) — it is simply not the quantity every other language here computes.
#
# 2. np.cumsum IS the naive left-to-right sum: it must emit every partial sum, so it
#    cannot reassociate. Its last element is exactly the C loop's accumulator, bit for
#    bit (verified). That is why it, not sum(), is used here.
#
# 3. FAIRNESS: NumPy pays O(N) memory (an 800 MB f64 array at N=1e8) where every other
#    language pays O(1) in a register. Ops are in-place to keep the peak at one array,
#    but a serial scan is simply not what a SIMD array library is for — NumPy cannot
#    vectorize an order-fixed accumulation, so it is stuck doing C's work with a
#    memory-bandwidth tax. Judge its time with that in mind.
import sys

import numpy as np

n = int(sys.argv[1]) if len(sys.argv) > 1 else 100000000

k = np.arange(1.0, n + 1.0, dtype=np.float64)
k *= k  # k²  (exact for k ≤ 1e8: the product ≤ 1e16 rounds to nearest, as C's x*x does)
np.divide(1.0, k, out=k)  # 1/k²
np.cumsum(k, out=k)  # sequential accumulation — the naive left-to-right sum

print("%.17f" % k[-1])

# K4 — all-pairs Manhattan distance sum, NumPy (row-chunked).
#
# MEMORY NOTE: the natural NumPy idiom for this kernel is the broadcast
#   np.abs(pts[:, None] - pts[None, :])
# which at N=15000 materializes a 15000^2 * 8 B = 1.8 GB temporary. That is not a
# benchmark, it is an OOM. So this walks ONE ROW at a time: row i is the vector
# |pts[i] - pts[i+1:]|, summed. Same O(N^2) triangular algorithm and same anchor
# as C/Rust/Go/Helix/CPython — only the inner loop is vectorized instead of
# scalar, and peak memory stays O(N).
#
# FAIRNESS NOTE: this row is a close like-for-like against the Helix row. Both
# are "vectorized/compiled inner loop, interpreted outer loop", and — contrary to
# what an earlier version of this file claimed — BOTH RUN ON ONE CORE. Helix's
# outer map does NOT parallelize on this kernel (measured 97% CPU; the triangular
# inner range makes it ineligible — see k4_allpairs.helix), and NumPy's ufuncs
# (subtract/abs/sum over int64) are not threaded either.
#
# Do not be fooled by the %CPU on this row: measured 361% at N=15000, but that is
# an idle OpenBLAS spin-pool, not work. `import numpy` ALONE measures 473% CPU,
# and forcing OMP_NUM_THREADS=OPENBLAS_NUM_THREADS=MKL_NUM_THREADS=1 drops this
# kernel to 105% CPU while the wall time does not improve (0.110s vs 0.116s) —
# the extra cores are burning cycles, not shortening the run.
import sys

import numpy as np

N = int(sys.argv[1]) if len(sys.argv) > 1 else 15000

pts = (np.arange(N, dtype=np.int64) * 2654435761) % 1000

total = 0
for i in range(N):
    total += int(np.abs(pts[i] - pts[i + 1 :]).sum())
print(total)

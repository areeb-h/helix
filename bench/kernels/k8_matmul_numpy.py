# K8 — dense f64 matmul, NumPy `a @ b` (python3 with numpy).
#   python3 k8_matmul_numpy.py [n]        # default n=1024
#
#   A[i][j] = ((i*j) % 100) * 0.5      B[i][j] = ((i+j) % 100) * 0.25
#   anchor  = C[0][0] + C[n-1][n-1] + C[n/2][n/2]   printed as %.6f
#
# This is the BLAS reference — `@` dispatches to whatever GEMM the wheel was linked
# against, blocked, SIMD, and multi-threaded by default. Here that is OpenBLAS
# 0.3.33 (USE64BITINT DYNAMIC_ARCH, Haswell kernel), per numpy 2.5.0's build config.
# It is the honest peer of k8_matmul.helix (tensor/faer path); it is NOT the peer of
# the naive C/Rust/Go/CPython triple loops, which call no GEMM at all.
#
# It is also the one file here that is not the same ALGORITHM as its siblings, which
# is the point of the kernel: the comparison being drawn is "who calls a real GEMM",
# not "whose loop codegen is better". Read the two Helix files together to separate
# those two claims.
#
# THIS FILE WINS THE KERNEL. Whole-process wall, min of 5, idle 6-core box: numpy
# 0.057s vs helix 0.087s at n=512, 0.069s vs 0.320s at n=1024, 0.136s vs 1.179s at
# n=2048. With the GEMM isolated from startup and setup, OpenBLAS beats faer at every
# size measured: 0.0010s vs 0.0059s (n=512), 0.0050s vs 0.0198s (n=1024), 0.0398s vs
# 0.0594s (n=2048). See k8_matmul.helix's header for the method and the caveats.
#
# The default is 1024, matching k8_matmul.helix, because at 512 this file is not
# really timing a GEMM: `import numpy` alone is 0.049s of the 0.057s wall (83%) and
# the GEMM is 1.0ms of it (1.7%). The naive files default to 512 instead — that is
# where an interpreted triple loop still finishes.
#
# A and B are built with broadcasting rather than a Python loop, which is the only way
# a NumPy user would write it. The O(n**2) setup is NOT noise next to the O(n**3) GEMM
# at these sizes — measured, the GEMM is 1.7% of this file's n=512 wall — but it is the
# same setup the Helix peer does, so the pair stays comparable.
# The anchor is bit-identical to the naive loops: every product and partial sum is an
# exact multiple of 1/8 inside f64's 53-bit mantissa, so BLAS's blocking, its FMAs and
# its thread count cannot move the value. (Verified: all 7 programs print the identical
# anchor at n=4/8/16/64/512 — bash verify_k8.sh N.)
import sys

import numpy as np

n = int(sys.argv[1]) if len(sys.argv) > 1 else 1024
if n <= 0:
    sys.exit("n must be positive")

i = np.arange(n, dtype=np.int64)
a = ((i[:, None] * i[None, :]) % 100).astype(np.float64) * 0.5
b = ((i[:, None] + i[None, :]) % 100).astype(np.float64) * 0.25

c = a @ b

anchor = c[0, 0] + c[n - 1, n - 1] + c[n // 2, n // 2]
print("%.6f" % anchor)

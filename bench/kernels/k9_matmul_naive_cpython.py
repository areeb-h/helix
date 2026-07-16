# K8 — dense f64 matmul, naive ijk triple loop (CPython, stdlib only).
#   python3 k8_matmul_cpython.py [n]        # default n=512
#
#   A[i][j] = ((i*j) % 100) * 0.5      B[i][j] = ((i+j) % 100) * 0.25
#   anchor  = C[0][0] + C[n-1][n-1] + C[n/2][n/2]   printed as %.6f
#
# HONEST WARNING: this is n**3 = 134 million iterations of interpreted bytecode at the
# default n=512 — measured 15.2s here (min of 3, idle 6-core box), against 0.369s for
# naive C (41x). That is the real cost of a hand-written numeric loop in CPython and
# is exactly why the numpy sibling exists; pretending CPython is only 2x off would be
# the lie this suite is here to avoid. (It does, however, beat the naive Helix file's
# 25.4s by 1.7x — see the note in k8_matmul_naive.helix. Honest suites report the
# losses too.)
#
# Same algorithm, same ijk order, same materialized row-major lists as the C file, and
# no local-variable tricks (no `a_row = a[i*n:...]` hoisting) that the other languages
# do not also get. Python floats ARE f64, and every product/partial sum here is an
# exact multiple of 1/8 inside the 53-bit mantissa, so the anchor is bit-identical.
import sys

n = int(sys.argv[1]) if len(sys.argv) > 1 else 512
if n <= 0:
    sys.exit("n must be positive")

a = [0.0] * (n * n)
b = [0.0] * (n * n)
c = [0.0] * (n * n)

for i in range(n):
    for j in range(n):
        a[i * n + j] = float((i * j) % 100) * 0.5
        b[i * n + j] = float((i + j) % 100) * 0.25

# naive ijk: inner loop strides B by n, matching every sibling.
for i in range(n):
    for j in range(n):
        s = 0.0
        for k in range(n):
            s += a[i * n + k] * b[k * n + j]
        c[i * n + j] = s

anchor = c[0] + c[(n - 1) * n + (n - 1)] + c[(n // 2) * n + (n // 2)]
print("%.6f" % anchor)

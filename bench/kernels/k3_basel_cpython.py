# K3 — Basel series, CPython. python3 k3_basel_cpython.py [N]
#
# Naive left-to-right f64 summation. A Python float IS a C double and `+` is a single
# IEEE add, so this reproduces the C loop's rounding exactly — one term at a time, in
# order. (Deliberately NOT math.fsum, which is exactly-rounded, and NOT sum(), which is
# also naive but would build the term sequence separately; the explicit loop is the
# clearest statement of "same algorithm as C".)
import sys

n = int(sys.argv[1]) if len(sys.argv) > 1 else 100000000

s = 0.0
for k in range(1, n + 1):
    x = float(k)
    s += 1.0 / (x * x)

print("%.17f" % s)

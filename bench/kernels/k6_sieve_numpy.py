# K6 — count the primes below N (default 10,000,000). Anchor 664579 = pi(10^7).
# Sieve of Eratosthenes, vectorized: each prime's multiples are struck out with one
# strided slice assignment instead of a Python-level inner loop. This is THE
# idiomatic NumPy sieve and it is a fair entry, but note what it is — like Helix's
# `primes()` builtin, the real loop runs in native code, not in the language.
#
# Same sieve, same 2 <= p < n boundary. Two presentational differences from the
# C/Rust/Go/CPython columns, both exactly equivalent:
#   * the outer loop stops at isqrt(n) — beyond it p*p >= n, so those iterations
#     would strike out nothing anyway;
#   * primes are counted in one final pass rather than fused into the outer loop.
#   python3 k6_sieve_numpy.py [N]
#
# READ THE WALL CLOCK CAREFULLY — this column's ~0.08 s process total is ~85% startup:
# `import numpy` alone is ~0.065 s, and the sieve itself is only ~0.013 s, the FASTEST
# in this kernel (C 0.017 s, Helix 0.020 s, Go 0.023 s — all full-process). Any table
# that ranks this column below those is ranking importer overhead, not sieves.
import math
import sys

import numpy as np


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 10000000
    if n <= 2:
        print(0)
        return

    # composite[p] marks p as composite; one byte per entry, like the byte arrays
    # in the other columns.
    composite = np.zeros(n, dtype=np.bool_)
    for p in range(2, math.isqrt(n) + 1):
        if not composite[p]:
            composite[p * p :: p] = True

    # Primes p with 2 <= p < n are the entries still unmarked at index >= 2.
    # (0 and 1 are never marked composite, so they must be excluded explicitly.)
    print(int((n - 2) - np.count_nonzero(composite[2:])))


main()

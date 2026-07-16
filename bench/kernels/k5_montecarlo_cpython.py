# K5 — Monte Carlo pi, CPython. The xorshift64 reference stream. Anchor = raw
# hit count.
#   python3 k5_montecarlo_cpython.py [N]
# Python ints are arbitrary precision, so the LEFT shifts must be masked back to
# 64 bits to emulate uint64 wraparound. `s` is therefore always in [0, 2^64), so
# `>>` on it is already the logical shift the reference stream wants.
# No NumPy variant exists for this kernel: each sample depends on the previous
# state, so the stream is inherently sequential and cannot be vectorized.
import sys

MASK64 = (1 << 64) - 1


def main() -> None:
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 100000000

    s = 88172645463325252
    hits = 0
    for _ in range(n):
        s ^= (s << 13) & MASK64
        s ^= s >> 7
        s ^= (s << 17) & MASK64
        x = (s >> 11) / 9007199254740992.0  # 2^53 — exact
        s ^= (s << 13) & MASK64
        s ^= s >> 7
        s ^= (s << 17) & MASK64
        y = (s >> 11) / 9007199254740992.0
        if x * x + y * y < 1.0:
            hits += 1
    print(hits)


main()

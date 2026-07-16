# K6 — count the primes below N (default 10,000,000). Anchor 664579 = pi(10^7).
# Sieve of Eratosthenes over a bytearray, with the SAME nested loop as the C, Rust
# and Go versions — deliberately NOT the `composite[p*p::p] = b"\x01" * ...` slice
# trick, which would push the inner loop into C and measure memset instead of the
# CPython interpreter. The vectorized sieve is the NumPy column's job
# (k6_sieve_numpy.py); this column is honestly the interpreter's own speed.
#   python3 k6_sieve_cpython.py [N]
#
# This is the slowest column that still runs a SIEVE (0.63 s vs C's 0.017 s), but do
# not read it as the suite's floor: pure Helix without `primes()` needs 97 s for the
# same count (k6_sieve_trial.helix), ~150x slower than this file, because Helix's
# immutable surface cannot express a sieve and falls back to trial division. The
# Helix column of this kernel (k6_sieve.helix) calls a native builtin instead.
import sys


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 10000000
    if n <= 2:
        print(0)
        return

    # composite[p] marks p as composite. Counts primes p with 2 <= p < n.
    composite = bytearray(n)
    count = 0
    for p in range(2, n):
        if not composite[p]:
            count += 1
            # When p*p >= n this range is empty.
            for m in range(p * p, n, p):
                composite[m] = 1

    print(count)


main()

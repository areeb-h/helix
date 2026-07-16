# K7 — word frequency over a generated corpus, CPython (stdlib only). python3 k7_wordcount_cpython.py [N]
# collections.Counter over a generator is THE Python word-count idiom. Counter's counting
# loop is C, but every word is still a heap-allocated str hashed by the dict — same work
# as everyone else, same order. The generator streams — one word is alive at a time and
# the Counter holds at most 10k entries — so memory is flat in N: measured peak RSS 11.2 MB
# at N=1e6 and 11.8 MB at N=5e6 (+5% for 5x the corpus). Contrast the Helix and NumPy
# variants, which materialize the whole corpus and are O(N)-resident — see their headers.
#
# Corpus = the k5 xorshift64 reference stream (seed 88172645463325252), one draw per word,
# bit-identical in all six languages. See k7_wordcount.c for why the previous
# `(i*2654435761) % 10000` generator was a non-test.
# Python ints are arbitrary precision, so the LEFT shifts are masked back to 64 bits to
# emulate uint64 wraparound (same as k5_montecarlo_cpython.py). `s` is therefore always in
# [0, 2^64), so `>>` on it is the logical shift the reference stream wants and
# `& 9007199254740991` is a no-op — spelled out to match Helix, where it is load-bearing.
import sys
from collections import Counter

DEFAULT_N = 5_000_000
MODULUS = 10000
MASK64 = (1 << 64) - 1


def words(n):
    s = 88172645463325252
    for _ in range(n):
        s ^= (s << 13) & MASK64
        s ^= s >> 7
        s ^= (s << 17) & MASK64
        yield "w" + str(((s >> 11) & 9007199254740991) % MODULUS)


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_N
    counts = Counter(words(n))
    # `default=0`: max() of an empty sequence raises ValueError, so N=0 would abort
    # where C/Rust/Go/Helix all print 0.
    print(max(counts.values(), default=0))


main()

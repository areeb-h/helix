# K7 — word frequency over a generated corpus, NumPy. python3 k7_wordcount_numpy.py [N]
#
# READ THIS BEFORE COMPARING: this is NOT the same algorithm as the other five, and its
# WALL TIME IS NOT A NUMPY DATAPOINT. Two separate reasons:
#
#   1. np.unique is a SORT-based group-by (O(n log n)), not an O(n) hash map.
#   2. The corpus is the k5 xorshift64 stream, where each state depends on the previous
#      one. That is inherently sequential and CANNOT be vectorized (same reason k5 has no
#      NumPy variant at all), so `np.fromiter` below drives a plain Python generator: the
#      id stream runs at CPython speed and dominates the runtime. What np.unique costs is
#      not separable from that here.
#
# It is kept for one reason only, and it is a good one: it counts by SORTING rather than
# by hashing, so when it agrees on the anchor it is independent evidence that the five
# hash-map implementations are right. Read it as an anchor cross-check, never as a speed
# comparison.
#
# MEMORY — this variant is O(N)-resident, unlike C/Rust/Go/CPython which stream one word
# at a time. Measured peak RSS 265 MB at N=5e6, and it scales: 75 MB at N=1e6 (~48 marginal
# bytes per word). See k7_wordcount.helix for the full cross-language table. The live
# arrays, from their verified dtype itemsizes at N=5e6:
#   ids   int64  8 B/elem   =  40 MB
#   U4    ids.astype('U4') 16 B/elem = 80 MB   (UCS-4: 4 bytes per char, fixed width)
#   words '<U5' 20 B/elem   = 100 MB
# The `astype('U4')` is not decoration. The obvious `np.char.add("w", ids.astype(str))`
# yields '<U22' — 88 B/elem, 440 MB — because int64 -> str reserves 21 chars for the
# widest possible int64. Going via 'U4' (ids are 0..9999, so 4 chars is lossless — verified
# equal to the astype(str) route) builds the '<U5' array directly and never materializes
# the 440 MB intermediate.
import sys

import numpy as np

DEFAULT_N = 5_000_000
MODULUS = 10000
MASK64 = (1 << 64) - 1


def id_stream(n):
    # Bit-identical to k7_wordcount.c: left shifts masked back to 64 bits to emulate
    # uint64 wraparound, so `s` stays in [0, 2^64) and `>>` is the logical shift the
    # reference stream wants.
    s = 88172645463325252
    for _ in range(n):
        s ^= (s << 13) & MASK64
        s ^= s >> 7
        s ^= (s << 17) & MASK64
        yield ((s >> 11) & 9007199254740991) % MODULUS


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_N
    ids = np.fromiter(id_stream(n), dtype=np.int64, count=n)
    words = np.char.add("w", ids.astype("U4"))  # '<U5', 20 B/elem
    # Zero-size guard: np.unique returns empty counts for an empty corpus, and
    # counts.max() then raises "zero-size array to reduction operation maximum which has
    # no identity" — where C/Rust/Go/Helix all print 0.
    if words.size == 0:
        print(0)
        return
    _, counts = np.unique(words, return_counts=True)
    print(int(counts.max()))


main()

# K4 — all-pairs Manhattan distance sum, pure CPython (stdlib only).
#
# SHAPE: idiomatic Python, not transliterated C. The inner loop is a list
# comprehension handed to sum(), with the loop-invariant pts[i] hoisted into `pi`
# and the triangle taken as a slice — how a Python programmer would really write
# it. The suite lets NumPy be NumPy and Helix be Helix, so CPython gets its own
# idiom too; pinning only CPython to C's nested-index shape would measure the
# transliteration, not the language. Same O(N^2) triangular algorithm and the
# same anchor (37499962500 at N=15000) as every other language here.
#
# Measured, best of 3 at N=15000 on CPython 3.12: this shape 4.98s, versus 10.61s
# for the C-shaped `for j in range(i+1, N): total += abs(pts[i] - pts[j])` — a
# 2.13x gap that is pure interpreter overhead (per-index bytecode dispatch, and
# re-loading pts[i] every iteration). CPython will not hoist pts[i] for you.
# C and Rust do get that hoist for free — measured, hand-hoisting pts[i] moves
# them by -1.5% and +1.2%, i.e. nothing but noise. Go does NOT fully: the same
# hand-hoist buys Go 11.4%, so its as-written number carries that cost.
#
# This is meant to be slow — ~112M inner iterations at the default N. That is the
# honest number; do NOT shrink N for CPython.
#
# NOTE: for points on a line this O(N^2) algorithm is deliberately the wrong one
# — see k4_allpairs.helix. Sorting and taking prefix sums gets this same anchor
# in 2.11 ms of plain CPython, faster than any O(N^2) row here, C included.
import sys

N = int(sys.argv[1]) if len(sys.argv) > 1 else 15000

pts = [(i * 2654435761) % 1000 for i in range(N)]

total = 0
for i in range(N):
    pi = pts[i]
    total += sum([abs(pi - pj) for pj in pts[i + 1 :]])
print(total)

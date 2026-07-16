# K2 — escape-time Mandelbrot, 1200x1200 grid, max 100 iterations, over
# x in [-2.1, 0.6] and y in [-1.2, 1.2]. Anchor = sum of every pixel's iteration count.
#
#   python3 k2_mandelbrot_cpython.py [grid]
#
# Pure CPython, no numpy: this kernel is deliberately the scalar escape-time
# algorithm, one pixel at a time, the same nested loop every other language here runs.
# (A vectorised numpy mandelbrot is a genuinely different algorithm — it iterates every
# pixel the full 100 times under a mask instead of breaking out early — so it is not
# included here. It would be a fair *separate* kernel, not a fair entry in this one.)
import sys

G = int(sys.argv[1]) if len(sys.argv) > 1 else 1200
total = 0
for y in range(G):
    ci = -1.2 + y * 2.4 / G
    for x in range(G):
        cr = -2.1 + x * 2.7 / G
        zr = 0.0
        zi = 0.0
        i = 0
        while i < 100 and zr * zr + zi * zi <= 4.0:
            zr, zi = zr * zr - zi * zi + cr, 2.0 * zr * zi + ci
            i += 1
        total += i
print(total)

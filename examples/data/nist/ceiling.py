#!/usr/bin/env python3
"""The most digits ANY double-precision program can report on NIST's StRD files.

`examples/statistics/nist.helix` scores Helix against NIST's certified values. This script
says what a perfect double-precision program would score: it reads the same files, rounds
every input to a float64 exactly as Helix does (including the polynomial columns x**p, which
the Helix program forms in floating point), then computes every statistic in EXACT rational
arithmetic (`fractions.Fraction`) and rounds the answer once. The LRE of that answer is the
ceiling — e.g. NumAcc4's standard deviation cannot exceed 8.3 digits, because `10000000.2` is
not a double. Standard library only; it shares no code with Helix.

    python3 examples/data/nist/ceiling.py examples/data/nist
"""
import math, re, sys
from fractions import Fraction as F

def lre(est, cert):
    err = abs(est) if cert == 0 else abs(est - cert) / abs(cert)
    return 15.0 if err == 0 else max(0.0, min(15.0, -math.log10(err)))

def lines_of(p): return open(p, encoding='ascii').read().replace('\r', '').split('\n')
def span(lines, label):
    hit = next(l for l in lines if re.search(label, l))
    m = re.search(r'lines\s+(\d+)\s+to\s+(\d+)', hit); return int(m[1]), int(m[2])
def block(lines, label):
    lo, hi = span(lines, label); return lines[lo-1:hi]
def after(lines, pat): return float(re.search(pat, next(l for l in lines if re.search(pat, l)))[1])

def solve(A, b):  # exact Gaussian elimination on Fractions
    n = len(A); M = [row[:] + [bb] for row, bb in zip(A, b)]
    for c in range(n):
        p = next(r for r in range(c, n) if M[r][c] != 0); M[c], M[p] = M[p], M[c]
        for r in range(n):
            if r != c and M[r][c] != 0:
                f = M[r][c] / M[c][c]; M[r] = [x - f*y for x, y in zip(M[r], M[c])]
    return [M[i][n] / M[i][i] for i in range(n)]

def inv(A):
    n = len(A); return [list(col) for col in zip(*[solve(A, [F(int(i == j)) for i in range(n)]) for j in range(n)])]

def lls(root, name):
    L = lines_of(f'{root}/lls/{name}.dat'); cert = block(L, 'Certified Values')
    params = [l.split() for l in cert if re.match(r'^\s*B\d+\s', l)]
    b = [float(p[1]) for p in params]; se = [float(p[2]) for p in params]
    icpt = params[0][0] == 'B0'
    rows = [l.split() for l in block(L, r'^\s*Data\s') if l.split()]
    y = [F(float(r[0])) for r in rows]; n = len(y); k = len(b)
    if len(rows[0]) > 2: cols = [[F(float(r[j])) for r in rows] for j in range(1, len(rows[0]))]
    else:
        deg = k - 1 if icpt else k
        cols = [[F(float(r[1]) ** p) for r in rows] for p in range(1, deg + 1)]  # float64 powers, as the Helix program forms them
    X = [[F(1)] * n] + cols if icpt else cols
    XtX = [[sum(a*c for a, c in zip(u, v)) for v in X] for u in X]
    Xty = [sum(a*c for a, c in zip(u, y)) for u in X]
    beta = solve(XtX, Xty)
    res = [yi - sum(bj * X[j][i] for j, bj in enumerate(beta)) for i, yi in enumerate(y)]
    rss = sum(r*r for r in res); s2 = rss / (n - k); Ai = inv(XtX)
    ses = [math.sqrt(float(s2 * Ai[j][j])) for j in range(k)]
    ybar = sum(y) / n
    tss = sum((v - ybar) ** 2 for v in y) if icpt else sum(v*v for v in y)
    r2 = float(1 - rss / tss); rsd = math.sqrt(float(s2))
    return (name, min(lre(float(beta[i]), b[i]) for i in range(k)), min(lre(ses[i], se[i]) for i in range(k)),
            lre(rsd, after(cert, r'Standard Deviation\s+(\S+)')), lre(r2, after(cert, r'R-Squared\s+(\S+)')))

def univ(root, name):
    L = lines_of(f'{root}/univ/{name}.dat'); cert = block(L, 'Certified Values')
    y = [F(float(l.split()[0])) for l in block(L, r'^\s*Data\s') if l.split()]; n = len(y)
    ybar = sum(y) / n; d = [v - ybar for v in y]; ss = sum(v*v for v in d)
    sd = math.sqrt(float(ss / (n - 1))); r1 = float(sum(d[i]*d[i-1] for i in range(1, n)) / ss)
    return (name, lre(float(ybar), after(cert, r'ybar:\s+(\S+)')), lre(sd, after(cert, r'\bs:\s+(\S+)')),
            lre(r1, after(cert, r'r\(1\):\s+(\S+)')))

root = sys.argv[1]
for n in ['Norris','Pontius','NoInt1','NoInt2','Filip','Longley','Wampler1','Wampler2','Wampler3','Wampler4','Wampler5']:
    r = lls(root, n); print(f'{r[0]:<10} {r[1]:5.1f} {r[2]:5.1f} {r[3]:5.1f} {r[4]:5.1f}')
for n in ['PiDigits','Lottery','Lew','Mavro','Michelso','NumAcc1','NumAcc2','NumAcc3','NumAcc4']:
    r = univ(root, n); print(f'{r[0]:<10} {r[1]:5.1f} {r[2]:5.1f} {r[3]:5.1f}')

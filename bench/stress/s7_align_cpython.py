# S7 — local pairwise alignment, pure CPython. Score-only Gotoh affine DP (rolling
# rows), replicating src/align.rs local mode exactly.
NEG = -10**9
MATCH, MIS, GOPEN, GEXT = 1, -1, -5, -1
FG = GOPEN + GEXT  # first gap base = -6


def local_score(x, y):
    m = len(y)
    prev_mm = [NEG] * (m + 1)
    prev_xx = [NEG] * (m + 1)
    prev_yy = [NEG] * (m + 1)
    prev_mm[0] = 0
    for j in range(1, m + 1):
        prev_yy[j] = max(prev_mm[j - 1] + FG, prev_yy[j - 1] + GEXT)
    best = 0
    for i in range(1, len(x) + 1):
        cur_mm = [NEG] * (m + 1)
        cur_xx = [NEG] * (m + 1)
        cur_yy = [NEG] * (m + 1)
        cur_xx[0] = max(prev_mm[0] + FG, prev_xx[0] + GEXT)
        xi = x[i - 1]
        for j in range(1, m + 1):
            s = MATCH if xi == y[j - 1] else MIS
            d = prev_mm[j - 1]
            if prev_xx[j - 1] > d:
                d = prev_xx[j - 1]
            if prev_yy[j - 1] > d:
                d = prev_yy[j - 1]
            mval = d + s
            if mval < 0:
                mval = 0
            cur_mm[j] = mval
            if mval > best:
                best = mval
            xo, xe = prev_mm[j] + FG, prev_xx[j] + GEXT
            cur_xx[j] = xo if xo >= xe else xe
            yo, ye = cur_mm[j - 1] + FG, cur_yy[j - 1] + GEXT
            cur_yy[j] = yo if yo >= ye else ye
        prev_mm, prev_xx, prev_yy = cur_mm, cur_xx, cur_yy
    return best


def read_fasta(path):
    seqs, cur = [], []
    with open(path) as f:
        for line in f:
            if line.startswith(">"):
                if cur:
                    seqs.append("".join(cur))
                    cur = []
            else:
                cur.append(line.strip())
    if cur:
        seqs.append("".join(cur))
    return seqs


ref = read_fasta("data/ref.fa")[0]
scores = [local_score(q, ref) for q in read_fasta("data/queries.fa")]
print(f"total={sum(scores)}")
print(f"hits={sum(1 for s in scores if s >= 40)}")
print(f"max={max(scores)}")

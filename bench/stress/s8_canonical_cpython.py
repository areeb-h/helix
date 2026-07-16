# S8 — canonical k-mer spectrum, pure CPython. canonical = min(kmer, revcomp).
K = 10
COMP = str.maketrans("ACGT", "TGCA")


def revcomp(s):
    return s.translate(COMP)[::-1]


def read_seq(path):
    parts = []
    with open(path) as f:
        for line in f:
            if not line.startswith(">"):
                parts.append(line.strip())
    return "".join(parts)


seq = read_seq("data/genome.fa")
counts = {}
for i in range(len(seq) - K + 1):
    km = seq[i:i + K]
    canon = km if km <= revcomp(km) else revcomp(km)
    counts[canon] = counts.get(canon, 0) + 1

print(f"distinct={len(counts)}")
print(f"total={sum(counts.values())}")
print(f"max={max(counts.values())}")

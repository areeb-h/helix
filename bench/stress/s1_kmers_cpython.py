# S1 — k-mer spectrum, pure CPython (idiomatic dict over string slices).
K = 10


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
    sub = seq[i:i + K]
    counts[sub] = counts.get(sub, 0) + 1

print(f"distinct={len(counts)}")
print(f"total={sum(counts.values())}")
print(f"max={max(counts.values())}")

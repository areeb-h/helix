# S2 — FASTQ GC + Phred, pure CPython.
def records(path):
    with open(path) as f:
        while True:
            h = f.readline()
            if not h:
                return
            seq = f.readline().strip()
            f.readline()           # '+'
            qual = f.readline().strip()
            yield seq, qual


n = 0
bases = 0
gcsum = 0.0
qsum = 0.0
hiq = 0
for seq, qual in records("data/reads.fq"):
    n += 1
    L = len(seq)
    bases += L
    gcsum += sum(1 for c in seq if c == "G" or c == "C") / L
    mq = sum(ord(c) - 33 for c in qual) / L
    qsum += mq
    if mq >= 30.0:
        hiq += 1

print(f"reads={n}")
print(f"bases={bases}")
print(f"gc4={int(gcsum / n * 10000 + 0.5)}")
print(f"q4={int(qsum / n * 10000 + 0.5)}")
print(f"hiq={hiq}")

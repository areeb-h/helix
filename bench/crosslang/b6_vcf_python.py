# NOTE: a one-field split, NOT a spec-compliant parse — it only reads column 6
# (QUAL) and ignores typing, INFO, validation. Helix's read_vcf does the full
# parse, so this Python number is NOT measuring the same work (see README).
total = 0.0
n = 0
with open("data/big.vcf") as f:
    for line in f:
        if line.startswith("#"):
            continue
        total += float(line.split("\t")[5])
        n += 1
print(total / n)

# S3 — VCF analysis, pure CPython (hand parse).
total = 0
counts = {}
keep = 0
with open("data/variants.vcf") as f:
    for line in f:
        if line.startswith("#"):
            continue
        total += 1
        c = line.rstrip("\n").split("\t")
        qual = float(c[5])
        if qual > 50.0:
            keep += 1
            info = dict(kv.split("=") for kv in c[7].split(";"))
            g = info["GENE"]
            counts[g] = counts.get(g, 0) + 1

print(f"total={total}")
print(f"pass={keep}")
for g in ["BRCA1", "BRCA2", "EGFR", "KRAS", "TP53"]:
    print(f"{g}={counts.get(g, 0)}")

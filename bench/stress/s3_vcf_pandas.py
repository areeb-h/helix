# S3 — VCF analysis, pandas. Parse the INFO GENE field, filter QUAL>50, group.
import pandas as pd

rows = []
with open("data/variants.vcf") as f:
    for line in f:
        if line.startswith("#"):
            continue
        c = line.rstrip("\n").split("\t")
        info = dict(kv.split("=") for kv in c[7].split(";"))
        rows.append((float(c[5]), info["GENE"]))

df = pd.DataFrame(rows, columns=["qual", "gene"])
keep = df[df.qual > 50.0]
counts = keep.groupby("gene").size()

print(f"total={len(df)}")
print(f"pass={len(keep)}")
for g in ["BRCA1", "BRCA2", "EGFR", "KRAS", "TP53"]:
    print(f"{g}={int(counts.get(g, 0))}")

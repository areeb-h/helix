#!/usr/bin/env python3
"""Deterministic data generator for the cross-language benchmark suite.

Writes the on-disk inputs the data-bound workloads read (B3 CSV, B6 VCF). The
compute-bound workloads (B1/B2/B4/B5/B7) generate their inputs in-process, so
they need no files. Deterministic (fixed formulae / seed) so every run and every
language sees identical bytes.

    python3 gen_data.py            # full sizes (1M-row CSV, 50k-variant VCF)
    python3 gen_data.py --scale 0.01   # 1% sizes, for a fast smoke test
"""
import argparse
import os
import random

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "data")


def gen_csv(rows: int) -> None:
    """1M-row table: a `group` key (0..49) and a `value` (deterministic float)."""
    path = os.path.join(DATA, "big.csv")
    with open(path, "w", newline="") as f:
        f.write("group,value\n")
        for i in range(rows):
            grp = i % 50
            val = (i * 2654435761) % 100000 / 1000.0  # Knuth-mix, stable
            f.write(f"{grp},{val}\n")
    print(f"  wrote {path} ({rows} rows)")


def gen_vcf(n: int) -> None:
    """`n`-variant VCF with a deterministic QUAL column (the B6 target)."""
    path = os.path.join(DATA, "big.vcf")
    rng = random.Random(42)
    bases = "ACGT"
    with open(path, "w") as f:
        f.write("##fileformat=VCFv4.2\n")
        f.write('##INFO=<ID=DP,Number=1,Type=Integer,Description="Depth">\n')
        f.write("#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n")
        for i in range(n):
            pos = 1 + i * 13
            ref = bases[rng.randrange(4)]
            alt = bases[rng.randrange(4)]
            qual = round(20.0 + (i % 800) / 10.0, 1)  # deterministic, mean ~59.95
            f.write(f"1\t{pos}\trs{i}\t{ref}\t{alt}\t{qual}\tPASS\tDP={30 + i % 50}\n")
    print(f"  wrote {path} ({n} variants)")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--scale", type=float, default=1.0, help="fraction of full size")
    args = ap.parse_args()
    os.makedirs(DATA, exist_ok=True)
    rows = max(1, int(1_000_000 * args.scale))
    variants = max(1, int(50_000 * args.scale))
    print(f"generating data (scale={args.scale}):")
    gen_csv(rows)
    gen_vcf(variants)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Deterministic data generator for the cross-language STRESS suite.

Writes the on-disk inputs the data-bound workloads read:
  data/genome.fa   one FASTA record, N bases of ACGT          (S1 k-mers)
  data/reads.fq    M reads x L bp with Phred quality          (S2 fastq)
  data/variants.vcf  V variants: QUAL + INFO GENE/DP          (S3 vcf)

Every byte is a deterministic function of an index (a fixed LCG), so every run
and every language sees identical input. Scale with --scale.

    python3 gen.py                 # full sizes
    python3 gen.py --scale 0.05    # 5% sizes, fast smoke test
"""
import argparse
import os

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "data")
BASES = "ACGT"


def lcg(seed: int):
    """A bare 64-bit LCG (Knuth MMIX constants); yields a fresh 64-bit state."""
    state = seed & 0xFFFFFFFFFFFFFFFF
    while True:
        state = (state * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
        yield state


def gen_genome(n: int) -> None:
    path = os.path.join(DATA, "genome.fa")
    g = lcg(0xC0FFEE)
    seq = bytearray(n)
    for i in range(n):
        seq[i] = ord(BASES[next(g) >> 62])
    with open(path, "wb") as f:
        f.write(b">chr1 synthetic\n")
        for i in range(0, n, 70):
            f.write(seq[i:i + 70])
            f.write(b"\n")
    print(f"  wrote {path} ({n} bp)")


def gen_reads(m: int, length: int = 150) -> None:
    path = os.path.join(DATA, "reads.fq")
    g = lcg(0x5EED)
    with open(path, "w") as f:
        for r in range(m):
            seq = "".join(BASES[next(g) >> 62] for _ in range(length))
            # Phred+33 quality: deterministic scores in [2, 40].
            qual = "".join(chr(33 + 2 + (next(g) >> 58) % 39) for _ in range(length))
            f.write(f"@read{r}\n{seq}\n+\n{qual}\n")
    print(f"  wrote {path} ({m} reads x {length} bp)")


def gen_vcf(v: int) -> None:
    path = os.path.join(DATA, "variants.vcf")
    g = lcg(0xBEEF)
    genes = ["BRCA1", "BRCA2", "TP53", "EGFR", "KRAS"]
    with open(path, "w") as f:
        f.write("##fileformat=VCFv4.2\n")
        f.write('##INFO=<ID=DP,Number=1,Type=Integer,Description="Depth">\n')
        f.write('##INFO=<ID=GENE,Number=1,Type=String,Description="Gene">\n')
        f.write("#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n")
        for i in range(v):
            pos = 1 + i * 13
            ref = BASES[next(g) >> 62]
            alt = BASES[next(g) >> 62]
            qual = round(10.0 + (next(g) >> 54) % 900 / 10.0, 1)  # [10.0, 100.0)
            dp = 10 + (next(g) >> 58) % 90
            gene = genes[(next(g) >> 61) % 5]
            f.write(f"1\t{pos}\trs{i}\t{ref}\t{alt}\t{qual}\tPASS\tDP={dp};GENE={gene}\n")
    print(f"  wrote {path} ({v} variants)")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--scale", type=float, default=1.0)
    args = ap.parse_args()
    os.makedirs(DATA, exist_ok=True)
    s = args.scale
    print(f"generating data (scale={s}):")
    gen_genome(max(1, int(10_000_000 * s)))
    gen_reads(max(1, int(200_000 * s)))
    gen_vcf(max(1, int(200_000 * s)))


if __name__ == "__main__":
    main()

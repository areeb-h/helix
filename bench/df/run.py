#!/usr/bin/env python3
"""Time every DataFrame verb on both engines, from one dual-engine binary.

    python3 bench/df/run.py [--reps 5] [--bin ./target/dual/gate/helix]

Requires a DUAL-ENGINE build, the same one `scripts/dfdiff.sh` wants:

    CARGO_TARGET_DIR=target/dual cargo build --profile gate --features native-df

WHY ONE BINARY, TWO ENGINES. Building two binaries would confound the engine with
everything else that differs between two builds. `HELIX_DF_ENGINE` picks the engine
inside a single binary, so the interpreter, the CSV path, the value types and the
printing are literally the same code -- the only difference is the thing under test.

EVERY PROGRAM CONSUMES ITS RESULT, and that is not a stylistic choice. A lazy
engine answers `.count()` without materialising anything, so `where(...).count()`,
`join(...).count()` and `sort(...).count()` all time how fast it DECLINED the
work. This suite reported polars at 5.7 ms for a join that costs 84 ms, and at
0.11 ms for a sort that costs 74 ms — turning two native wins into apparent
losses. The tell is always the same: sub-linear growth. Polars' "join" grew 1.12x
for 4x the rows, which no join does.

So every program ends in `.column(...)`, which forces the result out. `read.helix`
is the deliberate exception: it exists precisely to measure that shortcut, and
`parse.helix` is its honest counterpart.

THE OUTPUTS ARE COMPARED, NOT JUST THE TIMES. A benchmark that does not check what
the program printed will happily report that a syntax error runs in 0.00s; this
project has been bitten by exactly that. Here a divergence between engines is a
FINDING -- `scripts/dfdiff.sh` says the tracked corpus agrees byte-for-byte, so a
disagreement at 200k rows is a scale-dependent bug worth more than any timing.
"""

import argparse
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
ENGINES = ["polars", "native"]

# `read` is NOT a read. It is `read_csv(...).count()`, and a LAZY engine answers
# that by counting record separators without parsing a single field -- so it
# measures how cheaply an engine can decline to do the work, not how fast it reads.
# `parse` is the honest read measurement: it forces every column to materialise.
#
# This distinction is not pedantic. Read against `read` alone, polars looks 1.2x
# faster at "reading"; measured on `parse`, native reads the same file ~9x faster,
# because polars was never reading it. The first version of this file subtracted
# `read` from every other program as a shared floor, which is invalid across two
# engines that do not agree on what the floor contains.
PROGRAMS = ["read", "parse", "filter", "with", "sort", "group", "join", "unique"]


def load1():
    with open("/proc/loadavg", encoding="utf-8") as f:
        return float(f.read().split()[0])


def once(binary, prog, engine):
    env = os.environ.copy()
    env["HELIX_DF_ENGINE"] = engine
    t0 = time.perf_counter()
    res = subprocess.run(
        [binary, os.path.join(HERE, prog + ".helix")],
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        stdin=subprocess.DEVNULL,
    )
    return time.perf_counter() - t0, res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--bin", default="./target/dual/gate/helix")
    ap.add_argument("--max-load", type=float, default=1.5)
    a = ap.parse_args()

    binary = os.path.join(ROOT, a.bin) if not os.path.isabs(a.bin) else a.bin
    if not os.path.exists(binary):
        print("no dual-engine binary at %s" % binary)
        print("build it:  CARGO_TARGET_DIR=target/dual cargo build --profile gate --features native-df")
        return 2

    l1 = load1()
    if l1 >= a.max_load:
        print("REFUSING TO MEASURE -- load average is %.2f (limit %.2f)." % (l1, a.max_load))
        print("Timings taken under load are not comparable to timings taken without it,")
        print("and a number that cannot be compared is not worth the minutes it costs.")
        return 2

    print("dual binary: %s" % binary)
    print("load %.2f, min-of-%d\n" % (l1, a.reps))
    print("%-9s %11s %11s %9s   %s" % ("verb", "polars", "native", "native/", "output"))
    print("%-9s %11s %11s %9s" % ("", "(s)", "(s)", "polars"))
    print("-" * 64)

    best = {}
    diverged, failed = [], []
    for prog in PROGRAMS:
        outs, times = {}, {}
        for engine in ENGINES:
            once(binary, prog, engine)  # warm the page cache; discarded
            runs = [once(binary, prog, engine) for _ in range(a.reps)]
            bad = next((r for _, r in runs if r.returncode != 0), None)
            if bad is not None:
                print("%-9s FAILED on %s: %s" % (prog, engine, bad.stderr.strip()[:120]))
                failed.append((prog, engine))
                break
            ts = sorted(t for t, _ in runs)
            # Min AND spread. The min is the estimate; the spread is what says
            # whether a difference between two mins means anything at all.
            times[engine] = (ts[0], ts[-1] - ts[0])
            outs[engine] = runs[0][1].stdout
        if len(times) != len(ENGINES):
            continue

        # The correctness check, before any timing is reported. Two engines that
        # disagree are not two speeds of the same answer.
        if outs["polars"] != outs["native"]:
            diverged.append(prog)
            print("%-9s *** DIVERGENCE ***" % prog)
            print("            polars: %r" % outs["polars"].strip()[:80])
            print("            native: %r" % outs["native"].strip()[:80])
            continue

        best[prog] = times
        ratio = times["native"][0] / times["polars"][0]
        print(
            "%-9s %8.3f±%.3f %8.3f±%.3f %8.2fx   %s"
            % (
                prog,
                times["polars"][0],
                times["polars"][1],
                times["native"][0],
                times["native"][1],
                ratio,
                outs["polars"].strip()[:20],
            )
        )

    # NO NET-OF-READ COLUMN. An earlier version subtracted `read` from every other
    # program to isolate "what the verb costs". Two things killed it, and both are
    # worth remembering before anyone adds it back:
    #
    #   1. It is not a shared floor. `read` is a row count, which polars answers
    #      lazily without parsing and native answers by parsing everything -- so
    #      the subtraction removes a different quantity from each engine.
    #   2. It is a difference of two ~0.6s numbers with ~0.2s of run-to-run
    #      spread, so it reported the SAME unchanged binary at 0.240s and then
    #      0.118s. A statistic that unstable is not evidence.
    #
    # The totals below are what a user actually waits for, and `parse` is the
    # honest read measurement. That is the whole report.
    if "read" in best and "parse" in best:
        print()
        print("reading the same file, two ways:")
        print(
            "  read  (count only, no field parsed)   polars %6.3fs   native %6.3fs"
            % (best["read"]["polars"][0], best["read"]["native"][0])
        )
        print(
            "  parse (every column materialised)     polars %6.3fs   native %6.3fs"
            % (best["parse"]["polars"][0], best["parse"]["native"][0])
        )
        print("  -> `read` measures laziness; `parse` measures the CSV reader.")

    print()
    if diverged:
        print("DIVERGENCES (%d) -- these outrank every timing above:" % len(diverged))
        for d in diverged:
            print("   ", d)
    if failed:
        print("FAILED (%d): %s" % (len(failed), ", ".join("%s/%s" % f for f in failed)))
    if not diverged and not failed:
        print("both engines produced identical output for all %d programs" % len(best))
    return 1 if (diverged or failed) else 0


if __name__ == "__main__":
    sys.exit(main())

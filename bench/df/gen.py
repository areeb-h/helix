#!/usr/bin/env python3
"""Generate the DataFrame benchmark's input, deterministically.

    python3 bench/df/gen.py [rows]

The workload programs in this directory read FIXED paths under `/tmp/helix_dfbench`,
so they stay readable and hand-runnable; changing the size means regenerating in place
rather than threading a parameter through seven programs.

DETERMINISM IS THE POINT. A seeded generator means the two engines see byte-identical
input, so a difference in their OUTPUT is a real divergence rather than a difference in
what they were asked. It also makes an n-vs-4n comparison meaningful: 4n is the same
distribution with more rows, not a different dataset.
"""

import os
import random
import sys

DIR = "/tmp/helix_dfbench"
MAIN = os.path.join(DIR, "main.csv")
DIM = os.path.join(DIR, "dim.csv")

# Distinct join keys, and hence the dimension table's row count. Held CONSTANT as n
# grows so the join's build side stays fixed and the probe side is what scales --
# otherwise n-vs-4n would be measuring two things at once.
KEYS = 1000


def main():
    rows = int(sys.argv[1]) if len(sys.argv) > 1 else 200_000
    groups = max(1, rows // 100)
    rnd = random.Random(20260830)

    os.makedirs(DIR, exist_ok=True)
    with open(MAIN, "w", encoding="utf-8", newline="\n") as f:
        f.write("id,g,x,y,k\n")
        for i in range(rows):
            # `x` and `y` get a fixed number of decimals so the CSV text is stable and
            # every engine parses the identical literal -- no float formatting drift.
            f.write(
                "%d,g%d,%.6f,%.6f,%d\n"
                % (i, rnd.randrange(groups), rnd.random(), rnd.random(), rnd.randrange(KEYS))
            )

    with open(DIM, "w", encoding="utf-8", newline="\n") as f:
        f.write("k,label\n")
        for k in range(KEYS):
            f.write("%d,label_%d\n" % (k, k))

    print("rows=%d groups=%d keys=%d  ->  %s" % (rows, groups, KEYS, MAIN))
    print("main.csv %.1f MB" % (os.path.getsize(MAIN) / 1e6))


if __name__ == "__main__":
    main()

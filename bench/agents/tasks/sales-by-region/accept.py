"""Hidden check: run on a different month's file (new regions, ties, refunds, many rows)."""
import random
from checks import needs, same_on_engines, write


def expected(rows):
    totals = {}
    for _, region, _, amount in rows:
        totals[region] = totals.get(region, 0) + round(amount * 100)
    order = sorted(totals.items(), key=lambda kv: (-kv[1], kv[0]))
    return "\n".join("%s: %s%d.%02d" % (r, "-" if c < 0 else "", abs(c) // 100, abs(c) % 100) for r, c in order)


def check(ws, helix):
    failures = needs(ws, "report.helix")
    if failures:
        return failures
    rng = random.Random(68)
    regions = ["Atlantic", "Central", "Mountain", "Pacific", "Gulf", "Lakes"]
    rows = []
    for i in range(400):
        rows.append(("2026-11-%02d" % (1 + i % 30), rng.choice(regions), rng.choice(["a", "b"]),
                     rng.randint(-5000, 30000) / 100))
    # Two regions whose totals tie exactly, to test the tie order.
    rows += [("2026-11-30", "Zeta", "a", 1000.00), ("2026-11-30", "Alpha", "a", 600.00),
             ("2026-11-30", "Alpha", "b", 400.00)]
    write(ws, "sales.csv", "date,region,product,amount\n" +
          "".join("%s,%s,%s,%.2f\n" % r for r in rows))
    return same_on_engines(helix, ws, ["run", "report.helix"], want=expected(rows))

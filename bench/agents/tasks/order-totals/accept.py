"""Hidden check: a newer export; totals.json compared as data (key order does not matter)."""
import json
import os
import random
from checks import ENGINES, needs, read_json, run, write


def expected(orders):
    out = {}
    for o in orders:
        if o["status"] != "cancelled":
            out[o["customer"]] = out.get(o["customer"], 0) + sum(round(i["price"] * 100) * i["qty"] for i in o["items"])
    return {c: cents / 100 for c, cents in out.items()}


def check(ws, helix):
    failures = needs(ws, "totals.helix")
    if failures:
        return failures
    rng = random.Random(68)
    customers = ["ada", "bob", "cy", "dee", "eve", "fay", "gus"]
    orders = []
    for i in range(300):
        orders.append({"id": i, "customer": rng.choice(customers),
                       "status": rng.choice(["paid", "shipped", "cancelled"]),
                       "items": [{"sku": "S%d" % rng.randint(1, 50), "qty": rng.randint(1, 5),
                                  "price": rng.randint(1, 20000) / 100} for _ in range(rng.randint(0, 4))]})
    orders.append({"id": 999, "customer": "only-cancelled", "status": "cancelled", "items": []})
    orders.append({"id": 1000, "customer": "zero", "status": "paid", "items": []})
    write(ws, "orders.json", json.dumps(orders))
    want = expected(orders)
    for engine, env in ENGINES:
        out = os.path.join(ws, "totals.json")
        if os.path.exists(out):
            os.remove(out)
        rc, _, err = run(helix, ws, ["run", "totals.helix"], env=env)
        if rc != 0:
            return ["`helix run totals.helix` on the %s exited %s: %s" % (engine, rc, err.strip()[-300:])]
        try:
            got = read_json(ws, "totals.json")
        except (OSError, ValueError) as e:
            return ["totals.json is missing or not JSON on the %s: %s" % (engine, e)]
        if not isinstance(got, dict) or set(got) != set(want):
            return ["totals.json has customers %s, expected %s" % (sorted(got) if isinstance(got, dict) else got, sorted(want))]
        wrong = [c for c in want if not isinstance(got[c], (int, float)) or abs(got[c] - want[c]) > 0.005]
        if wrong:
            return ["wrong totals for %s (e.g. %s: got %r, expected %.2f)" % (wrong[:5], wrong[0], got[wrong[0]], want[wrong[0]])]
    return []

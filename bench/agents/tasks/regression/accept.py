"""Hidden check: other data sets, including a negative slope and a poor fit."""
import random
from checks import needs, same_on_engines, write


def expected(pts):
    n = len(pts)
    mx = sum(x for x, _ in pts) / n
    my = sum(y for _, y in pts) / n
    sxy = sum((x - mx) * (y - my) for x, y in pts)
    sxx = sum((x - mx) ** 2 for x, _ in pts)
    slope = sxy / sxx
    icpt = my - slope * mx
    ss_res = sum((y - (slope * x + icpt)) ** 2 for x, y in pts)
    ss_tot = sum((y - my) ** 2 for _, y in pts)
    return "slope: %.4f\nintercept: %.4f\nr2: %.4f" % (slope, icpt, 1 - ss_res / ss_tot)


def check(ws, helix):
    failures = needs(ws, "fit.helix")
    if failures:
        return failures
    rng = random.Random(68)
    for slope, icpt, noise, n in ((-1.75, 40.0, 0.5, 50), (0.3, -2.0, 25.0, 200), (12.0, 0.25, 3.0, 7)):
        pts = []
        for _ in range(n):
            x = round(rng.uniform(-20, 80), 2)
            pts.append((x, round(slope * x + icpt + rng.gauss(0, noise), 3)))
        write(ws, "points.csv", "x,y\n" + "".join("%.2f,%.3f\n" % p for p in pts))
        # Recompute from the rounded text the program will read.
        pts = [(float("%.2f" % x), float("%.3f" % y)) for x, y in pts]
        failures += same_on_engines(helix, ws, ["run", "fit.helix"], want=expected(pts))
    return failures

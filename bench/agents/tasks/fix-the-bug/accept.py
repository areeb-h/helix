"""Hidden check: the agent's own tests pass on three engines, the two bugs are fixed on many
inputs, and the functions nobody reported keep their old answers."""
import random
from checks import helix_test, needs, same_on_engines, write


def fmt(v):
    if v is None:
        return "missing"
    if isinstance(v, float):
        return repr(v) if v != int(v) else "%.1f" % v
    return str(v)


def check(ws, helix):
    failures = needs(ws, "stats.helix", "stats_test.helix")
    if failures:
        return failures
    failures += helix_test(helix, ws, "stats_test.helix")
    rng = random.Random(68)
    cases = [[1, 2, 3, 4], [1, 2], [7], [], [5, 5], [2, 9, 4, 4, 1, 8]]
    cases += [[rng.randint(-50, 50) for _ in range(rng.randint(1, 12))] for _ in range(20)]
    lines, want = ["import stats.{mean, median, spread, above_mean}"], []
    for i, xs in enumerate(cases):
        lit = "[%s]" % ", ".join(map(str, xs))
        lines.append('print("{mean(%s)} {median(%s)} {spread(%s)} {above_mean(%s)}")' % (lit, lit, lit, lit))
        if not xs:
            want.append("missing missing missing 0")
            continue
        s = sorted(xs)
        n = len(s)
        mean = sum(xs) / n
        med = s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2
        want.append("%s %s %s %d" % (fmt(mean), fmt(med), fmt(max(xs) - min(xs)), sum(1 for x in xs if x > mean)))
    write(ws, "zz_accept_driver.helix", "\n".join(lines) + "\n")
    failures += same_on_engines(helix, ws, ["run", "zz_accept_driver.helix"], want="\n".join(want))
    return failures

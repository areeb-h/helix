"""Hidden check: a driver imports the module and replays random request times; the answers
are compared with a model of the specification."""
import random
from checks import needs, same_on_engines, write


def model(capacity, refill, times):
    tokens, since, out = capacity, None, []
    for now in times:
        since = now if since is None else since
        earned = (now - since) // refill
        tokens += earned
        if tokens >= capacity:
            tokens, since = capacity, now
        else:
            since += earned * refill
        ok = tokens > 0
        tokens -= ok
        out.append("true" if ok else "false")
    return " ".join(out)


def check(ws, helix):
    failures = needs(ws, "limiter.helix")
    if failures:
        return failures
    rng = random.Random(68)
    cases = [(2, 1000, [0, 0, 0, 999, 1000])]
    for _ in range(12):
        cap, refill = rng.randint(1, 5), rng.choice([1, 7, 100, 250, 1000])
        t, times = rng.randint(0, 5000), []
        for _ in range(rng.randint(1, 60)):
            t += rng.choice([0, 0, 1, refill - 1, refill, refill + 1, rng.randint(0, 3 * refill)])
            times.append(t)
        cases.append((cap, refill, times))
    lines, want = ["import limiter.{new, allow}", ""], []
    for n, (cap, refill, times) in enumerate(cases):
        lines.append("r%d = [%s].reduce((new(%d, %d), []), (st, t) => do {" % (n, ", ".join(map(str, times)), cap, refill))
        lines.append("  step = allow(st[0], t)")
        lines.append("  (step[0], st[1].concat([step[1]]))")
        lines.append("})")
        lines.append('print(r%d[1].map((b) => "{b}").join(" "))' % n)
        want.append(model(cap, refill, times))
    write(ws, "zz_accept_driver.helix", "\n".join(lines) + "\n")
    return same_on_engines(helix, ws, ["run", "zz_accept_driver.helix"], want="\n".join(want))

"""Hidden check: other files, other --top values, the default, and --help."""
import random
import re
from checks import needs, run, same_on_engines, write


def expected(text, top):
    counts = {}
    for w in re.split(r"[^a-z]+", text.lower()):
        if w:
            counts[w] = counts.get(w, 0) + 1
    order = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:top]
    return "\n".join("%s %d" % kv for kv in order)


def check(ws, helix):
    failures = needs(ws, "words.helix")
    if failures:
        return failures
    rng = random.Random(68)
    vocab = ["alpha", "beta", "gamma", "delta", "Epsilon", "zeta", "eta", "theta", "iota", "kappa",
             "lambda", "mu", "nu", "xi", "omicron"]
    seps = [" ", "  ", ", ", ". ", "\n", "\t", "--", "'", "3", "(", ")!", "; "]
    text = "".join(rng.choice(vocab) + rng.choice(seps) for _ in range(3000))
    text += "Don't stop; it's 2026 -- café"
    write(ws, "big.txt", text)
    write(ws, "tiny.txt", "One one ONE two\n")
    write(ws, "empty.txt", "")
    for path, top in (("big.txt", 5), ("big.txt", 40), ("tiny.txt", 10), ("big.txt", 1)):
        failures += same_on_engines(helix, ws, ["run", "words.helix", "--path", path, "--top", str(top)],
                                    want=expected(open(ws + "/" + path).read(), top))
    failures += same_on_engines(helix, ws, ["run", "words.helix", "--path", "big.txt"], want=expected(text, 10))
    failures += same_on_engines(helix, ws, ["run", "words.helix", "--path", "empty.txt"], want="")
    rc, out, err = run(helix, ws, ["run", "words.helix", "--help"])
    if rc != 0 or "--path" not in out + err or "--top" not in out + err:
        failures.append("`helix run words.helix --help` did not describe --path and --top")
    return failures

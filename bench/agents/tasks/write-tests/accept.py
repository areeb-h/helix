"""Hidden check: the agent's tests pass on the real slug.helix (three engines) and fail on
every mutant, each of which breaks one stated rule."""
import os
import shutil
from checks import helix_test, needs, run

HERE = os.path.dirname(os.path.abspath(__file__))


def check(ws, helix):
    failures = needs(ws, "slug_test.helix")
    if failures:
        return failures
    shutil.copy(os.path.join(HERE, "files", "slug.helix"), os.path.join(ws, "slug.helix"))
    failures = helix_test(helix, ws, "slug_test.helix")
    if failures:
        return ["the tests fail on the correct slug.helix: " + failures[0]]
    survived = []
    for name in sorted(os.listdir(os.path.join(HERE, "mutants"))):
        shutil.copy(os.path.join(HERE, "mutants", name, "slug.helix"), os.path.join(ws, "slug.helix"))
        rc, _, _ = run(helix, ws, ["test", "slug_test.helix"])
        if rc == 0:
            survived.append(name)
    shutil.copy(os.path.join(HERE, "files", "slug.helix"), os.path.join(ws, "slug.helix"))
    if survived:
        failures.append("the tests pass on mutants they should catch: %s" % ", ".join(survived))
    return failures

"""What acceptance checks share: running Helix on the agent's files, and comparing answers.

Every check is `check(ws, helix) -> [failure, ...]`: an empty list is a pass. A check runs the
agent's program with the real binary, outside the agent's run, on all three engines where the
answer is a program's output — an answer only one engine gives is not an answer.
"""

import json
import os
import subprocess

ENGINES = [("jit", {}), ("vm", {"HELIX_NOJIT": "1"}), ("tree-walker", {"HELIX_NOVM": "1"})]


def run(helix, ws, args, env=None, timeout=60, stdin=None):
    """`helix <args>` in `ws`. Returns (exit code, stdout, stderr); a timeout is exit code None."""
    try:
        p = subprocess.run([helix] + list(args), cwd=ws, capture_output=True, timeout=timeout,
                           env=dict(os.environ, **(env or {})),
                           input=stdin.encode() if stdin is not None else None,
                           stdin=None if stdin is not None else subprocess.DEVNULL)
        return p.returncode, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")
    except subprocess.TimeoutExpired:
        return None, "", "timed out after %ss" % timeout


def needs(ws, *names):
    """Failures for each named file the agent was asked for and did not leave."""
    return ["`%s` was not written" % n for n in names if not os.path.exists(os.path.join(ws, n))]


def same_on_engines(helix, ws, args, want=None, timeout=60):
    """Run on all three engines; failures unless every run exits 0 with the same stdout (and,
    when `want` is given, that stdout is `want`, compared line by line with trailing space
    ignored)."""
    failures, outs = [], {}
    for engine, env in ENGINES:
        rc, out, err = run(helix, ws, args, env=env, timeout=timeout)
        if rc != 0:
            failures.append("`helix %s` on the %s exited %s: %s" % (" ".join(args), engine, rc, err.strip()[-300:]))
        outs[engine] = out
    if failures:
        return failures
    if len(set(outs.values())) > 1:
        return ["the three engines printed different output for `helix %s`" % " ".join(args)]
    if want is not None:
        got = [l.rstrip() for l in outs["jit"].strip().splitlines()]
        exp = [l.rstrip() for l in want.strip().splitlines()]
        if got != exp:
            return ["`helix %s` printed:\n%s\nexpected:\n%s" % (" ".join(args), "\n".join(got[:20]), "\n".join(exp[:20]))]
    return []


def helix_test(helix, ws, path, timeout=120):
    """`helix test <path>` on all three engines (`--engines`); failures unless it passes."""
    rc, out, err = run(helix, ws, ["test", "--engines", path], timeout=timeout)
    if rc != 0:
        return ["`helix test --engines %s` failed: %s" % (path, (out + err).strip()[-600:])]
    return []


def write(ws, name, text):
    with open(os.path.join(ws, name), "w") as fh:
        fh.write(text)


def read_json(ws, name):
    with open(os.path.join(ws, name)) as fh:
        return json.load(fh)

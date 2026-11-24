#!/usr/bin/env python3
"""deepvalues.py — every operation, on a value nested as deep as a program can make one.

    python3 scripts/deepvalues.py [--bin target/gate/helix] [--jobs 4] [--depth 100000]
    python3 scripts/deepvalues.py --selftest     # prove each oracle can still fail
    python3 scripts/deepvalues.py --list         # print the programs without running them

WHY THIS EXISTS. A Helix program can build a value nested as deep as its memory allows: a
tail-recursive loop that wraps an accumulator — `build(n - 1, [acc])`, `{next: acc}` —
makes a million levels in well under a second. Until src/deep.rs, everything the runtime
did to such a value recursed once per level on the native stack, and the process died with
`fatal runtime error: stack overflow` on all three engines: printing it, interpolating it,
`==`, `to_json`, `unique`, `frequencies`, sorting tuples, arithmetic over nested arrays,
rendering a table — 87 operations by the first count — and, at a few million levels (a
chain of dicts far sooner), merely letting the value go out of scope. Two of the walkers
were also quadratic in depth (each level copied the text below it) and one was
exponential (`tensor` measured the first row twice per level), so a stack fix alone would
have traded a crash for a hang.

No other generator in this tree makes such a value. `stranger.py` and `opfuzz.py` emit
programs a person would type, the differential fuzzers build ASTs, and tests/corpus is
hand-written; none of them nests a VALUE more than a few levels. This one does nothing
else: it builds a deep value of each container kind, applies each operation that takes
one, and runs every program on all three engines on a deliberately small evaluator stack
(`HELIX_STACK_MB`, default 4), so a walk that recursed per level overflows at a depth that
is quick to build.

Four oracles, all hard:

  1. NEVER-ABORT. Exit code in {0, 1}; stderr free of `panicked at`, `fatal runtime
     error`, `stack overflow`, `memory allocation` and `Aborted`. ADR 0024: a limit is an
     error, never a signal.
  2. ENGINES-AGREE. (exit, stdout, stderr) byte-identical on the JIT, the VM and the
     tree-walker.
  3. IN-TIME. Every run finishes inside --timeout seconds. At depth 100k a walk that is
     quadratic in depth takes hours, so a hang here is an algorithm, not a slow machine.
  4. AS-EXPECTED. Each operation states what it must do with a deep value: `ok` (exit 0 —
     deep values are SUPPORTED, so a future "fix" that refuses depth with an error fails
     here) or `error` (exit 1 with a clean message — e.g. `parse_json`, whose parser stops
     at 128 levels by design).

Exit status: non-zero if any oracle fails, or if no program ran at all.
"""

import argparse
import concurrent.futures
import os
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ENGINES = [("jit", {}), ("vm", {"HELIX_NOJIT": "1"}), ("tw", {"HELIX_NOVM": "1"})]
ABORT_MARKS = [b"panicked at", b"fatal runtime error", b"stack overflow", b"memory allocation", b"Aborted"]

# How each container kind wraps the accumulator once. `w` is built the same way, so
# comparisons walk all the way down before they can decide.
SHAPES = {
    "array": "[acc]",
    "pair": "[acc, 1]",
    "record": "{next: acc}",
    "tuple": "(acc, 1)",
    "dict": 'dict().insert("k", acc)',
    "closure": "(x) => if x then acc else 0",
    "mixed": 'if n % 3 == 0 then [acc] else if n % 3 == 1 then {next: acc} else (acc, n)',
}

# (name, program tail, shapes it applies to, expectation). `v` and `w` are deep values of
# the same shape; the tail must print something deterministic.
ALL = set(SHAPES)
DATA = ALL - {"closure"}
OPS = [
    ("drop", 'print("built")', ALL, "ok"),
    ("print", "print(v)", DATA, "ok"),
    ("interpolate", 'print("{v}".length())', DATA, "ok"),
    ("eq", "print(v == w)", ALL, "ok"),
    ("ne", "print(v != w)", ALL, "ok"),
    ("eq-self", "print(v == v)", ALL, "ok"),
    ("try-eq", "print(try (v == w))", ALL, "ok"),
    ("assert-eq", "assert_eq(v, w)\nprint(1)", DATA, "ok"),
    ("match-guard", "print(match v { x if x == w => 1, _ => 0 })", ALL, "ok"),
    ("contains", "print([v].contains(w))", ALL, "ok"),
    ("index-of", "print([v].index_of(w))", ALL, "ok"),
    ("unique", "print([v, w].unique().count())", ALL, "ok"),
    ("frequencies", "print([v, w].frequencies().count())", DATA, "ok"),
    ("count-where", "print([v, w].count_where((x) => x == w))", ALL, "ok"),
    ("to-json", "print(v.to_json().length())", DATA, "ok"),
    ("write-json", 'print([v].write_json("/dev/null"))', DATA, "ok"),
    ("json-roundtrip", "print(v.to_json().parse_json() == v)", {"array", "pair", "record"}, "error"),
    ("dict-value", 'print(dict().insert("a", v) == dict().insert("a", w))', ALL, "ok"),
    ("record-field", "print({a: v}.a == w)", ALL, "ok"),
    ("type-of", "print(type_of(v))", ALL, "ok"),
    ("lt", "print(v < w)", {"tuple"}, "ok"),
    ("le", "print(v <= w)", {"tuple"}, "ok"),
    ("sort", "print([w, v].sort().count())", {"tuple"}, "ok"),
    ("sort-by", "print([w, v].sort_by((x) => x).count())", {"tuple"}, "ok"),
    ("min", "print([v, w].min() == v)", {"tuple"}, "ok"),
    ("argmax", "print([v, w].argmax())", {"tuple"}, "ok"),
    ("add", "print((v + 1) == v)", {"array", "pair"}, "ok"),
    ("mul", "print((v * 2) == v)", {"array", "pair"}, "ok"),
    ("abs", "print(abs(v) == v)", {"array", "pair"}, "ok"),
    ("sign", "print(sign(v) == v)", {"array", "pair"}, "ok"),
    ("sqrt", "print(sqrt(v) == v)", {"array", "pair"}, "ok"),
    ("is-nan", "print(is_nan(v) == v)", {"array", "pair"}, "ok"),
    ("tensor", "print(tensor(v).count())", {"array"}, "error"),
    ("flatten", "print([v].flatten().count())", {"array", "pair"}, "ok"),
    ("to-table", "print([{a: v}].to_table().length() > 0)", DATA, "ok"),
    ("to-markdown", "print([{a: v}].to_markdown().length() > 0)", DATA, "ok"),
    ("to-html", "print([{a: v}].to_html().length() > 0)", DATA, "ok"),
    ("write-csv", 'print([{a: v}].write_csv("/dev/null"))', DATA, "ok"),
]


def programs(depth):
    out = []
    for shape, wrap in SHAPES.items():
        build = "fn build(n, acc) = if n == 0 then acc else build(n - 1, %s)\n" % wrap
        for name, tail, shapes, expect in OPS:
            if shape not in shapes:
                continue
            src = build + "v = build(%d, 1)\nw = build(%d, 1)\n%s\n" % (depth, depth, tail)
            out.append(("%s/%s" % (shape, name), src, expect))
    return out


def run_one(binary, path, stack_mb, timeout, extra):
    env = dict(os.environ, HELIX_STACK_MB=str(stack_mb), **extra)
    start = time.monotonic()
    try:
        p = subprocess.run([binary, path], capture_output=True, timeout=timeout, env=env,
                           stdin=subprocess.DEVNULL)
        return (p.returncode, p.stdout, p.stderr, time.monotonic() - start)
    except subprocess.TimeoutExpired:
        return ("timeout", b"", b"", timeout)


def judge(name, expect, results):
    """`results` maps engine -> (exit, stdout, stderr, seconds). Returns failures."""
    fails = []
    for eng, (rc, out, err, _) in results.items():
        if rc == "timeout":
            fails.append((3, "%s: no answer within the time limit on %s" % (name, eng)))
            continue
        if rc not in (0, 1) or any(m in err for m in ABORT_MARKS):
            fails.append((1, "%s: aborted on %s (exit %s): %s" % (name, eng, rc, err.strip()[-200:])))
    triples = {eng: r[:3] for eng, r in results.items() if r[0] != "timeout"}
    if len(set(triples.values())) > 1:
        fails.append((2, "%s: engines disagree: %s" % (
            name, "; ".join("%s=%s %r" % (e, t[0], (t[1] + t[2])[-120:]) for e, t in triples.items()))))
    want = {"ok": 0, "error": 1}[expect]
    for eng, (rc, _, err, _) in results.items():
        if rc in (0, 1) and rc != want:
            fails.append((4, "%s: expected exit %d on %s, got %d: %s" % (name, want, eng, rc, err.strip()[-200:])))
    return fails


def selftest(binary, tmp, stack_mb, timeout):
    """An oracle that never fires is indistinguishable from one that is broken."""
    ok = True

    def check(label, cond):
        nonlocal ok
        print("  %-52s %s" % (label, "ok" if cond else "BROKEN"))
        ok = ok and cond

    clean = {e: (0, b"1\n", b"", 0.1) for e, _ in ENGINES}
    check("a clean result passes", judge("x", "ok", clean) == [])
    r = dict(clean, vm=(-6, b"", b"thread 'main' has overflowed its stack\nfatal runtime error: stack overflow\n", 0.1))
    check("oracle 1 sees a stack-overflow abort", any(f[0] == 1 for f in judge("x", "ok", r)))
    r = dict(clean, tw=(1, b"", b"thread 'main' panicked at src/value.rs:1\n", 0.1))
    check("oracle 1 sees `panicked at` on exit 1", any(f[0] == 1 for f in judge("x", "ok", r)))
    r = dict(clean, jit=(0, b"2\n", b"", 0.1))
    check("oracle 2 sees engines disagree", any(f[0] == 2 for f in judge("x", "ok", r)))
    r = dict(clean, vm=("timeout", b"", b"", timeout))
    check("oracle 3 sees a timeout", any(f[0] == 3 for f in judge("x", "ok", r)))
    r = {e: (1, b"", b"error: nested too deeply\n", 0.1) for e, _ in ENGINES}
    check("oracle 4 sees depth refused where it must work", any(f[0] == 4 for f in judge("x", "ok", r)))
    check("oracle 4 sees success where an error is due", any(f[0] == 4 for f in judge("x", "error", clean)))

    # One real run end to end, so the harness itself (paths, env, decoding) is exercised.
    path = os.path.join(tmp, "selftest.helix")
    with open(path, "w") as fh:
        fh.write(programs(1000)[0][1])
    res = {e: run_one(binary, path, stack_mb, timeout, extra) for e, extra in ENGINES}
    check("a real shallow program passes every oracle", judge("real", "ok", res) == [])
    return 0 if ok else 1


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bin", default=os.path.join(REPO, "target", "gate", "helix"))
    ap.add_argument("--jobs", type=int, default=min(4, os.cpu_count() or 2))
    ap.add_argument("--depth", type=int, default=100_000)
    ap.add_argument("--stack-mb", type=int, default=4,
                    help="evaluator stack (HELIX_STACK_MB); small, so per-level recursion overflows fast")
    ap.add_argument("--timeout", type=float, default=60.0)
    ap.add_argument("--filter", default=None, help="only programs whose name contains this")
    ap.add_argument("--list", action="store_true", help="print the programs and exit")
    ap.add_argument("--selftest", action="store_true", help="prove each oracle can still fail")
    args = ap.parse_args()

    progs = [p for p in programs(args.depth) if not args.filter or args.filter in p[0]]
    if args.list:
        for name, src, expect in progs:
            print("== %s (expect %s)\n%s" % (name, expect, src))
        return 0
    binary = os.path.abspath(args.bin)
    if not os.access(binary, os.X_OK):
        print("deepvalues: no binary at %s (build with `cargo build --profile gate`)" % binary)
        return 2
    with tempfile.TemporaryDirectory(prefix="helix-deep-") as tmp:
        if args.selftest:
            print("deepvalues --selftest: can each oracle still say no?")
            return selftest(binary, tmp, args.stack_mb, args.timeout)
        if not progs:
            print("deepvalues: no programs matched --filter %r; that is a failure, not a pass" % args.filter)
            return 1

        def work(item):
            name, src, expect = item
            path = os.path.join(tmp, name.replace("/", "__") + ".helix")
            with open(path, "w") as fh:
                fh.write(src)
            res = {e: run_one(binary, path, args.stack_mb, args.timeout, extra) for e, extra in ENGINES}
            return name, judge(name, expect, res), max(r[3] for r in res.values())

        start = time.monotonic()
        fails, slowest = [], (0.0, "")
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            for name, f, secs in pool.map(work, progs):
                fails.extend(f)
                slowest = max(slowest, (secs, name))
        print("deepvalues: %d programs x %d engines at depth %d on a %d MB stack, %.0fs (slowest %s, %.1fs)"
              % (len(progs), len(ENGINES), args.depth, args.stack_mb, time.monotonic() - start,
                 slowest[1], slowest[0]))
        names = {1: "NEVER-ABORT", 2: "ENGINES-AGREE", 3: "IN-TIME", 4: "AS-EXPECTED"}
        for k in sorted(names):
            n = sum(1 for f in fails if f[0] == k)
            print("  oracle %d %-14s %s" % (k, names[k], "ok" if n == 0 else "%d FAILING" % n))
        for _, msg in fails[:40]:
            print("    " + msg)
        return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())

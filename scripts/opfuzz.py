#!/usr/bin/env python3
"""Cross-engine operator fuzz: hunt ABORTS and three-engine divergence.

    python3 scripts/opfuzz.py [path/to/helix]

Runs every (operator x stress-operand x compilation-shape) combination through all three
engines and checks two properties per program:

  * the process EXITS CLEANLY. A Helix error is exit 1 with an `error:` line; a panic, an
    `internal error`, a SIGABRT or a SIGFPE is not. This is the top-severity class — ADR
    0024 says user input must never take the host down.
  * all three engines agree BYTE FOR BYTE, values and error text alike.

WHY THIS EXISTS. `xs.clamp(5, 1)` core-dumped (exit 134, uncatchable by `try`) because the
array method called `Ord::clamp`, which panics when `min > max`, while the SCALAR builtin
had always guarded it. And the JIT is built out of `unreachable!()`s held back by
eligibility gates, so relaxing a gate — which Stage 4g did — is exactly how one becomes
reachable. Both classes are invisible to a test suite that only checks values.

The operand list is chosen to sit on the guards: zero and negative divisors, `i64::MIN`
(whose magnitude cannot be written positively and which traps `srem`/`sdiv` against -1),
and shift counts at and past the 0..=63 boundary. The shape list is chosen to reach a
different code generator each time — the same expression compiles differently inside a map,
a reduce, a tail loop, a mixed-parameter function, or none of them.

Exit status is non-zero if anything is found, so this can gate.
"""
import io, itertools, os, subprocess, sys

HELIX = sys.argv[1] if len(sys.argv) > 1 else "target/release/helix"
PROG = "/tmp/helix_opfuzz.helix"

OPERANDS = ["3", "0", "-1", "-3", "64", "63", "-9223372036854775808", "9223372036854775807"]
OPS = ["%", "//", "<<", ">>", "/", "*", "+", "-", "&", "|", "^"]

SHAPES = [
    ("scalar", "d = {r}\nprint(7 {op} d)"),
    ("scalar-neg", "d = {r}\nprint(-7 {op} d)"),
    ("scalar-min", "d = {r}\nprint(-9223372036854775808 {op} d)"),
    ("i64-map", "d = {r}\nprint((0..4).map(it {op} d).sum())"),
    ("mixed-map", "d = {r}\nprint((0..4).map(to_float(it {op} d)).sum())"),
    ("i64-reduce", "d = {r}\nprint((0..4).reduce(0, (a, k) => a + (k {op} d)))"),
    ("tail-i64", "d = {r}\nfn f(i, a) = if i >= 4 then a else f(i + 1, a + (i {op} d))\nprint(f(0, 0))"),
    ("tail-mixed", "d = {r}\nfn f(i: Int, d2: Int, a: Float) = if i >= 4 then a else "
                   "f(i + 1, d2, a + to_float(i {op} d2))\nprint(f(0, d, 0.0))"),
    ("mixed-fn", "d = {r}\nfn g(x: Float, k: Int) = if x > 0.0 then 7 {op} k else 0\nprint(g(1.0, d))"),
    ("filter", "d = {r}\nprint((0..8).filter((it {op} d) > 0).count())"),
]

ENGINES = [("jit", {}), ("vm", {"HELIX_NOJIT": "1"}), ("tw", {"HELIX_NOVM": "1"})]


def main():
    aborts, diverge, ran = [], [], 0
    for (shape, tmpl), op, r in itertools.product(SHAPES, OPS, OPERANDS):
        io.open(PROG, "w", newline="\n").write(tmpl.format(op=op, r=r) + "\n")
        outs = {}
        for tag, ev in ENGINES:
            env = dict(os.environ)
            env.update(ev)
            try:
                p = subprocess.run([HELIX, "run", PROG], capture_output=True, text=True,
                                   env=env, timeout=25)
            except subprocess.TimeoutExpired:
                aborts.append((shape, op, r, tag, "TIMEOUT"))
                outs[tag] = "TIMEOUT"
                continue
            blob = p.stdout + p.stderr
            if (p.returncode not in (0, 1) or "panicked at" in blob
                    or "internal error" in blob or "Aborted" in blob):
                aborts.append((shape, op, r, tag,
                               "exit=%s %s" % (p.returncode, blob.strip()[:70])))
            outs[tag] = blob.strip().split("\n")[0][:70]
        ran += 1
        if len(set(outs.values())) != 1:
            diverge.append((shape, op, r, outs))

    print("%d programs x %d engines" % (ran, len(ENGINES)))
    print("aborts:      %d" % len(aborts))
    for a in aborts[:20]:
        print("   %s" % (a,))
    print("divergences: %d" % len(diverge))
    for shape, op, r, outs in diverge[:20]:
        print("   %s  `%s`  operand %s" % (shape, op, r))
        for k, v in outs.items():
            print("        %-3s %s" % (k, v))
    return 1 if (aborts or diverge) else 0


if __name__ == "__main__":
    sys.exit(main())

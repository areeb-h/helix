#!/usr/bin/env python3
"""stranger.py — what happens when someone who does NOT know Helix types a file.

    python3 scripts/stranger.py [--bin target/gate/helix] [--jobs 8] [--bless]

WHY THIS EXISTS. Every other generator in this tree emits WELL-FORMED Helix. `scripts/
opfuzz.py` walks (operator x operand x compilation-shape) — every program it emits parses.
The differential fuzzers in `src/vm/tests.rs` build ASTs, so they cannot emit a syntax error
even in principle. `tests/corpus` is hand-written Helix. All of them start from the Helix
grammar, so none of them can ever produce `for x in xs:`, a `/* block comment */`, a file
that begins with a UTF-8 BOM, a CSV with a NUL byte in it, or a DataFrame built from a file
on disk. That last one is not hypothetical: the `read_csv(f).where(1)` SIGABRT fixed in
c243040 lived exactly there — in a dependency, reachable only from a real file, invisible to
every generator we had.

And no oracle anywhere asks whether an error message is any GOOD.

So this generates whole FILES the way a newcomer types them — the cross product of Python,
JavaScript, R, MATLAB, Julia, Go and C habits over statement forms, null literals, booleans,
length idioms, comment syntax, string kinds, builtin guesses, indexing and operators — plus
a byte-level axis (BOM, CRLF, NUL, invalid UTF-8, tabs, 4KB line, empty file) and a
malformed-CSV axis. Then it judges the output with four oracles.

  1. NEVER-ABORT (hard). Exit code in {0, 1}; stderr free of `panicked at` / `internal
     error` / `Aborted`. This is the DYNAMIC complement to the static unwrap-budget ratchet
     in tests/cli.rs: that test counts panicking CALLS, this one counts panicking RUNS. The
     budget cannot see a panic inside polars or memchr, because those lines are not in
     src/. This can.
  2. DETERMINISM (hard). Every program runs 3x on each of the 3 engines; all 9
     (exit, stdout, stderr) triples must be byte-identical. Two properties in one: a single
     engine disagreeing with itself is nondeterminism (the only oracle that would have
     caught the non-deterministic Polars error text), and two engines disagreeing with each
     other is the bit-identical-across-engines invariant.
  3. NO-LEAK (hard). stderr must not spill host paths, Rust source locations, Polars query
     plans or desugaring-internal variable names at the user.
  4. HAS-HELP (RATCHET). exit 1 should carry a `help:` line, and that help must not be the
     canned "each statement goes on its own line" when the source contains no `;` at all.

Oracle 4 fails a LOT today. A harness that is red on arrival is useless as a gate, so it is
a RATCHET, not a ban — exactly the argument tests/cli.rs makes for its ~90 `.unwrap()`s.
Those calls are not sloppiness and denying them outright would buy no safety; what matters
is that the number cannot silently GROW. Same here: today's bad help lines are a backlog,
not a regression, and the useful property is that a change cannot ADD one. The count lives
in scripts/stranger-baseline.json; `--bless` rewrites it. Like the unwrap budget, it also
fails when the count DROPS — a ratchet only ratchets if it tightens when the code improves.

DELIBERATELY NOT GENERATED: `http_get`/`http_post`/`http_request`/`listen` (network),
`random`/`randn`/`random_int`/`clock_monotonic`/`sleep` (nondeterministic by contract), and
`read_int` (blocks on stdin). Oracle 2 would flag them and be right, which is why they are
out — a determinism oracle is only meaningful over programs that are supposed to be
deterministic. stdin is /dev/null for every run regardless, for the same reason
`scripts/gate.sh` needs `< /dev/null`.

Exit status: non-zero if any hard oracle fails or if the oracle-4 count moved.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(REPO, "scripts", "stranger-baseline.json")

ENGINES = [("jit", {}), ("vm", {"HELIX_NOJIT": "1"}), ("tw", {"HELIX_NOVM": "1"})]
RUNS = 3
TIMEOUT = 25

ABORT_RE = re.compile(r"panicked at|internal error|Aborted")

# Things a user must never see. Host paths and .rs:LINE are the toolchain leaking through;
# the Polars fragments are a query engine leaking through; the `$`-prefixed names are
# Helix's own desugaring leaking through (a user never wrote `$acc0`).
LEAKS = [
    r"/\.cargo/",
    r"\.rs:\d+",
    r"Csv SCAN",
    r"ESTIMATED ROWS",
    r"Resolved plan until failure",
    r"Schema \{",
    r"truncate_ragged_lines",
    r"infer_schema_length",
    r"schema_overrides",
    r"\$obe",
    r"\$oba",
    r"m0\$",
    r"\$acc0",
    r"\$aff",
    r"\$arg_extreme",
    r"<main>",
]
LEAK_RE = [(p, re.compile(p)) for p in LEAKS]

CANNED = "each statement goes on its own line"


# ---------------------------------------------------------------------------
# The generator. Every table below is a fixed literal and every loop is over a
# sorted or literal order, so the emitted corpus is byte-for-byte the same on
# every run and on every machine. There is no PRNG anywhere in this file.
# ---------------------------------------------------------------------------

# (language, axis, tag, kind, source). kind is "e" for an expression fragment that gets
# dropped into five different syntactic positions, or "s" for a statement/whole-form that
# gets three.
HABITS = [
    # ---------------- Python ----------------
    ("py", "stmt", "for-colon", "s", "for x in xs:\n    print(x)\n"),
    ("py", "stmt", "while-colon", "s", "i = 0\nwhile i < 10:\n    i += 1\n"),
    ("py", "stmt", "def", "s", "def f(x):\n    return x * 2\n\nprint(f(3))\n"),
    ("py", "stmt", "elif", "s", "x = 1\nif x == 1:\n    print(1)\nelif x == 2:\n    print(2)\nelse:\n    print(3)\n"),
    ("py", "stmt", "class", "s", "class Point:\n    pass\n"),
    ("py", "stmt", "import", "s", "import numpy as np\nprint(np.mean([1, 2]))\n"),
    ("py", "stmt", "try-except", "s", "try:\n    x = 1\nexcept Exception as e:\n    print(e)\n"),
    ("py", "stmt", "with", "s", 'with open("f.txt") as fh:\n    print(fh)\n'),
    ("py", "stmt", "return-top", "s", "return 1\n"),
    ("py", "stmt", "augassign", "s", "x = 1\nx += 2\nprint(x)\n"),
    ("py", "stmt", "global", "s", "x = 1\n\ndef f():\n    global x\n    x = 2\n"),
    ("py", "null", "None", "e", "None"),
    ("py", "bool", "True", "e", "True"),
    ("py", "bool", "False", "e", "False"),
    ("py", "len", "len", "e", "len([1, 2, 3])"),
    ("py", "builtin", "sorted", "e", "sorted([3, 1, 2])"),
    ("py", "builtin", "sum-list", "e", "sum([1, 2, 3])"),
    ("py", "builtin", "str", "e", "str(3)"),
    ("py", "builtin", "int", "e", 'int("3")'),
    ("py", "builtin", "abs", "e", "abs(-1)"),
    ("py", "builtin", "upper", "e", '"abc".upper()'),
    ("py", "builtin", "range-py", "e", "list(range(4))"),
    ("py", "index", "neg", "e", "[1, 2, 3][-1]"),
    ("py", "index", "slice", "e", "[1, 2, 3][1:3]"),
    ("py", "index", "slice-step", "e", "[1, 2, 3][::2]"),
    ("py", "op", "in", "e", "2 in [1, 2, 3]"),
    ("py", "op", "chained-cmp", "e", "1 < 2 < 3"),
    ("py", "op", "ternary", "e", "1 if 2 > 1 else 0"),
    ("py", "op", "pow", "e", "2 ** 10"),
    ("py", "op", "not", "e", "not True"),
    ("py", "op", "is-none", "e", "x is None"),
    ("py", "op", "percent-fmt", "e", '"%d" % 3'),
    ("py", "str", "fstring", "e", 'f"v={1}"'),
    ("py", "str", "triple", "e", '"""three"""'),
    ("py", "str", "dict-literal", "e", '{"a": 1}'),
    ("py", "str", "comprehension", "e", "[i * i for i in range(4)]"),
    ("py", "lambda", "lambda", "e", "lambda x: x + 1"),
    # ---------------- JavaScript ----------------
    ("js", "stmt", "c-for", "s", "for (let i = 0; i < 3; i++) {\n  console.log(i);\n}\n"),
    ("js", "stmt", "function", "s", "function f(x) {\n  return x * 2;\n}\nconsole.log(f(3));\n"),
    ("js", "stmt", "const", "s", "const xs = [1, 2, 3];\nconsole.log(xs.length);\n"),
    ("js", "stmt", "if-braces", "s", 'let x = 1;\nif (x === 1) {\n  console.log("one");\n} else {\n  console.log("other");\n}\n'),
    ("js", "stmt", "switch", "s", "switch (x) {\n  case 1:\n    console.log(1);\n    break;\n  default:\n    break;\n}\n"),
    ("js", "stmt", "forEach", "s", "xs.forEach((v) => {\n  console.log(v);\n});\n"),
    ("js", "stmt", "require", "s", 'const fs = require("fs");\n'),
    ("js", "stmt", "class", "s", "class P {\n  constructor(x) {\n    this.x = x;\n  }\n}\n"),
    ("js", "cmt", "line", "s", "// a comment\nprint(1)\n"),
    ("js", "cmt", "block", "s", "/* block\n   comment */\nprint(1)\n"),
    ("js", "cmt", "jsdoc", "s", "/** doc */\nprint(1)\n"),
    ("js", "null", "null", "e", "null"),
    ("js", "null", "undefined", "e", "undefined"),
    ("js", "op", "andand", "e", "true && false"),
    ("js", "op", "oror", "e", "true || false"),
    ("js", "op", "strict-eq", "e", "1 === 1"),
    ("js", "op", "strict-ne", "e", "1 !== 2"),
    ("js", "op", "typeof", "e", "typeof 1"),
    ("js", "op", "nullish", "e", "1 ?? 0"),
    ("js", "op", "optchain", "e", "x?.y"),
    ("js", "op", "spread", "e", "[...[1, 2]]"),
    ("js", "op", "not", "e", "!true"),
    ("js", "len", "dot-length", "e", "[1, 2, 3].length"),
    ("js", "builtin", "console-log", "e", "console.log(1)"),
    ("js", "builtin", "toUpperCase", "e", '"abc".toUpperCase()'),
    ("js", "builtin", "sort", "e", "[3, 1, 2].sort()"),
    ("js", "builtin", "parseInt", "e", 'parseInt("3")'),
    ("js", "builtin", "Math-abs", "e", "Math.abs(-1)"),
    ("js", "builtin", "JSON", "e", "JSON.stringify([1])"),
    ("js", "builtin", "push", "e", "[1, 2].push(3)"),
    ("js", "str", "backtick", "e", "`hi ${1}`"),
    ("js", "str", "css-brace", "e", "{ color: red; }"),
    ("js", "lambda", "anon-fn", "e", "function (v) { return v; }"),
    # ---------------- R ----------------
    ("r", "stmt", "assign-arrow", "s", "x <- c(1, 2, 3)\nprint(x)\n"),
    ("r", "stmt", "function", "s", "f <- function(x) {\n  x * 2\n}\nprint(f(3))\n"),
    ("r", "stmt", "for", "s", "for (i in 1:3) {\n  cat(i)\n}\n"),
    ("r", "stmt", "if", "s", 'if (x > 1) {\n  print("big")\n}\n'),
    ("r", "stmt", "library", "s", "library(dplyr)\n"),
    ("r", "null", "NULL", "e", "NULL"),
    ("r", "null", "NA", "e", "NA"),
    ("r", "null", "NaN", "e", "NaN"),
    ("r", "bool", "TRUE", "e", "TRUE"),
    ("r", "bool", "FALSE", "e", "FALSE"),
    ("r", "len", "length", "e", "length(c(1, 2))"),
    ("r", "len", "nchar", "e", 'nchar("abc")'),
    ("r", "builtin", "c", "e", "c(1, 2, 3)"),
    ("r", "builtin", "cat", "e", 'cat("hi\\n")'),
    ("r", "builtin", "seq", "e", "seq(1, 10)"),
    ("r", "builtin", "paste", "e", 'paste("a", "b")'),
    ("r", "builtin", "toupper", "e", 'toupper("abc")'),
    ("r", "index", "one-based", "e", "c(10, 20)[1]"),
    ("r", "index", "double-bracket", "e", "xs[[1]]"),
    ("r", "op", "in-pct", "e", "1 %in% c(1, 2)"),
    ("r", "op", "colon-range", "e", "1:10"),
    ("r", "op", "assign-in-arg", "e", "sum(x <- 1)"),
    # ---------------- MATLAB ----------------
    ("m", "stmt", "for-end", "s", "for i = 1:3\n  disp(i)\nend\n"),
    ("m", "stmt", "function", "s", "function y = f(x)\n  y = x * 2;\nend\n"),
    ("m", "stmt", "if-end", "s", "if x == 1\n  disp('one')\nend\n"),
    ("m", "stmt", "while-end", "s", "while i < 3\n  i = i + 1;\nend\n"),
    ("m", "cmt", "percent", "s", "% a comment\ndisp(1)\n"),
    ("m", "cmt", "block-pct", "s", "%{\nblock\n%}\ndisp(1)\n"),
    ("m", "stmt", "matrix", "s", "x = [1 2 3];\ndisp(x)\n"),
    ("m", "len", "size", "e", "size([1, 2])"),
    ("m", "len", "numel", "e", "numel([1, 2])"),
    ("m", "builtin", "disp", "e", "disp(1)"),
    ("m", "builtin", "fprintf", "e", "fprintf('%d\\n', 3)"),
    ("m", "builtin", "zeros-paren", "e", "zeros(2, 2)"),
    ("m", "index", "paren-index", "e", "x(1)"),
    ("m", "op", "transpose", "e", "x'"),
    ("m", "op", "ne-tilde", "e", "1 ~= 2"),
    ("m", "op", "not-tilde", "e", "~true"),
    ("m", "op", "elemwise", "e", "[1, 2] .* [3, 4]"),
    ("m", "op", "step-range", "e", "1:0.5:3"),
    ("m", "str", "single-quote", "e", "'single quoted'"),
    ("m", "str", "end-keyword", "e", "end"),
    # ---------------- Julia ----------------
    ("jl", "stmt", "for-end", "s", "for i in 1:3\n  println(i)\nend\n"),
    ("jl", "stmt", "function", "s", "function f(x)\n  return x * 2\nend\nprintln(f(3))\n"),
    ("jl", "stmt", "elseif", "s", "if x == 1\n  println(1)\nelseif x == 2\n  println(2)\nend\n"),
    ("jl", "cmt", "block", "s", "#= block =#\nprintln(1)\n"),
    ("jl", "stmt", "using", "s", "using DataFrames\n"),
    ("jl", "null", "nothing", "e", "nothing"),
    ("jl", "builtin", "println", "e", 'println("hi")'),
    ("jl", "builtin", "Int-ctor", "e", "Int(3.0)"),
    ("jl", "builtin", "string", "e", "string(3)"),
    ("jl", "op", "broadcast", "e", "[1, 2] .+ 1"),
    ("jl", "op", "pipe", "e", "[1, 2] |> sum"),
    ("jl", "op", "caret-pow", "e", "2 ^ 10"),
    ("jl", "index", "end-index", "e", "[1, 2, 3][end]"),
    ("jl", "str", "comprehension", "e", "[i^2 for i in 1:3]"),
    ("jl", "str", "interp-dollar", "e", '"v=$x"'),
    # ---------------- Go ----------------
    ("go", "stmt", "whole-file", "s", 'package main\n\nimport "fmt"\n\nfunc main() {\n\tfmt.Println("hi")\n}\n'),
    ("go", "stmt", "c-for", "s", "for i := 0; i < 3; i++ {\n\tfmt.Println(i)\n}\n"),
    ("go", "stmt", "short-decl", "s", "x := 1\nif x == 1 {\n\tfmt.Println(1)\n}\n"),
    ("go", "stmt", "switch", "s", "switch x {\ncase 1:\n\tfmt.Println(1)\n}\n"),
    ("go", "stmt", "struct", "s", "type P struct {\n\tX int\n}\n"),
    ("go", "null", "nil", "e", "nil"),
    ("go", "len", "len", "e", "len([1, 2])"),
    ("go", "builtin", "fmt-Println", "e", "fmt.Println(1)"),
    ("go", "builtin", "make", "e", "make([]int, 3)"),
    ("go", "builtin", "append", "e", "append(xs, 1)"),
    ("go", "op", "short-decl-expr", "e", "x := 1"),
    ("go", "op", "var-decl", "e", "var x int = 1"),
    ("go", "lambda", "func-lit", "e", "func(x int) int { return x }"),
    # ---------------- C ----------------
    ("c", "stmt", "whole-file", "s", '#include <stdio.h>\n\nint main(void) {\n    printf("hi\\n");\n    return 0;\n}\n'),
    ("c", "stmt", "c-for", "s", 'for (int i = 0; i < 3; i++) {\n    printf("%d\\n", i);\n}\n'),
    ("c", "stmt", "decl-if", "s", 'int x = 1;\nif (x == 1) {\n    printf("one");\n}\n'),
    ("c", "stmt", "while", "s", "while (i < 3) {\n    i++;\n}\n"),
    ("c", "cmt", "block", "s", '/* block comment */\nprintf("1");\n'),
    ("c", "stmt", "define", "s", "#define N 3\nprint(N)\n"),
    ("c", "null", "NULL", "e", "NULL"),
    ("c", "builtin", "printf", "e", 'printf("%d\\n", 3)'),
    ("c", "builtin", "puts", "e", 'puts("hi")'),
    ("c", "builtin", "sizeof", "e", "sizeof(int)"),
    ("c", "op", "cast", "e", "(int) 3.5"),
    ("c", "op", "incr", "e", "x++"),
    ("c", "op", "ternary", "e", "1 ? 1 : 0"),
    ("c", "op", "addr-of", "e", "&x"),
    ("c", "op", "deref", "e", "*p"),
    ("c", "op", "bitand", "e", "3 & 1"),
    ("c", "op", "shift", "e", "8 >> 1"),
    ("c", "op", "comma", "e", "(1, 2)"),
    # ---------------- SQL / shell / data formats ----------------
    ("sql", "cmt", "dash-dash", "s", "-- a comment\nprint(1)\n"),
    ("sql", "stmt", "select", "s", "SELECT * FROM xs WHERE a > 1;\n"),
    ("sh", "cmt", "shebang", "s", "#!/usr/bin/env helix\nprint(1)\n"),
    ("sh", "stmt", "export", "s", "export X=1\necho $X\n"),
    ("json", "str", "json-file", "s", '{\n  "a": 1,\n  "b": [2, 3]\n}\n'),
    ("re", "str", "regex-lit", "e", "/^a{2,3}$/"),
    ("re", "str", "regex-str", "e", '"\\\\d{2,3}"'),
    ("css", "str", "css-rule", "s", "body {\n  color: red;\n}\n"),
]

# Where an expression fragment gets dropped. Each puts the stranger's text in a different
# position of the grammar, so the same habit is judged by a different parser state and a
# different code generator.
EXPR_CTX = [
    ("top", "print(%s)\n"),
    ("assign", "v = %s\nprint(v)\n"),
    ("fn", "fn probe(x) = %s\nprint(probe(2))\n"),
    ("map", "print([1, 2, 3].map(it => %s))\n"),
    ("try", "r = try print(%s)\nprint(r.ok)\n"),
]

# Where a whole statement form gets dropped. "after" matters: it moves the first error off
# line 1, which is where span arithmetic goes wrong.
STMT_CTX = [
    ("bare", "%s"),
    ("after", 'x = 1\nprint(x)\n%s'),
    ("before", '%sprint("done")\n'),
]


def habit_programs():
    out = []
    for lang, axis, tag, kind, src in HABITS:
        ctxs = EXPR_CTX if kind == "e" else STMT_CTX
        for cname, tmpl in ctxs:
            body = tmpl % src
            if kind == "s" and not body.endswith("\n"):
                body += "\n"
            out.append(("habit/%s/%s/%s/%s" % (lang, axis, tag, cname), body.encode()))
    return out


def pair_programs():
    """Two strangers in one file. A newcomer does not make one mistake at a time, and a
    second bad line is how you learn whether recovery reports the FIRST error or drowns."""
    exprs = [(l, t, s) for (l, a, t, k, s) in HABITS if k == "e"]
    out = []
    n = len(exprs)
    # A fixed stride walk: deterministic, and every element is both a left and a right.
    for stride in (1, 7, 23):
        for i in range(n):
            la, ta, sa = exprs[i]
            lb, tb, sb = exprs[(i + stride) % n]
            if la == lb:
                continue
            body = "a = %s\nb = %s\nprint(a)\nprint(b)\n" % (sa, sb)
            out.append(("pair/%s-%s/%s-%s" % (la, ta, lb, tb), body.encode()))
    return out


# ---------------------------------------------------------------------------
# Byte-level axis. Nothing above can produce these: a generator that emits text
# from a grammar always emits valid UTF-8 with LF endings and a final newline.
# ---------------------------------------------------------------------------

BYTE_BASES = [
    ("ok-print", b'print(1)\n'),
    ("ok-fn", b'fn f(x) = x * 2\nprint(f(3))\n'),
    ("ok-indent", b'fn f(x) =\n  x * 2\nprint(f(3))\n'),
    ("ok-str", b'print("hello")\n'),
    ("bad-name", b'print(undefinedThing)\n'),
    ("bad-syntax", b'for x in xs:\n    print(x)\n'),
    ("bad-unclosed", b'print("unclosed\n'),
    ("bad-paren", b'print((1 + 2)\n'),
]


def byte_transforms(src):
    yield "bom", b"\xef\xbb\xbf" + src
    yield "crlf", src.replace(b"\n", b"\r\n")
    yield "cr-only", src.replace(b"\n", b"\r")
    yield "no-final-nl", src.rstrip(b"\n")
    yield "nul-mid", src.replace(b"(", b"(\x00", 1)
    yield "nul-end", src + b"\x00"
    yield "bad-utf8", src[:1] + b"\xff\xfe" + src[1:]
    yield "latin1", src.rstrip(b"\n") + b"  # caf\xe9\n"
    yield "tabs", b"\t" + src.replace(b"\n", b"\n\t").rstrip(b"\t")
    yield "vtab-ff", src.replace(b"\n", b"\x0b\x0c\n", 1)
    yield "nbsp", src.replace(b" ", b"\xc2\xa0")
    yield "bom-crlf-notrail", (b"\xef\xbb\xbf" + src.replace(b"\n", b"\r\n")).rstrip(b"\r\n")


def byte_programs():
    out = [("bytes/empty/empty", b"")]
    out.append(("bytes/only-nl/only-nl", b"\n"))
    out.append(("bytes/only-bom/only-bom", b"\xef\xbb\xbf"))
    out.append(("bytes/only-nul/only-nul", b"\x00"))
    out.append(("bytes/only-ws/only-ws", b" \t \t \n\n \t\n"))
    out.append(("bytes/long-line/valid-4k", b"print(" + b"1 + " * 1000 + b"1)\n"))
    out.append(("bytes/long-line/name-4k", b"print(" + b"a" * 4096 + b")\n"))
    out.append(("bytes/long-line/string-4k", b'print("' + b"x" * 4096 + b'")\n'))
    out.append(("bytes/long-line/nested-4k", b"print(" + b"(" * 1024 + b"1" + b")" * 1024 + b")\n"))
    out.append(("bytes/long-line/unterminated-4k", b'print("' + b"x" * 4090 + b"\n"))
    for bname, src in BYTE_BASES:
        for tname, blob in byte_transforms(src):
            out.append(("bytes/%s/%s" % (bname, tname), blob))
    return out


# ---------------------------------------------------------------------------
# Malformed-CSV / DataFrame-from-a-file axis. This is the one no in-process
# generator can reach: the value has to come off the disk, through polars,
# before the bug is even loadable. `read_csv(f).where(1)` SIGABRTed here.
# ---------------------------------------------------------------------------

CSVS = [
    ("ok", b"a,b\n1,2\n3,4\n"),
    ("empty", b""),
    ("header-only", b"a,b\n"),
    ("no-header", b"1,2\n3,4\n"),
    ("ragged-short", b"a,b,c\n1,2\n3,4,5\n"),
    ("ragged-long", b"a,b\n1,2,3\n4,5\n"),
    ("dup-cols", b"a,a\n1,2\n"),
    ("blank-col-name", b"a,,c\n1,2,3\n"),
    ("spacey-col", b"a b, c\t\n1,2\n"),
    ("unbalanced-quote", b'a,b\n"1,2\n3,4\n'),
    ("newline-in-quote", b'a,b\n"line\nbreak",2\n'),
    ("mixed-types", b"a,b\n1,x\ny,2\n"),
    ("bigint-overflow", b"a\n99999999999999999999999999\n"),
    ("bom", b"\xef\xbb\xbfa,b\n1,2\n"),
    ("crlf", b"a,b\r\n1,2\r\n"),
    ("nul", b"a,b\n1,\x002\n"),
    ("bad-utf8", b"a,b\n1,\xff\xfe\n"),
    ("semicolons", b"a;b\n1;2\n"),
    ("tsv-in-csv", b"a\tb\n1\t2\n"),
    ("all-empty", b"a,b\n,\n,\n"),
    ("one-cell", b"x\n"),
    ("nan-inf", b"a\nNaN\ninf\n-inf\n"),
]

# What a newcomer does to a frame the moment they have one. `.where(1)` is here on
# purpose — it is the exact shape that core-dumped in c243040.
CSV_USES = [
    ("count", "print(d.count())"),
    ("columns", "print(d.columns())"),
    ("where-int", "print(d.where(1))"),
    ("where-str", 'print(d.where("a"))'),
    ("select-missing", 'print(d.select("nope"))'),
    ("column-a", 'print(d.column("a"))'),
    ("sort-a", 'print(d.sort("a"))'),
    ("group-a", 'print(d.group("a"))'),
    ("head-neg", "print(d.head(-1))"),
    ("filter-truthy", "print(d.filter(1))"),
    ("to-table", "print(d.to_table())"),
    ("to-markdown", "print(d.to_markdown())"),
    ("join-self", 'print(d.join(d, "a"))'),
    ("unique", "print(d.unique())"),
    ("with-expr", 'print(d.with("z", 1))'),
    ("index", "print(d[0])"),
]


def csv_programs(datadir):
    out = []
    for cname, blob in CSVS:
        path = os.path.join(datadir, "%s.csv" % cname)
        with open(path, "wb") as fh:
            fh.write(blob)
        for uname, use in CSV_USES:
            body = 'd = read_csv("%s")\n%s\n' % (path, use)
            out.append(("csv/%s/%s" % (cname, uname), body.encode()))
    # A frame is not the only thing a file becomes.
    for cname, _ in CSVS:
        path = os.path.join(datadir, "%s.csv" % cname)
        out.append(("file/%s/read_text" % cname, ('print(read_text("%s"))\n' % path).encode()))
        out.append(("file/%s/read_json" % cname, ('print(read_json("%s"))\n' % path).encode()))
        out.append(("file/%s/read_parquet" % cname, ('print(read_parquet("%s"))\n' % path).encode()))
        out.append(("file/%s/read_fasta" % cname, ('print(read_fasta("%s"))\n' % path).encode()))
    # Paths that are not files at all.
    for pname, p in [
        ("missing", os.path.join(datadir, "does-not-exist.csv")),
        ("dir", datadir),
        ("empty-str", ""),
        ("dev-null", "/dev/null"),
    ]:
        out.append(("path/%s/read_csv" % pname, ('print(read_csv("%s"))\n' % p).encode()))
        out.append(("path/%s/read_text" % pname, ('print(read_text("%s"))\n' % p).encode()))
    return out


def generate(datadir):
    progs = []
    progs += habit_programs()
    progs += pair_programs()
    progs += byte_programs()
    progs += csv_programs(datadir)
    seen, uniq = set(), []
    for name, blob in progs:
        if name in seen:
            continue
        seen.add(name)
        uniq.append((name, blob))
    uniq.sort(key=lambda t: t[0])
    return uniq


# ---------------------------------------------------------------------------
# Execution
# ---------------------------------------------------------------------------

def base_env():
    env = {k: v for k, v in os.environ.items() if not k.startswith("HELIX_")}
    env["LC_ALL"] = "C"
    env["NO_COLOR"] = "1"
    env["RUST_BACKTRACE"] = "0"
    return env


def run_one(binary, path, cwd, extra):
    env = base_env()
    env.update(extra)
    try:
        p = subprocess.run(
            [binary, "run", path],
            capture_output=True,
            env=env,
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            timeout=TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        return ("TIMEOUT", b"", b"")
    return (p.returncode, p.stdout, p.stderr)


def judge(name, src, results):
    """results: {(engine, run_index): (rc, out, err)}. Returns a list of findings."""
    found = []
    # --- oracle 1: never abort
    for (eng, i), (rc, out, err) in sorted(results.items()):
        blob = out + err
        if rc == "TIMEOUT":
            found.append((1, name, eng, "TIMEOUT after %ds" % TIMEOUT, rc, out, err))
            break
        if rc not in (0, 1):
            found.append((1, name, eng, "exit %s (not 0 or 1)" % rc, rc, out, err))
            break
        m = ABORT_RE.search(blob.decode("utf-8", "replace"))
        if m:
            found.append((1, name, eng, "stderr matched %r" % m.group(0), rc, out, err))
            break
    # --- oracle 2: determinism (9 identical triples)
    triples = {}
    for (eng, i), r in results.items():
        triples.setdefault(r, []).append("%s#%d" % (eng, i))
    if len(triples) != 1:
        # Distinguish a single engine disagreeing with ITSELF (nondeterminism) from two
        # engines disagreeing with EACH OTHER (a parity break). Both are hard failures;
        # they have completely different causes.
        per_engine = {}
        for (eng, i), r in results.items():
            per_engine.setdefault(eng, set()).add(r)
        flaky = sorted(e for e, s in per_engine.items() if len(s) > 1)
        kind = "NONDETERMINISTIC within %s" % ",".join(flaky) if flaky else "CROSS-ENGINE divergence"
        variants = sorted(((sorted(tags), r) for r, tags in triples.items()), key=lambda t: t[0])
        found.append((2, name, kind, variants, None, None, None))
    # --- oracle 3: no leak. Judge the reference run.
    ref = results.get(("jit", 0))
    if ref and ref[0] != "TIMEOUT":
        err_s = ref[2].decode("utf-8", "replace")
        for pat, rx in LEAK_RE:
            m = rx.search(err_s)
            if m:
                found.append((3, name, "jit", "leaked %s -> %r" % (pat, m.group(0)),
                              ref[0], ref[1], ref[2]))
                break
    # --- oracle 4: help quality (ratchet)
    help_bad = None
    if ref and ref[0] == 1:
        err_s = ref[2].decode("utf-8", "replace")
        if "help:" not in err_s:
            help_bad = "exit 1 with no `help:` line"
        elif CANNED in err_s and b";" not in src:
            help_bad = "canned `no ;` help but the source has no `;`"
    return found, help_bad


def selftest(binary, sandbox):
    """An oracle that never fires is indistinguishable from an oracle that is broken.

    The full corpus currently reports 0 for oracles 1 and 2, which is only good news if
    those two can still say "no". This feeds `judge()` results that MUST fail and one real
    nondeterministic program (`clock_monotonic()`, which is deliberately absent from the
    corpus for exactly this reason), and checks that each oracle notices.
    """
    ok = True

    def check(label, cond):
        nonlocal ok
        print("  %-46s %s" % (label, "ok" if cond else "BROKEN"))
        ok = ok and cond

    clean = {(e, i): (0, b"", b"") for e, _ in ENGINES for i in range(RUNS)}

    aborted = dict(clean)
    aborted[("vm", 1)] = (134, b"", b"thread 'main' panicked at src/vm.rs:99\nAborted\n")
    f, _ = judge("x", b"print(1)\n", aborted)
    check("oracle 1 sees exit 134", any(x[0] == 1 for x in f))

    panicked = dict(clean)
    panicked[("tw", 0)] = (1, b"", b"thread 'main' panicked at foo\n")
    f, _ = judge("x", b"print(1)\n", panicked)
    check("oracle 1 sees `panicked at` on exit 1", any(x[0] == 1 for x in f))

    split = dict(clean)
    split[("jit", 2)] = (0, b"different\n", b"")
    f, _ = judge("x", b"print(1)\n", split)
    check("oracle 2 sees a within-engine split", any(x[0] == 2 for x in f))

    for pat in LEAKS:
        probe = {k: (1, b"", b"error: e\n" + PROBE_FOR.get(pat, pat).encode() + b"\n") for k in clean}
        f, _ = judge("x", b"print(1)\n", probe)
        if not any(x[0] == 3 for x in f):
            check("oracle 3 sees %s" % pat, False)
    check("oracle 3 sees all %d deny-list patterns" % len(LEAKS), ok)

    canned = {k: (1, b"", b"error: e\nhelp: " + CANNED.encode() + b"; no `;`.\n") for k in clean}
    _, hb = judge("x", b"for x in xs:\n", canned)
    check("oracle 4 sees canned help with no `;`", hb is not None)
    _, hb = judge("x", b"a; b\n", canned)
    check("oracle 4 forgives canned help when `;` present", hb is None)
    _, hb = judge("x", b"print(1)\n", {k: (1, b"", b"error: e\n") for k in clean})
    check("oracle 4 sees a missing help: line", hb is not None)

    # One real subprocess: a program that is nondeterministic by contract.
    p = os.path.join(sandbox, "selftest-clock.helix")
    with open(p, "wb") as fh:
        fh.write(b"print(clock_monotonic())\n")
    res = {(e, i): run_one(binary, p, sandbox, extra)
           for e, extra in ENGINES for i in range(RUNS)}
    f, _ = judge("clock", b"print(clock_monotonic())\n", res)
    check("oracle 2 sees real clock_monotonic() drift", any(x[0] == 2 for x in f))
    return 0 if ok else 1


# Literal strings that each deny-list regex must match, for the selftest.
PROBE_FOR = {
    r"/\.cargo/": "/home/u/.cargo/registry/x",
    r"\.rs:\d+": "src/vm.rs:99",
    r"Schema \{": "Schema {a: str}",
    r"m0\$": "m0$x",
}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bin", default=os.path.join(REPO, "target", "gate", "helix"))
    ap.add_argument("--jobs", type=int, default=min(8, (os.cpu_count() or 4)))
    ap.add_argument("--sandbox", default="/tmp/helix-stranger")
    ap.add_argument("--filter", default=None, help="only run programs whose name contains this")
    ap.add_argument("--list", action="store_true", help="print the corpus and exit")
    ap.add_argument("--bless", action="store_true", help="rewrite the oracle-4 baseline")
    ap.add_argument("--selftest", action="store_true",
                    help="prove each oracle can still fail, then exit")
    ap.add_argument("--max-report", type=int, default=25)
    args = ap.parse_args()

    binary = os.path.abspath(args.bin)
    if not args.list and not os.path.exists(binary):
        sys.exit("no binary at %s — cargo build --profile gate --bin helix" % binary)
    # Identify the binary. Oracle 4's count is a property of a specific build, and a build
    # that changes UNDER a run silently rewrites the answer — which is exactly what happened
    # the first time this harness was run, against a tree a second session was rebuilding.
    bin_sha = ""
    if not args.list:
        h = hashlib.sha256()
        with open(binary, "rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                h.update(chunk)
        bin_sha = h.hexdigest()

    sandbox = os.path.abspath(args.sandbox)
    shutil.rmtree(sandbox, ignore_errors=True)
    srcdir = os.path.join(sandbox, "src")
    datadir = os.path.join(sandbox, "data")
    os.makedirs(srcdir)
    os.makedirs(datadir)

    if args.selftest:
        print("stranger --selftest: can each oracle still say no?")
        return selftest(binary, sandbox)

    progs = generate(datadir)
    if args.filter:
        progs = [p for p in progs if args.filter in p[0]]
    # ZERO PROGRAMS IS A FAILURE, NOT A PASS. Every oracle vacuously reports "0 failing"
    # over an empty corpus, so a typo'd --filter — or a generator that silently produced
    # nothing — would print PASS and exit 0. That is the shape of a gate that stops gating
    # without anyone noticing, which is the same argument the unwrap budget makes.
    if not progs:
        sys.exit(
            "no programs to run%s — refusing to report PASS over an empty corpus"
            % (" (--filter %r matched nothing)" % args.filter if args.filter else "")
        )
    if args.list:
        for name, blob in progs:
            print("%-56s %d bytes" % (name, len(blob)))
        print("\n%d programs" % len(progs))
        return 0

    # Materialize. One file per program, name-derived so a failure is reproducible by
    # hand: the path printed in the report IS the file that failed.
    paths = {}
    for name, blob in progs:
        fname = name.replace("/", "__") + ".helix"
        p = os.path.join(srcdir, fname)
        with open(p, "wb") as fh:
            fh.write(blob)
        paths[name] = p

    total_runs = len(progs) * len(ENGINES) * RUNS
    print("stranger: %d programs x %d engines x %d runs = %d executions, %d jobs"
          % (len(progs), len(ENGINES), RUNS, total_runs, args.jobs))
    print("sandbox:  %s" % sandbox)
    print("binary:   %s  sha256:%s" % (binary, bin_sha[:16]))
    sys.stdout.flush()

    import time
    t0 = time.time()

    def work(item):
        name, blob = item
        res = {}
        for eng, extra in ENGINES:
            for i in range(RUNS):
                res[(eng, i)] = run_one(binary, paths[name], sandbox, extra)
        return name, blob, res

    findings = []
    help_failures = []
    done = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as ex:
        for name, blob, res in ex.map(work, progs):
            f, hb = judge(name, blob, res)
            findings.extend(f)
            if hb:
                help_failures.append((name, hb))
            done += 1
            if done % 100 == 0:
                print("  ... %d/%d (%.0fs)" % (done, len(progs), time.time() - t0))
                sys.stdout.flush()
    elapsed = time.time() - t0

    findings.sort(key=lambda f: (f[0], f[1]))
    help_failures.sort()

    def show(blob, limit=600):
        s = blob.decode("utf-8", "replace") if isinstance(blob, bytes) else str(blob)
        return s if len(s) <= limit else s[:limit] + "\n    ...[truncated]"

    hard = [f for f in findings if f[0] in (1, 2, 3)]
    by_oracle = {1: [], 2: [], 3: []}
    for f in hard:
        by_oracle[f[0]].append(f)

    print("\n%s\nRESULTS  (%.1fs wall, %d executions)\n%s"
          % ("=" * 78, elapsed, total_runs, "=" * 78))
    print("oracle 1 NEVER-ABORT   : %d failing programs" % len(by_oracle[1]))
    print("oracle 2 DETERMINISM   : %d failing programs" % len(by_oracle[2]))
    print("oracle 3 NO-LEAK       : %d failing programs" % len(by_oracle[3]))
    print("oracle 4 HAS-HELP      : %d failing programs (ratchet)" % len(help_failures))
    if help_failures:
        why = {}
        for _, h in help_failures:
            why[h] = why.get(h, 0) + 1
        for h, n in sorted(why.items(), key=lambda t: -t[1]):
            print("           %4d  %s" % (n, h))

    for on, title in ((1, "NEVER-ABORT"), (3, "NO-LEAK")):
        for (_, name, eng, why, rc, out, err) in by_oracle[on][: args.max_report]:
            print("\n--- oracle %d %s: %s [%s]\n    %s" % (on, title, name, eng, why))
            print("    file: %s" % paths[name])
            print("    --- source ---\n%s" % show(open(paths[name], "rb").read()))
            print("    --- exit %s ---" % rc)
            print("    --- stdout ---\n%s" % show(out))
            print("    --- stderr ---\n%s" % show(err))
        if len(by_oracle[on]) > args.max_report:
            print("\n  (+%d more oracle-%d failures)" % (len(by_oracle[on]) - args.max_report, on))

    for (_, name, kind, variants, _a, _b, _c) in by_oracle[2][: args.max_report]:
        print("\n--- oracle 2 DETERMINISM: %s\n    %s" % (name, kind))
        print("    file: %s" % paths[name])
        print("    --- source ---\n%s" % show(open(paths[name], "rb").read()))
        for tags, (rc, out, err) in variants:
            print("    [%s] exit=%s" % (",".join(tags), rc))
            print("      stdout: %s" % show(out, 300).replace("\n", "\n              "))
            print("      stderr: %s" % show(err, 300).replace("\n", "\n              "))
    if len(by_oracle[2]) > args.max_report:
        print("\n  (+%d more oracle-2 failures)" % (len(by_oracle[2]) - args.max_report))

    # --- the oracle-4 ratchet
    names = sorted(n for n, _ in help_failures)
    if args.filter:
        # A subset cannot be compared against a whole-corpus count, and blessing from one
        # would silently erase every failure the filter excluded.
        print("\n--filter set: oracle-4 ratchet not gated (%d help failures in this subset)."
              % len(names))
        ratchet_bad = False
        if args.bless:
            sys.exit("refusing to --bless a filtered run")
    elif args.bless:
        with open(BASELINE, "w") as fh:
            json.dump({
                "_comment": "Oracle-4 (help-quality) ratchet for scripts/stranger.py. "
                            "See the module docstring: this is a backlog, not a target. "
                            "It may only go DOWN. Regenerate with --bless.",
                "help_failures": len(names),
                "corpus_size": len(progs),
                # Informational, never gated: the count must survive an ordinary rebuild.
                # It is here so a surprising delta can be checked against "did the binary
                # move?" before anyone goes looking for a bug in the compiler.
                "blessed_against_sha256": bin_sha,
                "programs": names,
            }, fh, indent=1)
            fh.write("\n")
        print("\nblessed baseline: %d help failures over %d programs -> %s"
              % (len(names), len(progs), BASELINE))
        ratchet_bad = False
    else:
        try:
            with open(BASELINE) as fh:
                base = json.load(fh)
        except (IOError, ValueError):
            print("\nNO BASELINE at %s — run with --bless once to create it." % BASELINE)
            return 1
        was = base.get("blessed_against_sha256")
        if was and was != bin_sha:
            print("\nnote: baseline was blessed against sha256:%s, this is sha256:%s — "
                  "a different build. The count is still expected to hold." % (was[:16], bin_sha[:16]))
        want = base["help_failures"]
        got = len(names)
        ratchet_bad = got != want
        if got > want:
            new = [n for n in names if n not in set(base.get("programs", []))]
            print("\nORACLE 4 RATCHET BROKEN: %d help failures, baseline %d (+%d)"
                  % (got, want, got - want))
            for n in new[:40]:
                why = dict(help_failures)[n]
                print("   NEW  %-56s %s" % (n, why))
        elif got < want:
            print("\nORACLE 4 improved: %d help failures, baseline %d (-%d) — "
                  "run --bless to lock the win in. A ratchet only ratchets if it tightens."
                  % (got, want, want - got))
        else:
            print("\noracle 4 ratchet: %d == baseline %d, held." % (got, want))

    ok = not hard and not ratchet_bad
    print("\n%s" % ("PASS" if ok else "FAIL"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

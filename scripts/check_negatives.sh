#!/usr/bin/env bash
# Smoke-test that the type checker catches errors BEFORE running.
cd "$(dirname "$0")/.."
BIN=./target/debug/helix
chk() {
  printf '%s\n' "$2" > /tmp/hxneg.helix
  echo "### $1"
  "$BIN" /tmp/hxneg.helix 2>&1 | head -5
  echo
}
chk "string + int"          'print(5 + "x")'
chk "if non-bool"           'print(if 5 then 1 else 2)'
chk "unknown function"      'print(velociti(3))'
chk "unknown array method"  'print([1, 2].maen())'
chk "no truthiness"         'print(5 and true)'
chk "wrong arity (range)"   'print(range(1, 2, 3))'
chk "return mismatch"       'fn f(x: Int) -> String = x + 1'
chk "unknown type annot"    'fn g(x: Intt) = x'
chk "undefined variable"    'print(scoer)'
chk "index by string"       'xs = [1, 2, 3]
print(xs["a"])'

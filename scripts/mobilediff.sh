#!/usr/bin/env bash
# The phone build (`--no-default-features --features mobile`) must run std.ui apps exactly as
# the default build does, and its capability gate (ADR 0021) must refuse exactly what the
# default build's refuses.
#
#   Usage: scripts/mobilediff.sh DEFAULT_BIN MOBILE_BIN
#
# The test suite cannot do this: `cargo test` builds ONE feature set, and `CARGO_BIN_EXE_helix`
# is that set's binary, so a test inside it can never see a second build. CI's `mobile` job
# builds both and runs this; locally, build the two into different places (the second build
# replaces target/gate/helix) and pass both paths.
#
# Each std.ui app's scripted session (`--replay`, one HTML screen per event) is run on every
# engine of both builds. The phone build has no JIT, so its "JIT" run is the VM's bytecode
# path; that it still matches is the HELIX_NOJIT contract, and the reason the JIT can be left
# out. stopwatch is the app rust-mobile ships; gallery's data_table is the reason `native-df`
# is in the profile. files.helix is not here: its session acts on a real directory, and
# tests/cli.rs already confines it.
cd "$(dirname "$0")/.."
DEF="${1:?usage: mobilediff.sh DEFAULT_BIN MOBILE_BIN}"
MOB="${2:?usage: mobilediff.sh DEFAULT_BIN MOBILE_BIN}"
fail=0
say() { printf '%s\n' "$*"; }

# THE TWO BINARIES MUST BE THE TWO BUILDS. Comparing a binary with itself passes every check
# below, so first make each say which build it is: a wrong path fails here, not silently.
feats='print([has_feature("mobile"), has_feature("default"), has_feature("jit"), has_feature("regex"), has_feature("native-df")])'
d=$("$DEF" eval "$feats" 2>&1); m=$("$MOB" eval "$feats" 2>&1)
[ "$d" = "[false, true, true, true, true]" ] || { say "FAIL $DEF is not a default build: $d"; fail=1; }
[ "$m" = "[true, false, false, true, true]" ] || { say "FAIL $MOB is not a mobile build: $m"; fail=1; }

for app in stopwatch gallery design-lab explorer orbit; do
  f="examples/apps/$app.helix"
  for eng in "" HELIX_NOJIT=1 HELIX_NOVM=1; do
    a=$(env $eng "$DEF" run "$f" --replay 2>&1); ca=$?
    b=$(env $eng "$MOB" run "$f" --replay 2>&1); cb=$?
    tag="$app ${eng:-default-engine}"
    if [ "$ca" -ne 0 ] || [ "$cb" -ne 0 ]; then
      say "FAIL $tag: exit default=$ca mobile=$cb"; say "${b:0:400}"; fail=1
    elif [ -z "$a" ]; then
      say "FAIL $tag: the replay printed nothing"; fail=1
    elif [ "$a" != "$b" ]; then
      say "DIFF $tag"; diff <(printf '%s\n' "$a") <(printf '%s\n' "$b") | head -5; fail=1
    else
      say "ok   $tag ($(printf '%s\n' "$a" | wc -l) screens)"
    fi
  done
done

# AND IT SERVES. Stopwatch's replay never parses a colour seed, so it runs even on the
# appliance; serving it does not: the served page's theme goes through std.ui's `#rrggbb`
# parser, a `re_match`, and the appliance exits 1 before printing a link. This is the path rust-mobile's shell runs: start the app,
# open the link it prints, get the page.
port=$((20000 + $$ % 20000))
log=$(mktemp)
"$MOB" run examples/apps/stopwatch.helix --port "$port" >"$log" 2>&1 &
pid=$!
for _ in $(seq 1 100); do grep -q 'http://' "$log" && break; sleep 0.1; done
url=$(grep -o 'http://[^ ]*' "$log" | head -1)
jar=$(mktemp)
page=$( [ -n "$url" ] && curl -sSfL --max-time 10 -c "$jar" -b "$jar" "$url" 2>&1 )
kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
if [[ "$page" == *"<h1>Stopwatch</h1>"* ]]; then
  say "ok   stopwatch served (${#page} bytes)"
else
  say "FAIL stopwatch served: link='$url' log: $(head -c 300 "$log") page: ${page:0:200}"; fail=1
fi
rm -f "$log" "$jar"

# THE GATE IS THE SAME IN EVERY BUILD. Under HELIX_CAP=enforce, network and process authority
# are refused unless granted; the phone build must refuse with the same words and status, and
# a grant must open it the same way (a transport error, not a denial: nothing listens on :9).
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
printf 'r = try http_get("http://127.0.0.1:9/nope")\nprint(r.ok)\nprint(r.error)\n' > "$tmp/net.helix"
printf 'print(run("echo", ["hi"]).status)\n' > "$tmp/proc.helix"
check() { # name, program, expected substring, env...
  local name=$1 prog=$2 want=$3; shift 3
  a=$(env "$@" "$DEF" "$prog" 2>&1); ca=$?
  b=$(env "$@" "$MOB" "$prog" 2>&1); cb=$?
  if [ "$ca" != "$cb" ] || [ "$a" != "$b" ]; then
    say "DIFF cap $name: default exit $ca / mobile exit $cb"; say "  default: ${a:0:200}"; say "  mobile:  ${b:0:200}"; fail=1
  elif [[ "$b" != *"$want"* ]]; then
    say "FAIL cap $name: expected \"$want\" in: ${b:0:200}"; fail=1
  else
    say "ok   cap $name (exit $cb)"
  fi
}
check "net denied"      "$tmp/net.helix"  "capability denied: \`http_get\` needs \`net\`" HELIX_CAP=enforce
check "process denied"  "$tmp/proc.helix" "capability denied: \`run\` needs \`process\`"  HELIX_CAP=enforce
check "net granted"     "$tmp/net.helix"  "failed"                                        HELIX_CAP=enforce HELIX_ALLOW_NET=on
[ "$(HELIX_CAP=enforce "$MOB" "$tmp/proc.helix" >/dev/null 2>&1; echo $?)" = 1 ] \
  || { say "FAIL cap process denied: the mobile build did not exit 1"; fail=1; }

say "RESULT=$fail"
exit "$fail"

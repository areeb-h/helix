#!/usr/bin/env bash
# A native app window (ADR 0055 Phase 4), driven on a virtual display.
#
#   bash tests/ui/window.sh [path/to/helix-built-with-desktop] [out-dir]
#
# Checks what only a real window can show, on the binary a `--features desktop` build makes:
#   1. `--window` with NO display falls back to the browser and the app still serves;
#   2. under Xvfb + a window manager, `--window` opens a window titled with the app, at its size;
#   3. the window's close button (WM_DELETE_WINDOW, as a person clicks it) ends the app and
#      frees its port;
#   4. a program that ends while its window is open exits with ITS exit code.
# A screenshot of the window lands in out-dir. Needs Xvfb, openbox, xdotool, wmctrl, curl,
# ImageMagick's `import` and dbus-run-session (gdb, if present, explains a hang). Exits non-zero
# on any failed check.
set -u
HELIX=${1:-target/desktop/gate/helix}
OUT=${2:-target/ui}
mkdir -p "$OUT"
fails=0
ok() { echo "  ok   $1"; }
bad() { echo "  FAIL $1"; fails=$((fails + 1)); }
work=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$work"' EXIT

# Each windowed run gets its own session bus, as a desktop login gives one. A CI runner's own
# user bus can carry services that never finish starting outside a real desktop session (the
# desktop portal waits for one), and GTK and WebKit then block on each call to them for D-Bus's
# 25 s timeout — a window that appears late and a page that never starts. That is the runner's
# environment, not the app's, so it is kept out of the test.
in_session() { dbus-run-session -- "$@"; }
# What the app's threads are doing, when it does not do what it should.
stacks() {
  local p
  p=$(pgrep -n -x helix) || return 0
  command -v gdb >/dev/null && sudo -n gdb -p "$p" -batch -ex 'thread apply all bt 12' 2>/dev/null | grep -E '^(Thread|#)' | head -80
}

echo "— no display: --window falls back and still serves"
env -u DISPLAY -u WAYLAND_DISPLAY "$HELIX" run examples/apps/gallery.helix --port 18960 --window >"$work/nodisp.log" 2>&1 &
pid=$!
for _ in $(seq 50); do grep -q 'token=' "$work/nodisp.log" && break; sleep 0.1; done
tok=$(grep -o 'token=[0-9a-f]*' "$work/nodisp.log" | head -1 | cut -d= -f2)
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Cookie: helix_app_18960=$tok" http://127.0.0.1:18960/)
[ "$code" = 200 ] && ok "serves with no display (200)" || bad "no-display run did not serve ($code)"
kill $pid 2>/dev/null; wait $pid 2>/dev/null

echo "— a window on a virtual display"
Xvfb :91 -screen 0 1280x860x24 >/dev/null 2>&1 &
sleep 1
export DISPLAY=:91
openbox >/dev/null 2>&1 &
sleep 1
start=$(date +%s)
in_session "$HELIX" run examples/apps/gallery.helix --port 18961 --window >"$work/win.log" 2>&1 &
pid=$!
wid=""
# Up to 30 s: a first start on a fresh machine can spend many seconds building caches.
for _ in $(seq 300); do wid=$(xdotool search --name '^Component gallery$' 2>/dev/null | head -1); [ -n "$wid" ] && break; sleep 0.1; done
if [ -n "$wid" ]; then
  ok "a window titled \"Component gallery\" opened ($(( $(date +%s) - start )) s)"
  sleep 3
  geo=$(xdotool getwindowgeometry "$wid" | grep Geometry | awk '{print $2}')
  [ "$geo" = "1100x760" ] && ok "at the default size ($geo)" || bad "window size is $geo, not 1100x760"
  import -window root "$OUT/desktop-window.png" 2>/dev/null && ok "screenshot: $OUT/desktop-window.png"
  # WebKit draws page content in separate processes; if they never started, the window maps
  # but stays blank and its event loop waits on them. Say which, so a failure names its cause.
  procs=$(ps -eo comm | grep -c '^WebKit' || true)
  [ "$procs" -ge 2 ] && ok "WebKit's page processes are running ($procs)" || { bad "WebKit's page processes did not start ($procs)"; stacks; }
  # The close button, as a window manager delivers it (WM_DELETE_WINDOW). The app may still be
  # busy loading its page on a slow machine, so it has up to 20 s to act on it.
  # By the window's own id, retried: on a slow machine the window manager can still be
  # registering the window when the close is first asked for (wmctrl then finds nothing to close).
  hex=$(printf '0x%08x' "$wid")
  sent=""
  for _ in $(seq 100); do wmctrl -i -c "$hex" 2>/dev/null && { sent=1; break; }; sleep 0.1; done
  [ -n "$sent" ] && ok "the close request was delivered" || bad "the window manager never accepted a close for $hex"
  for _ in $(seq 200); do kill -0 $pid 2>/dev/null || break; sleep 0.1; done
  if kill -0 $pid 2>/dev/null; then
    bad "the app outlived its window's close button"
    echo "windows still on the display:"; wmctrl -l; tail -5 "$work/win.log"; stacks
    kill $pid
  else
    ok "closing the window ends the app"
  fi
  wait $pid 2>/dev/null
  curl -s -m 1 -o /dev/null http://127.0.0.1:18961/ && bad "the port is still held" || ok "and frees its port"
else
  bad "no window appeared"; cat "$work/win.log"; echo "windows on the display:"; xwininfo -root -tree | head -30; ps aux | grep -E 'helix|WebKit' | grep -v grep
fi

echo "— a program that ends while its window is open"
printf 'w = open_window("http://127.0.0.1:18962/", "short", 400, 300)\nsleep(500)\nraise("boom")\n' >"$work/fail.helix"
timeout 60 dbus-run-session -- "$HELIX" run "$work/fail.helix" >/dev/null 2>"$work/fail.log"
status=$?
[ $status = 1 ] && grep -q 'boom' "$work/fail.log" && ok "exits with the program's code (1) and its error" || bad "exit $status: $(cat "$work/fail.log")"

[ $fails = 0 ] && echo "all window checks passed" || { echo "$fails failed"; exit 1; }

#!/usr/bin/env bash
# Rigorous-ish benchmark for Helix's Polars-backed DataFrame path.
# Isolates: interpreter startup, count-only, and the analytical query;
# across CSV vs Parquet and 5M/10M/50M rows; reports best-of-3 wall time and
# peak RSS. Warm-cache by default (note: WSL without root can't reliably drop
# page cache, so treat these as warm-cache numbers).
set -u
cd "$(dirname "$0")/.."
BIN=./target/release/helix
TIMER=/usr/bin/time

if [[ ! -x "$BIN" ]]; then echo "build release first: cargo build --release"; exit 1; fi

# --- generate the query scripts ---
echo 'print("ok")' > /tmp/b_startup.helix

q_count() { echo "print(read_${2}(\"$1\").count())" > "$3"; }
q_query() {
  # filter -> group -> mean -> sort -> head : the analytical workload (no count)
  echo "print(read_${2}(\"$1\").where(x > 500).group(grp).mean(y).sort(grp).head(5))" > "$3"
}

# best-of-3 wall (%e) and peak RSS (%M, KB) for a script
run() { # $1 label  $2 script
  local best=99999 rss=0 e m
  for _ in 1 2 3; do
    read -r e m < <("$TIMER" -f "%e %M" "$BIN" "$2" 2>&1 >/dev/null | tail -1)
    awk -v a="$e" -v b="$best" 'BEGIN{exit !(a<b)}' && best=$e
    rss=$m
  done
  printf "%-34s %8ss   %8.0f MB\n" "$1" "$best" "$(awk -v r="$rss" 'BEGIN{print r/1024}')"
}

echo "================ Helix DataFrame benchmark (release, warm cache) ================"
printf "%-34s %10s   %10s\n" "case" "best/3 wall" "peak RSS"
echo "--------------------------------------------------------------------------------"
run "interpreter startup (no data)" /tmp/b_startup.helix
echo "-- CSV --"
for n in 5 10 50; do
  q_count  "/tmp/d${n}.csv" csv  /tmp/b_c.helix; run "csv ${n}M  count()"            /tmp/b_c.helix
  q_query  "/tmp/d${n}.csv" csv  /tmp/b_q.helix; run "csv ${n}M  where+group+sort+head" /tmp/b_q.helix
done
echo "-- Parquet --"
for n in 5 10 50; do
  if [[ -f "/tmp/d${n}.parquet" ]]; then
    q_count "/tmp/d${n}.parquet" parquet /tmp/b_c.helix; run "parquet ${n}M  count()"            /tmp/b_c.helix
    q_query "/tmp/d${n}.parquet" parquet /tmp/b_q.helix; run "parquet ${n}M  where+group+sort+head" /tmp/b_q.helix
  fi
done
echo "================================================================================"

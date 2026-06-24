#!/usr/bin/env bash
# Helix vs raw Python-Polars on the identical 50M-row query (warm cache).
# Shows that Helix's "syntax -> lazy plan -> Polars" path adds ~no overhead over
# calling Polars directly: query execution is the same engine; Helix's end-to-end
# is competitive because a compiled binary skips Python's import tax.
set -u
cd "$(dirname "$0")/.."
HELIX=./target/release/helix
PY=/tmp/hxvenv/bin/python

cat > /tmp/q.py <<'PY'
import polars as pl, time, sys
path = sys.argv[1]
scan = pl.scan_parquet(path) if path.endswith(".parquet") else pl.scan_csv(path)
t0 = time.perf_counter()
(scan.filter(pl.col("x") > 500).group_by("grp").agg(pl.col("y").mean())
     .sort("grp").limit(5).collect())
sys.stderr.write(f"{time.perf_counter()-t0:.3f}\n")
PY
echo 'print(read_parquet("/tmp/d50.parquet").where(x > 500).group(grp).mean(y).sort(grp).head(5))' > /tmp/qp.helix

best() { # runs $@ three times, echoes min %e
  local b=99 e
  for _ in 1 2 3; do e=$(/usr/bin/time -f "%e" "$@" 2>&1 >/dev/null | tail -1)
    awk -v a="$e" -v c="$b" 'BEGIN{exit !(a<c)}' && b=$e; done
  echo "$b"
}
pyquery() { # min query-only (perf_counter from stderr) over 3 runs
  local b=99 e
  for _ in 1 2 3; do e=$($PY /tmp/q.py "$1" 2>&1 >/dev/null | tail -1)
    awk -v a="$e" -v c="$b" 'BEGIN{exit !(a<c)}' && b=$e; done
  echo "$b"
}

# warm up the cache
$HELIX /tmp/qp.helix >/dev/null 2>&1

echo "================ Helix vs Python-Polars (50M Parquet, warm, best/3) ================"
printf "  %-38s %ss\n" "Helix       total wall (binary)"       "$(best $HELIX /tmp/qp.helix)"
printf "  %-38s %ss\n" "Python      total wall (incl. import)" "$(best $PY /tmp/q.py /tmp/d50.parquet)"
printf "  %-38s %ss\n" "Python      query-only (pure Polars)"  "$(pyquery /tmp/d50.parquet)"
echo "==================================================================================="

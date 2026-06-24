#!/usr/bin/env bash
# Cold- vs warm-cache benchmark. Uses posix_fadvise(DONTNEED) to evict a file
# from the page cache without root (advisory, but the standard no-root method).
set -u
cd "$(dirname "$0")/.."
BIN=./target/release/helix

evict() {
  python3 - "$1" <<'PY'
import os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
os.close(fd)
PY
}
timeit() { /usr/bin/time -f "%e s wall, %M KB RSS" "$BIN" "$1" 2>&1 >/dev/null | tail -1; }

echo 'print(read_csv("/tmp/d50.csv").where(x > 500).group(grp).mean(y).sort(grp).head(5))' > /tmp/qc.helix
echo 'print(read_parquet("/tmp/d50.parquet").where(x > 500).group(grp).mean(y).sort(grp).head(5))' > /tmp/qp.helix

echo "================ cold vs warm (50M, release) ================"
echo "-- CSV 50M (1.4 GB) --"
sync; evict /tmp/d50.csv
echo "  cold: $(timeit /tmp/qc.helix)"
echo "  warm: $(timeit /tmp/qc.helix)"
echo "-- Parquet 50M (57 MB) --"
sync; evict /tmp/d50.parquet
echo "  cold: $(timeit /tmp/qp.helix)"
echo "  warm: $(timeit /tmp/qp.helix)"
echo "============================================================"

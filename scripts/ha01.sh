#!/usr/bin/env bash
for t in python3 go node gcc clang cc Rscript npx tsc deno bun; do
  if command -v "$t" >/dev/null 2>&1; then
    echo "HAVE $t -> $(command -v "$t")"
  else
    echo "MISS $t"
  fi
done

# Deploying Helix

Helix is a single self-contained binary with no runtime, no interpreter to install,
and ~0 startup — which makes it a good fit for containers, batch jobs, and serverless.
The build/packaging decisions here are recorded in
[ADR 0016](adr/0016-build-and-packaging.md).

## The binary

The core binary is self-contained (no Python, no system libraries beyond the C runtime
for the glibc build). It is built with:

- **mimalloc** as the global allocator (a pure runtime win; the fix for musl's slow
  default malloc).
- **PGO** (profile-guided optimization) on the primary `x86_64-linux-gnu` artifact,
  trained on `bench/crosslang`.
- A fully **static musl** variant (`helix-x86_64-unknown-linux-musl`) with no shared-
  library dependencies — for air-gapped use and the container image.

Install (prebuilt, glibc-PGO by default). The gnu build's glibc floor is **2.35**
(as of v0.4.0); `install.sh` falls back to the static musl binary automatically on
musl distros and on older glibc, and `HELIX_MUSL=1` forces it:

```sh
curl -LsSf https://raw.githubusercontent.com/<owner>/helix/main/install.sh | sh
```

## The appliance build

For small boxes where the full binary is mostly dead weight, the `appliance` feature
profile ([ADR 0032](adr/0032-appliance-profile.md)) keeps the full language surface —
HTTP, mimalloc, and the native DataFrame engine ([ADR 0033](adr/0033-native-dataframe-engine.md))
— while leaving out polars, the genomics readers, and the JIT:

```sh
cargo build --no-default-features --features appliance
```

That is ~9.3 MB stripped (gate profile) against ~76 MB for the full build. Frames run
on the native engine (filter/select/with/sort/group/join/unique/vstack/head, CSV and
parquet in both directions); a verb needing an absent backend says what to rebuild
with instead of failing obscurely.

## Docker

A multi-stage [`Dockerfile`](../Dockerfile) compiles the static musl binary and drops
it onto `distroless/static` — no shell, no package manager, no base OS, so the image is
essentially just the binary with ~zero OS/CVE surface.

```sh
docker build -t helix .
docker run --rm helix eval "print(1 + 2)"
docker run --rm -v "$PWD:/work" -w /work helix run script.helix
```

## Serverless / batch

Helix's ~0 cold start (no JVM warmup, no Python import tax) is its serverless advantage.
Two natural models:

1. **Container batch jobs** — the image on Cloud Run / AWS Fargate / AWS Batch, triggered
   per job. The natural fit for CPU-bound data and genomics pipelines (read → compute →
   query in one binary, no Python layer).
2. **Function runtime** — a custom-runtime handler that runs a `.helix` script per event.

Note: the Cranelift JIT emits executable memory and is x86_64-Linux-only; some
serverless sandboxes restrict that (W^X). The bytecode **VM** (`HELIX_NOJIT=1`) is the
universal fallback and remains fast, so run on the VM where the sandbox or architecture
(e.g. arm64) precludes the JIT. Since v0.4.0 the JIT is also a build-time gate (cargo
feature `jit`, on by default): a binary built without it carries no codegen at all and
runs identical bytecode on the VM.

## Capping CPU — `HELIX_THREADS`

Helix parallelizes array work across every core it can see. That is a **wall-clock-for-CPU
trade**, and whether it is the right one depends entirely on the workload — so it is a
setting, not a policy. `HELIX_THREADS=N` caps the worker pool; `HELIX_THREADS=1` runs fully
serial. Anything absent or unparsable leaves the default (one worker per core).

**Results never depend on it.** Parallel `map`/`filter` are elementwise so chunking cannot
reorder anything, float reductions are never reassociated (that would change the last bits
and break the three-engine oracle), and the parallel nested reduce partitions over
independent outer indices and collects in order. Pinned by
`thread_count_changes_cpu_not_results`, which also runs the whole corpus at
`HELIX_THREADS=1` and byte-compares.

Measured, min-of-4 on a 6-core box (gate profile), showing why one default cannot be right:

| workload | 1 thread | all cores | wall gain | total CPU |
|---|---|---|---|---|
| compute-bound (all-pairs, 3.6e9 distances) | 0.59s @ 99% | 0.11s @ 550% | **5.4×** | 0.58 → 0.61 core-s (**+4%**) |
| allocation-bound (dot, two 160 MB arrays) | 0.14s @ 96% | 0.08s @ 300% | 1.75× | 0.13 → 0.24 core-s (**+79%**) |

On compute-bound work the extra cores are nearly free and you should take them. On
allocation-bound work — where much of the time is the kernel faulting in and zeroing fresh
pages, not arithmetic — efficiency drops to ~45% and the last cores buy little: on that dot
product, 2 threads reach 0.10s for 0.14 core-s while all cores reach 0.08s for 0.24. If you
are billed per core-second, running several jobs on one box, or on a laptop, set
`HELIX_THREADS` to 1–2 and give up ~25% wall for ~45% less CPU.

**When to reach for it**
- Serverless/batch billed on CPU time, or with a hard core quota → set it to the quota.
- Many concurrent Helix processes → `HELIX_THREADS=1` each, and let the scheduler
  parallelize across jobs instead of within them (higher total throughput).
- Latency-critical single job on an idle machine → leave it unset.

## Status & roadmap

Built: mimalloc allocator, PGO CI pipeline, static musl artifact with the installer's
glibc-2.35-floor auto-fallback, the `appliance` profile (ADR 0032), distroless Dockerfile,
the `scripts/perf-verify.sh` regression gate. Deferred (see ADR 0016): BOLT, an
x86-64-v3 *extra* artifact with CPU detection, ready-made Lambda/Cloud-Run examples, and
a WASM/WASI edge build (interpreter+VM only — no JIT, limited Polars).

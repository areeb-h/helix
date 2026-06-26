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

Install (prebuilt, glibc-PGO by default; set `HELIX_MUSL=1` for the static binary):

```sh
curl -LsSf https://raw.githubusercontent.com/<owner>/helix/main/install.sh | sh
```

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
(e.g. arm64) precludes the JIT.

## Status & roadmap

Built: mimalloc allocator, PGO CI pipeline, static musl artifact, distroless Dockerfile,
the `scripts/perf-verify.sh` regression gate. Deferred (see ADR 0016): BOLT, an
x86-64-v3 *extra* artifact with CPU detection, ready-made Lambda/Cloud-Run examples, and
a WASM/WASI edge build (interpreter+VM only — no JIT, limited Polars).

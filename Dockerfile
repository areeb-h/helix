# Multi-stage build for a tiny, dependency-free Helix image (ADR 0016).
#
# The build stage compiles a fully-static musl binary (no shared libraries); the
# runtime stage is `distroless/static` — no shell, no package manager, no base OS —
# so the image is essentially just the binary, with ~zero OS/CVE surface. The mimalloc
# global allocator (default-on) replaces musl's slow default malloc, so the static
# build stays glibc-fast.
#
#   docker build -t helix .
#   docker run --rm helix eval "print(1 + 2)"
#   docker run --rm -v "$PWD:/work" -w /work helix run script.helix

# --- build -------------------------------------------------------------------------
FROM rust:1-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /src
COPY . .
# `.cargo/config.toml` applies `+crt-static` for this target; mimalloc is default-on.
RUN cargo build --release --target x86_64-unknown-linux-musl

# --- runtime -----------------------------------------------------------------------
FROM gcr.io/distroless/static-debian12
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/helix /usr/local/bin/helix
ENTRYPOINT ["/usr/local/bin/helix"]

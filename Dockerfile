FROM rust:1.98-alpine@sha256:a10e64dd139b7387337c7fbe8aca31b959b57b2fd4c8ae20a02cf1d6ea424dce AS build

# musl-dev supplies the linker; nothing here needs OpenSSL, since the engine is
# plain HTTP and the service terminates no TLS of its own.
RUN apk add --no-cache musl-dev

WORKDIR /src
# Manifests first so the dependency compile lands on its own cached layer.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Alpine's host target is x86_64-unknown-linux-musl, where Rust links musl
# statically by default — the equivalent of the old CGO_ENABLED=0 build.
RUN cargo build --release --locked

FROM docker.io/mwader/static-ffmpeg:8.1.2-amd64@sha256:3bfa407c614a29a4535f1e3220fd9f6bc9cd7c25483036962e3c8ff711b56e01
LABEL org.opencontainers.image.source="https://github.com/trexx/rust-acestream-proxy"

# The base image ships /ffmpeg and /ffprobe at the filesystem root; the service
# exec's them by name, so "/" must be on PATH.
ENV PATH="/usr/local/bin:/"

COPY --from=build /src/target/release/rust-acestream-proxy /usr/local/bin/rust-acestream-proxy

USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/rust-acestream-proxy"]

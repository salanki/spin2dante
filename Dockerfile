FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev pkgconfig alsa-lib-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    mkdir -p /out && \
    cargo build --release 2>&1 && \
    cp target/release/spin2dante /out/

FROM alpine:3
LABEL \
  org.opencontainers.image.source="https://github.com/salanki/spin2dante" \
  org.opencontainers.image.description="Bridge from Sendspin audio streams to DANTE via inferno_aoip." \
  org.opencontainers.image.licenses="GPL-3.0-or-later OR AGPL-3.0-or-later"
RUN apk add --no-cache alsa-lib libgcc
COPY --from=builder /out/spin2dante /usr/local/bin/
ENTRYPOINT ["/bin/sh", "-c", "mkdir -p ${TMPDIR:-/tmp} && exec spin2dante \"$@\"", "--"]

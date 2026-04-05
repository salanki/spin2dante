ARG BUILD_FROM=ghcr.io/home-assistant/base:latest
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev pkgconfig alsa-lib-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release 2>&1

FROM $BUILD_FROM
RUN apk add --no-cache alsa-lib libgcc
COPY --from=builder /build/target/release/spin2dante /usr/local/bin/spin2dante
COPY addons/spin2dante/run.sh /run.sh
RUN chmod a+x /run.sh
CMD ["/run.sh"]

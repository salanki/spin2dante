ARG BUILD_FROM=ghcr.io/home-assistant/base:latest
ARG BUILD_VERSION=edge
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev pkgconfig alsa-lib-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release 2>&1

FROM $BUILD_FROM
ARG BUILD_VERSION
LABEL \
  io.hass.version="${BUILD_VERSION}" \
  io.hass.type="addon" \
  io.hass.arch="aarch64|amd64" \
  io.hass.name="spin2dante" \
  io.hass.description="Bit-perfect bridge of one or more Sendspin audio streams to Audinate DANTE channels." \
  io.hass.url="https://github.com/salanki/spin2dante"
RUN apk add --no-cache alsa-lib jq libgcc
COPY --from=builder /build/target/release/spin2dante /usr/local/bin/spin2dante
COPY addons/spin2dante/run.sh /run.sh
RUN chmod a+x /run.sh
CMD ["/run.sh"]

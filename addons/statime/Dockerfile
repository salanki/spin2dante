ARG BUILD_FROM=ghcr.io/home-assistant/base:latest
ARG BUILD_VERSION=edge
ARG STATIME_REPOSITORY=https://github.com/teodly/statime.git
ARG STATIME_REF=inferno-dev

FROM rust:1-alpine AS builder
ARG STATIME_REPOSITORY
ARG STATIME_REF
RUN apk add --no-cache musl-dev git
WORKDIR /build
RUN git clone --recurse-submodules -b "$STATIME_REF" "$STATIME_REPOSITORY" .
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release -p statime-linux 2>&1

FROM $BUILD_FROM
ARG BUILD_VERSION
LABEL \
  io.hass.version="${BUILD_VERSION}" \
  io.hass.type="addon" \
  io.hass.arch="aarch64|amd64" \
  io.hass.name="statime" \
  io.hass.description="Export a PTP clock for spin2dante and inferno-based Dante tools." \
  io.hass.url="https://github.com/salanki/spin2dante"
RUN apk add --no-cache libgcc iproute2
COPY --from=builder /build/target/release/statime /usr/local/bin/statime
COPY statime/statime-docker.toml /etc/statime.toml.template
COPY addons/statime/run.sh /run.sh
RUN chmod a+x /run.sh
CMD ["/run.sh"]

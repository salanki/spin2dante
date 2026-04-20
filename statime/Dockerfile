FROM rust:1-alpine AS builder
ARG STATIME_REPOSITORY=https://github.com/teodly/statime.git
ARG STATIME_REF=inferno-dev
RUN apk add --no-cache musl-dev git
WORKDIR /build
RUN git clone --recurse-submodules -b "$STATIME_REF" "$STATIME_REPOSITORY" .
# Fix: unwrap_or -> unwrap_or_else so manual identity config works on VMs/Docker
# (get_clock_id panics on locally-administered MACs, but unwrap_or evaluates eagerly)
RUN sed -i 's/\.unwrap_or(ClockIdentity(/\.unwrap_or_else(|| ClockIdentity(/' statime-linux/src/main.rs
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release -p statime-linux 2>&1

FROM alpine:3
LABEL \
  org.opencontainers.image.source="https://github.com/salanki/spin2dante" \
  org.opencontainers.image.description="PTP daemon image for exporting a usrvclock socket for spin2dante and inferno-based Dante tools." \
  org.opencontainers.image.licenses="GPL-3.0-or-later OR AGPL-3.0-or-later"
RUN apk add --no-cache libgcc
COPY --from=builder /build/target/release/statime /usr/local/bin/statime
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
# Default config — can be overridden via volume mount or STATIME_CONFIG env
COPY statime-docker.toml /etc/statime.toml
ENTRYPOINT ["/entrypoint.sh"]

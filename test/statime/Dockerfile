FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev git
WORKDIR /build
# Use our fork with the unwrap_or_else fix for Docker/VM compatibility
RUN git clone --recurse-submodules -b inferno-dev https://github.com/teodly/statime.git .
# Fix: unwrap_or -> unwrap_or_else so manual identity config works on VMs/Docker
# (get_clock_id panics on locally-administered MACs, but unwrap_or evaluates it eagerly)
RUN sed -i 's/\.unwrap_or(ClockIdentity(/\.unwrap_or_else(|| ClockIdentity(/' statime-linux/src/main.rs
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release -p statime-linux 2>&1

FROM alpine:3
RUN apk add --no-cache libgcc
COPY --from=builder /build/target/release/statime /usr/local/bin/statime
COPY statime-docker.toml /etc/statime.toml
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]

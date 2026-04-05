FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev git
WORKDIR /build
RUN git clone --recurse-submodules -b inferno-dev https://github.com/teodly/statime.git .
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release -p statime-linux 2>&1

FROM alpine:3
RUN apk add --no-cache libgcc
COPY --from=builder /build/target/release/statime /usr/local/bin/statime
COPY statime-docker.toml /etc/statime.toml
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]

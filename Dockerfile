FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev pkgconfig alsa-lib-dev
WORKDIR /build

# Copy everything (inferno-fork is either a symlink or copied by build context)
COPY . .

ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release 2>&1

FROM alpine:3
RUN apk add --no-cache alsa-lib libgcc
COPY --from=builder /build/target/release/spin2dante /usr/local/bin/
ENTRYPOINT ["/bin/sh", "-c", "mkdir -p ${TMPDIR:-/tmp} && exec spin2dante \"$@\"", "--"]

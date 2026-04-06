# Testing Guide

All testing runs in Docker containers — no local Rust toolchain, ALSA, or PTP daemon required.

## Prerequisites

- Docker with BuildKit (Docker Desktop or `docker buildx`)
- The inferno repo cloned alongside this repo with submodules initialized:
  ```
  projects-tmp/
  ├── inferno/          # git submodule update --init --recursive
  └── sendspin-bridge/  # this repo
  ```

## Quick Start

```sh
# 1. Build inferno2pipe image (only needed once, or after inferno updates)
cd ../inferno
git submodule update --init --recursive
docker build -f Dockerfile.alpine-i2pipe -t inferno_aoip:alpine-i2pipe .

# 2. Run the E2E test
cd ../sendspin-bridge/test
docker compose down --remove-orphans 2>/dev/null
docker compose up --build
```

Or use the wrapper script:
```sh
cd test
./run_test.sh
```

## Test Architecture

Six Docker containers on a shared bridge network:

```
┌──────────────┐  ┌──────────────────┐  ┌───────────────┐
│ clock_source │  │ sendspin_source   │  │ control_and_  │
│ (usrvclock)  │  │ (Python sendspin) │  │ test          │
│ PTP clock    │  │ 1kHz sine WAV    │  │ (netaudio +   │
│ for all      │  │ served via WS    │  │  signal check)│
└──────┬───────┘  └────────┬─────────┘  └───────┬───────┘
       │                   │ WebSocket           │ netaudio
       │    ┌──────────────▼───────────┐         │ subscription
       ├───→│     bridge               │         │
       │    │  (sendspin_bridge)       │◄────────┘
       │    │  DANTE TX: "SSBridge"    │
       │    └──────────────┬───────────┘
       │                   │ DANTE multicast UDP
       │    ┌──────────────▼───────────┐
       └───→│     i2pipe               │
            │  (inferno2pipe)          │
            │  captures to .raw file   │
            └──────────────────────────┘
```

### Container Details

| Container | Image | Role |
|-----------|-------|------|
| `init` | `alpine:3` | Clears the shared volume before each run |
| `clock_source` | Built from `inferno/test/dockerized_trx/fake_usrvclock_server` | Fake PTP clock. Creates a Unix datagram socket at `/shared/usrvclock` |
| `sendspin_source` | `python:3.13-alpine` + `pip install sendspin` | Generates a 30s 1kHz sine WAV, serves it via `sendspin serve` on port 8927 |
| `bridge` | Built from this repo's `Dockerfile` | The bridge under test. Connects to sendspin_source, transmits as DANTE device "SSBridge" |
| `i2pipe` | `inferno_aoip:alpine-i2pipe` (pre-built) | DANTE receiver. Captures audio to `/shared/capture.raw` |
| `control_and_test` | `python:3.13-alpine` + `netaudio` | Orchestrator: discovers DANTE devices, creates subscriptions, validates captured audio |

## Critical: The usrvclock TMPDIR Gotcha

The fake PTP clock uses Unix datagram sockets. The server creates a socket at `/shared/usrvclock`. Each client (bridge, i2pipe) creates a response socket in `$TMPDIR`. The server sends clock overlays back to these client sockets.

**The client TMPDIR must be on the shared Docker volume.** If TMPDIR is `/tmp` (container-local), the clock_source container can't reach the client sockets and you get:

```
clock_source  | sendto failed: No such file or directory
bridge        | clock unavailable, can't transmit. is the PTP daemon running?
```

Fix: set `TMPDIR=/shared/tmp_<container>` and `mkdir -p` it before starting the process. Each container needs a unique TMPDIR subdirectory to avoid socket name collisions.

```yaml
# docker-compose.yml
bridge:
  environment:
    TMPDIR: /shared/tmp_bridge
  entrypoint: ["/bin/sh", "-c"]
  command: ["mkdir -p /shared/tmp_bridge && exec sendspin_bridge ..."]
```

The clock_source also needs `USRVCLOCK_SOCKET=/shared/usrvclock` set explicitly.

## Critical: inferno2pipe Image Must Be Pre-Built

The `i2pipe` service uses `image: inferno_aoip:alpine-i2pipe` (not `build:`). This is because the inferno Dockerfile expects the full inferno repo as build context with submodules initialized, which docker-compose can't reliably do with a cross-repo context path.

Build it manually before running tests:
```sh
cd ../inferno
git submodule update --init --recursive
docker build -f Dockerfile.alpine-i2pipe -t inferno_aoip:alpine-i2pipe .
```

If you see this error, you forgot submodules:
```
failed to read `/build/searchfire/Cargo.toml`: No such file or directory
```

## Startup Timing

The containers start in dependency order, but some take time to initialize:

1. **sendspin_source** (~15-20s): pip installs sendspin, generates WAV, starts server
2. **bridge** retries connection every 2s until sendspin_source is ready
3. **i2pipe** sleeps 5s before starting (gives bridge time to register as DANTE device)
4. **FlowsTransmitter** may log "clock unavailable" for ~10-20s until it receives its first PTP overlay
5. **control_and_test** waits up to 90s for both DANTE devices to appear via mDNS

Total time from `docker compose up` to audio flowing: ~30-40s.

## DANTE Device Discovery and Subscriptions

The `control_and_test` container uses [netaudio](https://pypi.org/project/netaudio/) to discover and connect DANTE devices.

The bridge's DANTE device name includes a random suffix based on its MAC/IP (e.g., "SSBridge ac150004"). The test script extracts this dynamically:

```sh
bridge_name=$(netaudio device list | grep "SSBridge" | awk '{print $1, $2}')
netaudio subscription add --tx "01@${bridge_name}" --rx "01@i2pipe"
```

The `netaudio subscription add` syntax is `--tx "channel@device" --rx "channel@device"`. Channel names are "01", "02" (factory names from inferno).

## Inspecting Test Results

### During a run
```sh
# All logs interleaved
docker compose logs -f

# Just the bridge
docker compose logs -f bridge

# Just control output
docker compose logs control_and_test
```

### Key log lines to look for

**Success indicators:**
```
bridge  | connected to Sendspin server
bridge  | stream start: codec=pcm rate=48000 ch=2 bits=24
bridge  | starting FlowsTransmitter (start_time=0)
bridge  | prebuffer complete (2400 samples written), fill=..., now transmitting
bridge  | [buffer] fill=... target=2400 ...

control_and_test | Signal present: YES
control_and_test | Capture file size OK
```

**Failure indicators:**
```
bridge  | clock unavailable, can't transmit     # Normal at startup, bad if persistent
bridge  | rejecting stream: ...                  # Format mismatch
clock_source | sendto failed                     # TMPDIR not on shared volume
control_and_test | TIMEOUT: devices not found    # Network or startup issue
control_and_test | Signal present: NO            # Audio not flowing
```

### Capture file format

`/shared/capture.raw` is written by inferno2pipe:
- Format: signed 32-bit integer, native endian (little-endian on x86)
- Layout: interleaved stereo (L0 R0 L1 R1 ...)
- Sample rate: 48000 Hz
- Inferno's internal format: 24-bit value in **upper 24 bits** of i32

Convert to WAV for listening:
```sh
docker run --rm -v $(pwd)/../test-shared:/shared alpine:3 sh -c "
  apk add sox &&
  sox --no-dither -t raw -e signed-integer -b 32 -c 2 -r 48000 \
    /shared/capture.raw /shared/capture.wav
"
```

## Running Tests Manually (Step by Step)

Useful for debugging when `docker compose up` doesn't give you enough control:

```sh
cd test

# 1. Start infrastructure
docker compose up -d init clock_source sendspin_source

# 2. Wait for sendspin to be ready (~20s)
sleep 20
docker compose logs sendspin_source | tail -5

# 3. Start the bridge
docker compose up -d bridge

# 4. Wait for bridge to connect and start DANTE device (~15s)
sleep 15
docker compose logs bridge | tail -20

# 5. Start i2pipe
docker compose up -d i2pipe

# 6. Start test orchestrator
docker compose up control_and_test

# 7. Check bridge metrics while running
docker compose logs bridge | grep "\[buffer\]"

# 8. Clean up
docker compose down --remove-orphans
```

## Rebuilding After Code Changes

```sh
# Rebuild just the bridge image (uses Docker cache for dependencies)
cd ..
docker build -t sendspin-bridge .

# Then re-run tests
cd test
docker compose down --remove-orphans
docker compose up --build
```

The `--build` flag rebuilds changed images. The bridge Dockerfile doesn't cache Cargo dependencies between builds (no separate dep-fetch layer), so a full rebuild takes ~2 minutes.

## Known Limitations

- **No automated pass/fail**: The test checks signal presence but doesn't verify bit-perfect output or exact waveform shape. WavDiff comparison is planned.
- **Sendspin source codec**: The `sendspin serve` command decides the codec. With a local WAV file it typically sends PCM, but behavior may vary by version.
- **FlowsTransmitter startup delay**: 10-20s of "clock unavailable" is normal while the PTP clock propagates through inferno's internal channels.
- **Single-run test audio**: The 30s test tone loops only if sendspin loops it. After 30s, the stream may end.

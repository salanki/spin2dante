# Testing Guide

All testing runs in Docker containers — no local Rust toolchain, ALSA, or PTP daemon required.

## Prerequisites

- Docker with BuildKit (Docker Desktop or `docker buildx`)
- The inferno repo cloned alongside this repo with submodules initialized:
  ```
  projects-tmp/
  ├── inferno/          # git submodule update --init --recursive
  └── spin2dante/  # this repo
  ```

## Music Assistant Test Server

For manual bridge testing against a real Music Assistant instance, a standalone
Docker setup is provided in `test/music_assistant/`.

Why this setup:
- Runs entirely in Docker, with no host package installs
- Uses `network_mode: host`, which is the simplest option for player discovery
- Keeps persistent MA state in `test/music_assistant/data/`
- Optionally mounts `test/music_assistant/media/` read-only at `/media`

Start it:
```sh
make ma-up
```

Stop it:
```sh
make ma-down
```

Tail logs:
```sh
make ma-logs
```

Then open Music Assistant at:
```text
http://localhost:8095
```

Notes:
- The first start may take a bit while the image is pulled and the server initializes.
- If you want MA to see local test files, place them under `test/music_assistant/media/`
  and add that folder as a local filesystem provider from the MA UI.
- By default, the compose file uses the official published image
  `ghcr.io/music-assistant/server:latest`.
- The helper scripts intentionally use a clean temporary Docker config at
  `/tmp/music-assistant-docker-config`. This avoids stale `ghcr.io` credentials
  in `~/.docker/config.json` causing GHCR pulls to fail.

## Quick Start

```sh
# Single-stream E2E test (1 bridge, 1 receiver)
make test

# Multi-stream E2E test (4 bridges in one Sendspin group, 4 receivers)
make test-multi

# Override inferno location if needed
make test INFERNO_DIR=/path/to/inferno
```

The Makefile handles building the bridge image, the inferno2pipe image (with submodule init), the Music Assistant helper workflow, and the docker-compose based test runs.

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
       │    │  (spin2dante)       │◄────────┘
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
| `ptp-master` | Built from `statime/` | PTPv2 clock master (reference clock) |
| `ptp-follower` | Built from `statime/` | PTPv2 clock follower — syncs to master, exports usrvclock |
| `sendspin_source` | `python:3.13-alpine` + `pip install sendspin` | Generates a 30s 1kHz sine WAV, serves it via `sendspin serve` on port 8927 |
| `bridge` | Built from this repo's `Dockerfile` | The bridge under test. Connects to sendspin_source, transmits as DANTE device "SSBridge" |
| `i2pipe` | `inferno_aoip:alpine-i2pipe` (pre-built) | DANTE receiver. Captures audio to `/shared/capture.raw` |
| `control_and_test` | `python:3.13-alpine` + `netaudio` | Orchestrator: discovers DANTE devices, creates subscriptions, validates captured audio |

## Critical: The usrvclock TMPDIR Gotcha

The Statime PTP follower exports clock overlays via Unix datagram sockets (usrvclock protocol). The server creates a socket at `/shared/usrvclock`. Each client (bridge, i2pipe) creates a response socket in `$TMPDIR`. The server sends clock overlays back to these client sockets.

**The client TMPDIR must be on the shared Docker volume.** If TMPDIR is `/tmp` (container-local), the clock_source container can't reach the client sockets and you get:

```
ptp-follower  | sendto failed: No such file or directory
bridge        | clock unavailable, can't transmit. is the PTP daemon running?
```

Fix: set `TMPDIR=/shared/tmp_<container>` and `mkdir -p` it before starting the process. Each container needs a unique TMPDIR subdirectory to avoid socket name collisions.

```yaml
# docker-compose.yml
bridge:
  environment:
    TMPDIR: /shared/tmp_bridge
  entrypoint: ["/bin/sh", "-c"]
  command: ["mkdir -p /shared/tmp_bridge && exec spin2dante ..."]
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

1. **sendspin_source** (~5-10s): generates 30s WAV test tone, starts sendspin server (pip install happens at image build time)
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
docker build -t spin2dante .

# Then re-run tests
cd test
docker compose down --remove-orphans
docker compose up --build
```

The `--build` flag rebuilds changed images. The bridge Dockerfile doesn't cache Cargo dependencies between builds (no separate dep-fetch layer), so a full rebuild takes ~2 minutes.

## Multi-Stream Test (`make test-multi`)

Tests 4 bridge instances all connected to the same Sendspin server, simulating a real multi-room deployment where all zones play the same stream in sync.

### What it does

- 1 Sendspin server serving a 1kHz test tone
- 4 bridge containers (SS01–SS04), each a separate DANTE TX device
- 4 i2pipe containers (rx01–rx04), each capturing one bridge's output
- 1 control container that creates all 8 subscriptions and analyzes results

### What it measures

1. **Signal presence**: Each of the 4 captures is checked for non-zero audio
2. **Cross-stream sync**: Compares the onset (first non-zero sample) across all 4 captures and reports the spread in samples and milliseconds

```
Onset spread: 48 samples (1.00ms)
SYNC: GOOD (spread < 10ms)
```

Sync quality thresholds:
- **GOOD**: < 10ms spread (< 480 samples)
- **FAIR**: < 100ms spread
- **POOR**: >= 100ms spread

### Resource requirements

35 containers total (16 bridges + 16 receivers + clock + source + control). Each inferno DeviceServer uses a real-time thread. Expect:
- ~2-3GB RAM
- Significant CPU during startup (all containers building/initializing in parallel)
- ~3-4 minutes total runtime (builds + discovery timeout + 20s recording)

### Container naming

The test harness uses explicit `INFERNO_DEVICE_ID` values (not `PROCESS_ID`/`ALT_PORT`) because all containers are on a Docker bridge network with unique IPs, unlike production where all bridges share the host IP. This is a harness-specific shortcut.
- Bridge IDs: `0000000000000101` through `0000000000000110`
- Receiver IDs: `0000000000000201` through `0000000000000210`

## PTP Clock Architecture (all tests)

All tests use real PTPv2 clock synchronization via two Statime instances:

```
┌────────────────────┐
│  Statime PTPv2     │ ← PTP master (clock reference)
│  MASTER            │    Does NOT export usrvclock
└────────┬───────────┘
         │ PTP sync messages (multicast)
         ▼
┌────────────────────┐
│  Statime PTPv2     │ ← PTP follower (syncs to master)
│  FOLLOWER          │    EXPORTS usrvclock overlays
└────────┬───────────┘
         │ usrvclock (Unix datagram socket)
         ▼
┌────────────────────┐     ┌──────────────────┐
│  spin2dante bridge │ ──→ │ inferno2pipe     │
│  (DANTE TX)        │     │ (DANTE RX)       │
└────────────────────┘     └──────────────────┘
         ↑                          ↑
         └──── both read from ──────┘
               /shared/usrvclock
```

### Why master + follower

Only PTP **followers** export usrvclock overlays. A master doesn't adjust its clock, so the overlay export callback never fires. The follower syncs to the master and exports overlays that inferno reads.

### Auto-realignment

The bridge starts writing at local-domain position 0. When the PTP clock warms up and the FlowsTransmitter starts reading at PTP-domain positions (~140 billion), the bridge detects the misalignment (write_pos and read_pos more than ring buffer size apart) and calls `snap_to_live()` to realign write_pos to the PTP domain.

### Config files

- `statime/statime-ptpv2-master.toml` — PTPv2 master, no usrvclock export
- `statime/statime-ptpv2-follower.toml` — PTPv2 follower, exports usrvclock

### Timing notes

The follower needs ~10-15s to sync with the master and start exporting overlays. The bridge auto-realigns once the clock becomes available — no manual timing coordination needed.

## Edge Case Behavior

Tested and validated via `make test-resilience`:

| Scenario | Bridge behavior |
|----------|----------------|
| **Sendspin server disconnects** | Bridge detects disconnect, logs "session ended with error", zeros ring, enters Idle, waits 2s, auto-reconnects. DANTE device stays on network. |
| **Stream seek (StreamClear)** | Stale buffered audio is zeroed from current read position. Bridge enters rebuffer mode, refills jitter buffer with fresh data, then resumes. |
| **Stream ends (StreamEnd)** | Ring zeroed, bridge enters Idle. DANTE device stays on network. |
| **New stream with same format (StreamStart, already Running)** | Stale audio cleared, rebuffer mode, no device restart. |
| **New stream with different format** | Format validated; if supported (PCM 16/24-bit), stream_format updated and enters WaitingForSubscriber. Unsupported formats rejected. Device is NOT restarted. |
| **PTP master disappears** | Statime stops exporting clock overlays. FlowsTransmitter reports "clock unavailable" until PTP master returns. Audio stops but bridge stays alive. |
| **Multiple StreamStart without StreamEnd** | If already Running with same format: clear + rebuffer. Otherwise: enter WaitingForSubscriber. |

## Known Limitations

- **No automated pass/fail**: The test checks signal presence but doesn't verify bit-perfect output or exact waveform shape. WavDiff comparison is planned.
- **Sendspin source codec**: The `sendspin serve` command decides the codec. With a local WAV file it typically sends PCM, but behavior may vary by version.
- **PTP clock warmup**: 10-15s of "clock unavailable" is normal while the Statime follower syncs to the master. The bridge auto-realigns once the clock becomes available.
- **Single-run test audio**: The 30s test tone loops only if sendspin loops it. After 30s, the stream may end.
- **FLAC not testable**: sendspin-rs v0.1 only has a PCM decoder. FLAC testing requires either a newer sendspin-rs or a custom decoder.

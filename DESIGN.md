# spin2dante — Design Document

## Context

This bridge streams audio from Sendspin sources (e.g., Music Assistant) to DANTE receivers without going through the ALSA subsystem. It's a direct protocol-to-protocol bridge: receive audio via Sendspin's WebSocket protocol, write it into inferno_aoip's transmit ring buffers, and let the DANTE TX engine send it on the network. The result is a completely userspace, bit-perfect (for PCM) audio bridge.

## Architecture

```
Sendspin Server (Music Assistant)
        │ WebSocket (PCM audio chunks)
        ▼
┌─────────────────────┐
│    spin2dante        │  ← this crate
│                      │
│  1. Connect as player│
│  2. Receive audio    │
│  3. Deinterleave     │
│  4. Atomic write to  │
│     ring buffers     │
└────────┬────────────┘
         │ Vec<Atomic<i32>> (self-owned, per channel)
         │ exposed via ExternalBufferParameters
         ▼
┌─────────────────────┐
│   inferno_aoip       │  ← UNMODIFIED upstream dependency
│   DeviceServer       │
│                      │
│  FlowsTransmitter   │
│  reads ring buffers  │
│  at PTP timestamps   │
│  → DANTE UDP packets │
└─────────────────────┘
         │ Multicast UDP :4321
         ▼
   DANTE Receivers
```

**Key design decision**: Zero changes to inferno_aoip. The bridge uses the existing public `ExternalBufferParameters` + `transmit_from_external_buffer` API.

## Clock Model

### Model: "start at zero, infer running position"

The bridge sends `start_time = 0` to inferno immediately. This means:

```
timestamp_shift = -0 - latency = -latency
read_pos = (ptp_now - latency) % RING_BUFFER_SIZE
```

Since `ExternalBuffer::unconditional_read()` returns true, inferno reads whatever is in the ring buffer at `read_pos` without checking write/readable positions. The bridge writes at monotonically increasing positions that wrap around the ring buffer naturally.

The bridge does NOT obtain a PTP clock directly. Instead, it uses the `PositionReportDestination` mechanism to observe where inferno is reading. The jitter buffer is maintained by keeping `write_pos` ahead of `read_pos`.

### Why not PTP-anchored start_time?

The `get_realtime_clock_receiver()` API returns a `RealTimeBoxReceiver` designed for real-time threads. On a single-threaded tokio runtime, the background task that feeds it may not deliver overlays reliably to user code. The "start at zero" model avoids this issue entirely and still produces correct audio output.

### Device Lifetime

The DANTE device (DeviceServer + TX) is started **once at process startup** and stays alive for the entire process lifetime. This matches standard DANTE behavior where devices are persistent network entities. Subscriptions configured in Dante Controller persist across playback sessions.

### Startup sequence

1. `DeviceServer::start()` — creates DANTE device, blocks until PTP clock available
2. Ring buffer created and zeroed (silence)
3. `transmit_from_external_buffer()` — registers ring buffers with inferno
4. `start_tx.send(0)` — starts FlowsTransmitter (idle, transmitting silence)
5. Bridge enters **Idle** state — device visible on network, ring silent
6. Connect to Sendspin server (retry loop)
7. On StreamStart → enter **WaitingForSubscriber**

The FlowsTransmitter may report "clock unavailable" until it receives its first PTP overlay. This is non-blocking — the device is still registered and visible on the network.

### Why WaitingForSubscriber exists

When a stream starts, the bridge writes audio to the ring buffer. But inferno reads at `(ptp_now - latency) % RING_BUFFER_SIZE` — a position determined by the PTP clock, not by our write position. If no subscriber has connected yet, or the PTP clock isn't available, the read and write positions are in different domains.

WaitingForSubscriber monitors `read_pos` from `PositionReportDestination`. When `read_pos` starts advancing (subscriber connected + clock working), the bridge calls `snap_to_live()`:
1. Zero-fill `[read_pos, read_pos + prebuffer_target)` — silence during prebuffer
2. Set `write_pos = read_pos + prebuffer_target` — fresh audio lands right after
3. Enter Prebuffering

If `read_pos` doesn't advance within 5 seconds (clock may still be warming up), the bridge falls back to Prebuffering without snap-to-live alignment. This is a pragmatic degradation for environments where clock overlay propagation is slow.

## State Machine

```
process start → Idle (device + TX alive, ring silent)
                  │
            StreamStart
                  ▼
        WaitingForSubscriber → Prebuffering → Running
                  ↑                 ↑              │
                  │                 │ StreamClear   │
                  │                 └─Rebuffering ──┘
                  │                       │
                  │             StreamEnd │
                  └────── Idle ←──────────┘
```

All states have the DANTE device + TX alive. The difference is what audio is in the ring:

- **Idle**: Ring is explicitly zeroed (silence). No stale audio can loop.
- **WaitingForSubscriber**: Discardable scratch audio in ring, waiting for subscriber + clock.
- **Prebuffering**: Fresh audio accumulating after snap_to_live (or timeout fallback).
- **Running**: Live audio with correct jitter buffer.
- **Rebuffering**: Zero-fill + fresh audio after seek/clear.

### Stream lifecycle handling

- **StreamStart**: Enter WaitingForSubscriber (or snap_to_live if subscriber already active)
- **StreamStart (same format, already Running)**: Clear stale audio, enter Rebuffering
- **StreamClear**: Zero-fill from read_pos, jump write_pos ahead, enter Rebuffering
- **StreamEnd**: Zero entire ring, reset stream state, enter Idle (device stays on network)
- **Sendspin disconnect**: Same as StreamEnd — zero ring, enter Idle, reconnect

### Clear/Rebuffer behavior

On stream/clear, the bridge must discard stale audio that inferno is about to read:

1. Read `read_pos` from `PositionReportDestination`
2. Zero-fill ring buffer positions `[read_pos, read_pos + prebuffer_target)`
3. Set `write_pos = read_pos + prebuffer_target`
4. Enter Rebuffering state

This ensures inferno reads silence immediately (not stale pre-seek audio), then the bridge refills the jitter buffer with fresh data.

### Subscriber reconnect alignment

If a DANTE subscriber disconnects and reconnects (or a new subscriber joins while a stream is active), the bridge must ensure the subscriber hears current audio, not stale data left in the ring buffer from an earlier write position.

The mechanism: when `read_pos` starts advancing at a new position (detected in WaitingForSubscriber or during Running state monitoring), `snap_to_live()` fires:

1. Read the subscriber's current `read_pos`
2. Zero-fill `[read_pos, read_pos + prebuffer_target)` — brief silence
3. Set `write_pos = read_pos + prebuffer_target`
4. Enter Prebuffering with fresh audio

This anchors the bridge's write position to wherever the subscriber is reading NOW. The subscriber gets current audio with only a prebuffer-sized gap of silence (~50ms), regardless of how long the bridge was writing to the ring before the subscriber appeared.

**Note:** This mechanism requires `read_pos` from `PositionReportDestination` to advance, which only happens when inferno's FlowsTransmitter has a valid PTP clock and is actively reading. In Docker test environments with the fake clock, the clock overlay may not propagate reliably to the FlowsTransmitter, so a 5-second timeout fallback enters Prebuffering without snap-to-live alignment. In production with a real PTP clock (Statime synced to DANTE hardware), snap_to_live works correctly.

## Data Path

1. Sendspin delivers `AudioChunk { data: Arc<[u8]> }` — raw PCM bytes over WebSocket
2. Bridge parses bytes, deinterleaves (L/R), and shifts samples to inferno format
3. Bridge writes via `Atomic::store()` into self-owned `Vec<Atomic<i32>>` ring buffers
4. FlowsTransmitter reads via `Atomic::load()` at PTP-synchronized timestamps

At stereo 48kHz 32-bit, that's ~384KB/s — negligible overhead.

## Sample Format Alignment

- **Sendspin PCM**: 24-bit little-endian signed integers (3 bytes per sample, interleaved)
- **Inferno `Sample`**: i32 with 24-bit value in **upper 24 bits**
- **Conversion**: Parse 24-bit LE → sign-extend to i32 → shift left by 8

```rust
let raw = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
let sign_extended = (raw << 8) >> 8;
let inferno_sample = sign_extended << 8;
```

Lossless and bit-perfect for PCM.

## Jitter Buffer Monitoring

Uses `PositionReportDestination` to get the actual read position from FlowsTransmitter.

Periodic console metrics (every ~5s):

```
[buffer] fill=2412 target=2400 drift=+1.2ppm min=2388 max=2436 underruns=0 overruns=0
```

## Format Enforcement

The bridge **rejects** streams at StreamStart that don't match:
- Sample rate must be 48000 Hz
- Channel count must be 2 (stereo)
- Codec must be "pcm"
- Bit depth must be 16 or 24

## Reconnection

The bridge has an outer reconnect loop. If the WebSocket drops (server restart, network issue), it zeros the ring buffer, enters Idle, waits 2 seconds, and reconnects. The DANTE device stays on the network — only the stream state resets.

## Codec Support

| Codec | Status | Notes |
|-------|--------|-------|
| PCM   | Supported, bit-perfect | 16-bit and 24-bit LE |
| FLAC  | Not yet supported | sendspin-rs v0.1 only has PCM decoder |
| Opus  | Not supported | Lossy codec |
| MP3   | Not supported | Lossy codec |

## Multi-Stream Deployment

One bridge process per Sendspin stream. For 32 streams: 32 containers, each with unique `--name`, `INFERNO_PROCESS_ID`, and `INFERNO_ALT_PORT`. Device ID is auto-derived from host IP + process ID.

## Configuration

### CLI Arguments
- `--url` / `-u`: Sendspin server WebSocket URL (required)
- `--name` / `-n`: DANTE device name (default: "Sendspin Bridge")
- `--buffer-ms`: Jitter buffer size in ms (default: 50)

### Environment Variables (passed through to inferno_aoip)
- `INFERNO_CLOCK_PATH`, `INFERNO_SAMPLE_RATE`, `INFERNO_BIND_IP`
- `INFERNO_PROCESS_ID`, `INFERNO_ALT_PORT` (required for multiple bridges on same host)
- `INFERNO_TX_LATENCY_NS`

## Lessons Learned

Hard-won knowledge from development and testing. Read this before modifying the clock or buffer logic.

### The PTP clock chain

```
DANTE devices ←PTP→ Statime daemon
                        │
                        │ usrvclock (Unix datagram socket)
                        ▼
                  inferno AsyncClient (tokio task)
                        │
                        │ watch::Sender<Option<ClockOverlay>>
                        ▼
                  watch::Receiver (per subscriber)
                        │
                        │ RealTimeBoxReceiver (lock-free channel)
                        ▼
                  FlowsTransmitter (real-time thread, priority 81)
```

Statime syncs with PTP masters on the network and exports a `ClockOverlay` (shift + frequency offset) via the usrvclock protocol. Inferno's `AsyncClient` receives this and publishes it through a `watch` channel. The `FlowsTransmitter` reads it via a `RealTimeBoxReceiver` designed for RT threads.

### Why "start at zero" instead of PTP-anchored start_time

The original plan was to obtain the PTP media clock time and send it as `start_time` to anchor ring buffer positions to PTP time. This failed because:

1. **`get_realtime_clock_receiver()` doesn't reliably deliver overlays on single-threaded tokio.** The method creates a `RealTimeBoxReceiver` fed by a background tokio task. On a `current_thread` runtime, this task only runs when the main code yields. Polling in a loop with `yield_now()` + `sleep()` should work but the initial overlay was frequently `None`.

2. **Chicken-and-egg with `current_timestamp`.** The `FlowsTransmitter` only writes to `current_timestamp` after receiving `start_time`, but we wanted to use `current_timestamp` to compute `start_time`.

The "start at zero" model avoids both problems: send `start_time=0` immediately, let the transmitter start, and write at monotonically increasing positions. Since `ExternalBuffer::unconditional_read()` returns `true`, inferno reads any ring buffer position without checking write status — our writes just need to stay ahead of reads.

### Why Statime can't be a standalone PTP master in Docker

Only PTP **followers** export usrvclock overlays. A PTP master doesn't adjust its clock (it IS the reference), so the overlay export callback never fires. This is true regardless of whether the master has followers or not.

For production, you need:
1. A PTP master on the network (DANTE hardware, or Statime in PTPv2 master mode)
2. A Statime **follower** that syncs to the master and exports usrvclock

The `fake_usrvclock_server` (used in `make test`) sidesteps this by sending periodic overlays unconditionally (every 1 second), but the overlay values are not PTP-synchronized — they're based on `CLOCK_MONOTONIC_RAW` with zero offset.

For Docker-only testing with real PTP: `make test-ptpv2` runs a Statime master + follower pair. The follower syncs to the master and exports valid overlays.

### PositionReportDestination limitation

Inferno's `ExternalBuffer::unconditional_read()` returns `true`, which skips the `readable_pos` update in `RBOutput::read_at()`. This means `PositionReportDestination` is never updated for our ExternalBuffer-based ring buffers, even when the FlowsTransmitter IS actively reading and sending packets.

Consequence: `read_pos` from PositionReportDestination is always 0. Buffer metrics (fill, drift, subscriber detection via read_pos) are unreliable. The WaitingForSubscriber snap_to_live mechanism cannot trigger. Audio flows correctly regardless — this only affects observability.

### Why VMs and Docker virtual NICs break Statime identity

Statime derives its PTP clock identity from the NIC's MAC address, filtering out locally-administered MACs (bit 1 of first octet set). Docker's virtual NICs (`02:xx:xx`) and many VM hypervisors generate locally-administered MACs, causing `get_clock_id()` to find no valid MAC and panic.

Statime has an `identity` config key for manual override, but as of the inferno-dev branch, the code uses `unwrap_or` (eager evaluation) instead of `unwrap_or_else` (lazy), so `get_clock_id()` panics even when `identity` is set. This is an upstream bug.

### Why TMPDIR must be on a shared volume

The usrvclock protocol uses Unix datagram sockets. The client (bridge) creates a socket at `$TMPDIR/usrvclock-client.{PID}.{N}`. The server (Statime) discovers this path via `recvfrom()` and sends overlays back to it via `sendto()`.

Docker containers have isolated filesystems even with host networking (`network_mode: host` only shares the network namespace, not the mount namespace). If TMPDIR is container-local `/tmp`, Statime cannot reach the bridge's client socket. The shared volume makes these paths visible across containers.

Each bridge needs a unique TMPDIR subdirectory because container main processes are typically PID 1, which would cause socket name collisions.

### DANTE device naming

Inferno uses two hostnames:

- **`friendly_hostname`**: What shows in Dante Controller. Comes from the `NAME` config key (our `--name` arg). If not set, defaults to `"{app_name} {hex_ip}"` (e.g., "SSBridge ac150004").
- **`factory_hostname`**: Used for mDNS: `"{short_name}-{hex_device_id}"`.

The device ID is auto-derived: `0000<IP_bytes><PROCESS_ID_bytes>`. With host networking, all bridges share the same IP, so `PROCESS_ID` is what makes each device unique.

### Sendspin protocol reality

- **sendspin-rs v0.1** only has a PCM decoder ("Phase 1: PCM only"). FLAC is planned but not implemented.
- `AudioChunk` delivers **raw bytes** (`data: Arc<[u8]>`), not decoded samples. For PCM, we parse the bytes directly. For FLAC, we'd need our own decoder.
- The Sendspin protocol spec mentions a `codec_header` field in StreamStart for codecs like FLAC, but the framing of FLAC data within audio chunks is not well-specified.
- `sendspin serve` (the Python CLI) sends PCM when given a local WAV file. The codec is server-decided; the client cannot request a specific codec.
- The `split()` method on `ProtocolClient` returns a tuple `(messages, audio, clock_sync, sender, guard)`, not a struct with named fields.

### ExternalBuffer unconditional_read()

This is critical to understanding the bridge's clock model. When `unconditional_read()` returns `true` (which it does for `ExternalBuffer`), inferno's `RBOutput::read_at()` reads from any ring buffer position without checking `readable_pos` or `writing_pos`. It trusts that the external writer (our bridge) has placed valid data there.

This means inferno reads at `(ptp_time - start_time - latency) % RING_BUFFER_SIZE` regardless of what we've written. Our job is simply to keep writing ahead of the read position.

## Future Work

- **FLAC support**: When sendspin-rs gains FLAC decoding
- **Sendspin clock_sync**: Monitor for drift detection
- **Drift compensation**: Sample insertion/dropping when fill deviates
- **Prometheus metrics**: Production monitoring endpoint

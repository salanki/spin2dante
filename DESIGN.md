# spin2dante — Design Document

## Context

This bridge streams audio from Sendspin sources (e.g., Music Assistant) to DANTE receivers without going through the ALSA subsystem. It's a direct protocol-to-protocol bridge: receive audio via Sendspin's WebSocket protocol, write it into inferno_aoip's transmit ring buffers, and let the DANTE TX engine send it on the network. The result is a completely userspace, bit-perfect (for PCM) audio bridge.

## Architecture

```
Sendspin Server (Music Assistant)
        │ WebSocket (PCM audio chunks)
        ▼
┌─────────────────────┐
│   sendspin_bridge    │  ← this crate (separate repo)
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

### Startup sequence

1. `DeviceServer::start()` — creates DANTE device, waits for PTP clock
2. `transmit_from_external_buffer()` — registers ring buffers
3. `start_tx.send(0)` — starts FlowsTransmitter immediately (no blocking)
4. Bridge enters Prebuffering state, writes audio at positions 0, 1, 2...
5. After `prebuffer_target` samples written, transitions to Running

The FlowsTransmitter may report "clock unavailable" until it receives its first PTP overlay from the background clock thread. This is non-blocking — the bridge continues writing audio regardless. Once the clock becomes available, inferno starts reading and transmitting.

## State Machine

```
        StreamStart
  Idle ───────────→ Prebuffering ──→ Running
                         ↑               │
                         │  StreamStart   │
                         │  StreamClear   │
                         └──── Rebuffering←┘
                                   │
                         StreamEnd │
                              Idle ←┘
```

- **Idle**: No stream, DANTE device may not exist
- **Prebuffering**: Writing audio, accumulating jitter buffer
- **Running**: Actively transmitting, metrics logged
- **Rebuffering**: Stream cleared/restarted, stale audio discarded, refilling

### Stream lifecycle handling

- **StreamStart (first)**: Start DANTE device, enter Prebuffering
- **StreamStart (same format)**: Clear stale audio, enter Rebuffering (no device restart)
- **StreamStart (different format)**: Stop device, restart with new format
- **StreamClear**: Zero-fill from current read_pos, jump write_pos ahead, enter Rebuffering
- **StreamEnd**: Stop transmitter, enter Idle

### Clear/Rebuffer behavior

On stream/clear, the bridge must discard stale audio that inferno is about to read:

1. Read `read_pos` from `PositionReportDestination`
2. Zero-fill ring buffer positions `[read_pos, read_pos + prebuffer_target)`
3. Set `write_pos = read_pos + prebuffer_target`
4. Enter Rebuffering state

This ensures inferno reads silence immediately (not stale pre-seek audio), then the bridge refills the jitter buffer with fresh data.

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

The bridge has an outer reconnect loop. If the WebSocket drops (server restart, network issue), it stops the current session, waits 2 seconds, and reconnects. The DANTE device is stopped and recreated on reconnect since the stream state resets.

## Codec Support

| Codec | Status | Notes |
|-------|--------|-------|
| PCM   | Supported, bit-perfect | 16-bit and 24-bit LE |
| FLAC  | Not yet supported | sendspin-rs v0.1 only has PCM decoder |
| Opus  | Not supported | Lossy codec |
| MP3   | Not supported | Lossy codec |

## Multi-Stream Deployment

One bridge process per Sendspin stream. For 32 streams: 32 containers, each with unique `--name` and `INFERNO_DEVICE_ID`.

## Configuration

### CLI Arguments
- `--url` / `-u`: Sendspin server WebSocket URL (required)
- `--name` / `-n`: DANTE device name (default: "Sendspin Bridge")
- `--buffer-ms`: Jitter buffer size in ms (default: 50)

### Environment Variables (passed through to inferno_aoip)
- `INFERNO_BIND_IP`, `INFERNO_DEVICE_ID`, `INFERNO_SAMPLE_RATE`
- `INFERNO_CLOCK_PATH`, `INFERNO_TX_LATENCY_NS`

## Future Work

- **FLAC support**: When sendspin-rs gains FLAC decoding
- **Sendspin clock_sync**: Monitor for drift detection
- **Drift compensation**: Sample insertion/dropping when fill deviates
- **Prometheus metrics**: Production monitoring endpoint

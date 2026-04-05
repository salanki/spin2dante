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

### Startup sequence

1. `DeviceServer::start()` — creates DANTE device, waits for PTP clock
2. `transmit_from_external_buffer()` — registers ring buffers
3. `start_tx.send(0)` — starts FlowsTransmitter immediately (no blocking)
4. Bridge enters **WaitingForSubscriber** state
5. Audio is written to the ring buffer as a circular scratch buffer
6. When a DANTE subscriber connects (read_pos starts advancing), bridge snaps to live and enters Prebuffering
7. After `prebuffer_target` samples of fresh audio written, transitions to Running

The FlowsTransmitter may report "clock unavailable" until it receives its first PTP overlay from the background clock thread. This is non-blocking — the bridge continues writing audio regardless. Once the clock becomes available, inferno starts reading and transmitting.

### Why WaitingForSubscriber exists

Without this state, the bridge would write audio into the ring buffer starting at position 0 while inferno reads at `(ptp_now - latency) % RING_BUFFER_SIZE`. These are in completely different domains. If a DANTE subscriber connects later, it reads whatever happens to be at that ring position — which is stale audio from seconds ago, not the live stream. Multiple subscribers connecting at different times would hear different offsets.

The fix: the bridge writes to the ring buffer as scratch (keeping the WebSocket alive) but doesn't commit to a live position until a subscriber actually appears. When read_pos starts advancing, the bridge calls `snap_to_live()`:

1. Read the current `read_pos` from `PositionReportDestination`
2. Zero-fill `[read_pos, read_pos + prebuffer_target)` — silence during prebuffer
3. Set `write_pos = read_pos + prebuffer_target` — fresh audio lands right after
4. Enter Prebuffering — accumulate jitter buffer with live audio

This ensures every subscriber hears current audio from the moment it connects.

## State Machine

```
        StreamStart
  Idle ───────────→ WaitingForSubscriber ──→ Prebuffering ──→ Running
                           ↑                      ↑               │
                           │                      │  StreamStart   │
                           │ subscriber lost       │  StreamClear   │
                           │                      └──── Rebuffering←┘
                           │                                │
                           │                      StreamEnd │
                           └──────────────────────── Idle ←─┘
```

- **Idle**: No stream, DANTE device may not exist
- **WaitingForSubscriber**: Receiving audio into scratch buffer, DANTE device registered but no subscriber consuming. Audio is not committed to a live position.
- **Prebuffering**: Subscriber detected, writing fresh audio to fill jitter buffer
- **Running**: Actively transmitting, metrics logged
- **Rebuffering**: Stream cleared/restarted, stale audio discarded, refilling

### Stream lifecycle handling

- **StreamStart (first)**: Start DANTE device, enter WaitingForSubscriber
- **StreamStart (same format)**: Clear stale audio, enter Rebuffering (no device restart)
- **StreamStart (different format)**: Stop device, restart with new format
- **StreamClear**: Zero-fill from current read_pos, jump write_pos ahead, enter Rebuffering
- **StreamEnd**: Stop transmitter, enter Idle

### Subscriber detection

The bridge monitors `read_pos` from `PositionReportDestination`. When `read_pos` changes from its initial value, a subscriber has connected and inferno is consuming audio. This triggers `snap_to_live()`.

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

Statime as a PTP master with no followers has nothing to synchronize against. Its overlay export callback only fires on clock adjustments, which don't happen for a masterless PTP node. Result: the usrvclock socket is created but no overlays are ever sent, so inferno's FlowsTransmitter permanently reports "clock unavailable."

The fake_usrvclock_server (used in `make test`) works because it sends periodic overlays unconditionally (every 1 second via `select()` timeout), regardless of PTP state.

For production, you need real DANTE hardware on the network acting as PTP master.

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

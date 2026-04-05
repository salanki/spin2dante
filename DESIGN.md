# spin2dante — Design Document

## Context

This bridge streams audio from Sendspin sources (e.g., Music Assistant) to DANTE receivers without going through the host's audio subsystem. It's a direct protocol-to-protocol bridge: receive audio via Sendspin's WebSocket protocol, write it into inferno_aoip's transmit ring buffers, and let the DANTE TX engine send it on the network. The result is a completely userspace, bit-perfect (for PCM) audio bridge.

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
│  4. Write via RBInput│
└────────┬────────────┘
         │ Owned ring buffers (RBInput/RBOutput)
         ▼
┌─────────────────────┐
│   inferno_aoip       │  ← fork with transmit_from_owned_buffer()
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

The bridge uses a fork of inferno_aoip that adds `transmit_from_owned_buffer()` — a method that creates owned ring buffers with proper `readable_pos` tracking and returns `RBInput` write handles to the caller.

## Clock Model

### Overview

The bridge sends `start_time = 0` to inferno immediately. This means:

```
timestamp_shift = -0 - latency = -latency
TX read position = next_ts + timestamp_shift = next_ts - latency
```

The FlowsTransmitter reads from the ring buffer at PTP-domain positions. The bridge writes at monotonically increasing positions starting from 0. With owned buffers, inferno only reads data that has been marked as readable via `write_from_at()`.

### Current limitation: write/read domain alignment

The bridge writes at positions in its own domain (0, 1, 2, ...) while the FlowsTransmitter reads at PTP-domain positions (`next_ts - latency`). These domains don't align:

- Our `write_pos` starts at 0 and increments monotonically
- Inferno's `start_ts` (read position) is `next_ts + timestamp_shift`, where `next_ts` is a PTP timestamp

With owned buffers (`unconditional_read = false`), inferno checks `readable_pos` before reading. If the transmitter's `start_ts` is outside the range we've written, it reads zeros. The inferno fork now exposes that true consumer read position so the bridge can align writes to the actual PTP-domain read cursor instead of guessing from an approximation.

### Read position tracking

The inferno fork exposes the actual TX-side consumer cursor by publishing `start_ts` from the FlowsTransmitter. The bridge uses that true read position for:

- `snap_to_live()`: aligning `write_pos` to where inferno will actually read next
- `WaitingForSubscriber`: detecting when the FlowsTransmitter has started consuming
- Buffer fill estimation against the real consumer cursor

## Device Lifetime

The DANTE device (DeviceServer + TX) is started **once at process startup** and stays alive for the entire process lifetime. This matches standard DANTE behavior where devices are persistent network entities.

### Startup sequence

1. `DeviceServer::start()` — creates DANTE device, blocks until PTP clock available
2. `transmit_from_owned_buffer()` — creates owned ring buffers, returns RBInput handles
3. `start_tx.send(0)` — starts FlowsTransmitter (idle, transmitting silence/zeros)
4. Bridge enters **Idle** state — device visible on network
5. Connect to Sendspin server (retry loop)
6. On StreamStart → enter **WaitingForSubscriber**

### WaitingForSubscriber and timeout fallback

When a stream starts, the bridge writes audio via `RBInput::write_from_at()`. It monitors `current_timestamp` to detect when the FlowsTransmitter is operating:

- If `current_timestamp` changes from `usize::MAX` → snap_to_live()
- If no change within 5 seconds → fall back to Prebuffering without alignment

The timeout fallback is pragmatic for environments where the PTP clock takes time to propagate to the FlowsTransmitter.

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

- **Idle**: Ring filled with silence. No stale audio can leak.
- **WaitingForSubscriber**: Discardable scratch audio in ring, waiting for TX to start.
- **Prebuffering**: Fresh audio accumulating after snap_to_live (or timeout fallback).
- **Running**: Live audio being written and transmitted.
- **Rebuffering**: Zero-fill + fresh audio after seek/clear.

### Stream lifecycle handling

- **StreamStart**: Enter WaitingForSubscriber (or snap_to_live if TX already active)
- **StreamStart (same format, already Running)**: Clear stale audio, enter Rebuffering
- **StreamClear**: Zero-fill from approximate read_pos, jump write_pos ahead, enter Rebuffering
- **StreamEnd**: Fill ring with silence, reset stream state, enter Idle (device stays on network)
- **Sendspin disconnect**: Same as StreamEnd — silence ring, enter Idle, reconnect

## Data Path

1. Sendspin delivers `AudioChunk { data: Arc<[u8]> }` — raw PCM bytes over WebSocket
2. Bridge parses bytes, deinterleaves (L/R), and shifts samples to inferno format
3. Bridge writes per-channel via `RBInput::write_from_at(write_pos, samples_iter)`
4. FlowsTransmitter reads via `RBOutput::read_at()` at PTP-synchronized timestamps

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

The bridge approximates buffer fill from `write_pos - current_timestamp`. When `current_timestamp` is valid (not `usize::MAX`), metrics show:

```
[buffer] fill=2412 target=2400 drift=+1.2ppm write_pos=... read_pos=...
```

When `current_timestamp` is not yet available:
```
[buffer] writing at N samples (read_pos not yet available)
```

**Note**: Fill and drift are approximate because `current_timestamp` is `min_next_ts`, not the actual ring read position. True buffer occupancy requires exposing the consumer cursor from the FlowsTransmitter (planned inferno fork enhancement).

## PTP Clock Chain

```
DANTE devices ←PTP→ Statime (PTPv2 follower)
                        │
                        │ usrvclock (Unix datagram socket)
                        ▼
                  inferno AsyncClient (tokio task)
                        │
                        │ watch channel → RealTimeBoxReceiver
                        ▼
                  FlowsTransmitter (real-time thread)
```

**Only PTP followers export usrvclock overlays.** A PTP master doesn't adjust its clock, so the overlay export callback never fires. For Docker-only testing, use a PTPv2 master + follower pair. For production with DANTE hardware, Statime runs as PTPv1 follower syncing to the DANTE PTP master.

## Format Enforcement

The bridge rejects streams at StreamStart that don't match:
- Sample rate must be 48000 Hz
- Channel count must be 2 (stereo)
- Codec must be "pcm"
- Bit depth must be 16 or 24

## Reconnection

The bridge has an outer reconnect loop. If the WebSocket drops, it fills the ring with silence, enters Idle, waits 2 seconds, and reconnects. The DANTE device stays on the network — only the stream state resets.

## Codec Support

| Codec | Status | Notes |
|-------|--------|-------|
| PCM   | Supported, bit-perfect | 16-bit and 24-bit LE |
| FLAC  | Not yet supported | sendspin-rs v0.1 only has PCM decoder |
| Opus  | Not supported | Lossy codec |
| MP3   | Not supported | Lossy codec |

## Multi-Stream Deployment

One bridge process per Sendspin stream. Each bridge needs unique `INFERNO_PROCESS_ID` and `INFERNO_ALT_PORT`. Device ID is auto-derived from host IP + process ID.

## Inferno Fork

This project uses a fork of inferno_aoip that adds:

- `transmit_from_owned_buffer()` — creates owned ring buffers and returns `RBInput` write handles
- Re-exports: `OwnedBuffer`, `RBInput`, `RBOutput`, `new_owned_ring_buffer`

The owned buffer path provides:
- `readable_pos` tracking on the write side (inferno only reads validated data)
- `unconditional_read() == false` (reads check readable_pos)
- Hole detection and fill via `hole_fix_wait`
- Configurable TX dithering, with spin2dante explicitly disabling the 24-bit path so PCM payloads can be validated bit-for-bit over the received overlap

The fork exposes the consumer-side read position, and the bridge uses that real cursor instead of approximating from `current_timestamp`.

## Lessons Learned

### Why "start at zero" instead of PTP-anchored start_time

Getting a PTP media clock time to send as `start_time` proved unreliable:
1. `get_realtime_clock_receiver()` uses a `RealTimeBoxReceiver` that requires the sender to keep sending for the receiver to see updates
2. `make_shared_media_clock()` returns immediately without waiting for the first overlay
3. `current_timestamp` is only set after `start_time` is received (chicken-and-egg)

Sending `start_time = 0` avoids all three issues. The tradeoff is that ring buffer positions are in our local domain (0, 1, 2...) rather than the PTP domain.

### TMPDIR must be on a shared volume

The usrvclock protocol uses Unix datagram sockets. Client sockets are created in `$TMPDIR`. Docker containers have isolated filesystems even with host networking, so the socket files must be on a shared volume for Statime to reach them. Each bridge needs a unique TMPDIR subdirectory (PIDs overlap across containers).

### DANTE device naming

Inferno uses `friendly_hostname` from the `NAME` config key (our `--name` arg). Without it, the default is `"{app_name} {hex_ip}"`. The device ID is auto-derived from `IP + PROCESS_ID`.

## Future Work

- **True consumer position**: Expose `start_ts` or `timestamp_shift` from FlowsTransmitter so the bridge can write at PTP-domain positions
- **FLAC support**: When sendspin-rs gains FLAC decoding
- **Drift compensation**: Sample insertion/dropping when fill deviates from target
- **Prometheus metrics**: Production monitoring endpoint

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

The bridge uses a fork of inferno_aoip (pinned to commit `5b1c9d1`) that adds `transmit_from_owned_buffer()` and `ReadPositionSnapshot`.

## Two-Stage Queue

Audio flows through two stages before reaching the DANTE network:

1. **Pending queue** (`VecDeque<PendingChunk>`): Holds decoded PCM chunks keyed by server timestamp. Absorbs Sendspin's ahead-of-time buffering. Bounded by `MAX_PENDING_CHUNKS` (200).

2. **Dante ring buffer** (`RBInput`, 16384 samples / ~341ms): Final local playout queue. FlowsTransmitter reads from here at PTP-synchronized timestamps.

The pending queue decouples chunk arrival from ring placement. Chunks are drained to the ring when their server-time target falls within the ring's writable horizon.

### Buffer capacity

The bridge advertises a small `buffer_capacity` (~500ms of stereo 24-bit PCM) via the Sendspin `PlayerV1Support` handshake, so the server doesn't send audio too far ahead of real-time.

## Cross-Bridge Sync Architecture

### Goal

Multiple bridges connected to the same Sendspin server, sharing the same PTP clock, should place the same audio chunk at the same ring position. Target: < 1ms (48 samples) cross-bridge spread. Achieved: **< 0.5ms** (1-16 samples).

### How it works

Each bridge establishes a **stable anchor** mapping Sendspin server time to a ring position:

```
anchor_server_us = server_time_at_snapshot
anchor_ring_pos  = read_pos_at_snapshot + prebuffer_target
```

All subsequent chunk targets are computed relative to this anchor:

```
target = anchor_ring_pos + (chunk.timestamp - anchor_server_us) * SAMPLE_RATE / 1_000_000
```

This gives stable chunk-to-chunk spacing (unaffected by wall-clock jitter) and cross-bridge consistency (all bridges using the same anchor mapping place the same chunk at the same position).

### ReadPositionSnapshot (the key to sub-millisecond sync)

The critical insight: sampling `read_pos` and `server_now_us()` separately introduces a timing gap that causes cross-bridge anchor offset. With separate sampling, bridges that anchor at different wall-clock times get different mappings.

The inferno fork provides a `ReadPositionSnapshot` — a seqlock-protected `(read_position, monotonic_nanos)` pair written by the TX thread at the exact moment it updates `read_position`. The bridge reads this consistent pair and converts the monotonic timestamp to server time via ClockSync:

```
(snap_read_pos, snap_instant) = snapshot  // consistent pair from TX thread
snap_server_us = ClockSync(snap_instant)  // convert to Sendspin server time
anchor = (snap_server_us, snap_read_pos + prebuffer)
```

Since PTP time and server time both advance at 48kHz, the dt cancels:
```
Bridge A at time T:  anchor = (S, R + prebuffer)
Bridge B at time T+dt: anchor = (S + dt*1M, R + dt*48000 + prebuffer)

For chunk C:
  target_A = R + prebuffer + (C - S) * 48/1000
  target_B = R + dt*48000 + prebuffer + (C - S - dt*1M) * 48/1000
           = R + dt*48000 + prebuffer + (C-S)*48/1000 - dt*48000
           = target_A  ✓
```

The sync_key metric (`ring_pos - server_us * rate / 1M`) confirms this — it differs by only 1-16 samples across bridges.

### Chunk eligibility decisions

- `target + frames ≤ read_pos` → drop (entirely consumed)
- `target < read_pos < target + frames` → trim stale prefix, write remainder
- `target far ahead of write frontier` → scheduler activation (first chunk) or discontinuity (settled)
- `target behind write_pos by more than one chunk` → rebuffer (broken scheduler state)
- Otherwise → write at target

### Sequential fallback

Before the PTP clock is available (`read_pos = 0`), the bridge writes chunks sequentially at `write_pos`. Once `read_pos` becomes valid and ClockSync converges, the anchor is established and timestamp-driven positioning activates.

## PTP Clock Model

The bridge sends `start_time = 0` to inferno. FlowsTransmitter reads from ring positions in the PTP domain. The bridge detects the domain mismatch (write_pos near 0 vs read_pos near ~140 billion) and calls `snap_to_live()` to realign.

### Read position tracking

The inferno fork exposes `read_position` (the actual `start_ts` from FlowsTransmitter) and `ReadPositionSnapshot` for:
- `snap_to_live()`: aligning `write_pos` to where inferno will actually read next
- Anchor creation: consistent `(read_pos, time)` pair for cross-bridge sync
- Buffer fill estimation against the real consumer cursor

## Device Lifetime

The DANTE device (DeviceServer + TX) starts once at process startup and stays alive for the entire process lifetime. The device is visible on the DANTE network regardless of stream state.

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

- **Idle**: Ring filled with silence. No stale audio can leak.
- **WaitingForSubscriber**: Waiting for DANTE subscriber (5s timeout to Prebuffering).
- **Prebuffering**: Fresh audio accumulating after snap_to_live.
- **Running**: Live audio being written and transmitted.
- **Rebuffering**: Zero-fill + fresh audio after seek/clear.

### Stream lifecycle handling

- **StreamStart**: Enter WaitingForSubscriber (or snap_to_live if TX already active)
- **StreamStart (same format, already Running)**: Clear stale audio, enter Rebuffering
- **StreamClear**: Zero-fill, enter Rebuffering
- **StreamEnd**: Fill ring with silence, enter Idle (device stays on network)
- **Sendspin disconnect**: Silence ring, enter Idle, reconnect after 2s

## Data Path

1. Sendspin delivers `AudioChunk { data: Arc<[u8]> }` — raw PCM bytes over WebSocket
2. Bridge decodes, deinterleaves (L/R), shifts to inferno format → `PendingChunk`
3. `drain_pending()` computes target from anchor, writes via `RBInput::write_from_at()`
4. FlowsTransmitter reads via `RBOutput::read_at()` at PTP-synchronized timestamps

## Sample Format Alignment

- **Sendspin PCM 24-bit**: 3 bytes LE signed → sign-extend to i32 → shift left 8
- **Sendspin PCM 16-bit**: 2 bytes LE signed → cast to i32 → shift left 16
- **Inferno `Sample`**: i32 with 24-bit value in upper 24 bits

The bridge currently advertises and accepts PCM `16-bit` and `24-bit` Sendspin streams. Both are transported losslessly through Inferno's `Sample` representation.

`TX_SOURCE_BIT_DEPTH` is intentionally fixed to `24`. This is not a statement that the bridge only supports 24-bit source audio; it reflects Inferno's 24-bit-oriented TX sample path and keeps TX dithering disabled for bit-perfect PCM transport.

This is an implementation choice, not a fundamental architectural limit. Supporting wider PCM formats in the future would require explicit protocol, decode, and TX-path validation, but the bridge design itself is not inherently restricted to only `16-bit` and `24-bit` PCM.

## Player Capabilities

The bridge advertises itself as a Sendspin player with no volume or mute support (`supported_commands: []`). It is a transparent passthrough — audio is delivered to DANTE exactly as the server sends it, with no gain processing. Volume control is expected to happen upstream (in Music Assistant) or downstream (on the DANTE receiver/amplifier).

## Multi-Stream Deployment

One bridge process per Sendspin stream. Each bridge needs unique `INFERNO_PROCESS_ID` and `INFERNO_ALT_PORT` (or unique `INFERNO_DEVICE_ID` in Docker bridge networks).

## Inferno Fork

[`github.com/salanki/inferno`](https://github.com/salanki/inferno/tree/spin2dante-owned-buffer), pinned to commit `5b1c9d1`:

- `transmit_from_owned_buffer()` — creates owned ring buffers, returns `RBInput` handles
- `ReadPositionSnapshot` — seqlock `(read_pos, monotonic_nanos, ref_instant)` for precise timing
- `read_position: Arc<AtomicUsize>` — exposes TX consumer cursor
- `TX_SOURCE_BIT_DEPTH` — controls dithering (set to 24 for bit-perfect PCM)

## Lessons Learned

### Why the two-stage queue, not direct write

Per-chunk live targeting (`target = read_pos + prebuffer + delta_from_server_now`) was attempted first. It caused chunk overlap because `server_now` advances between chunks within a drain cycle. The stable anchor approach — set once, compute all targets relative to it — preserves chunk spacing.

### Why ReadPositionSnapshot for sync

Sampling `read_pos` and `server_now_us()` separately introduces a timing gap (microseconds to milliseconds). This gap differs per bridge, causing 30-50ms of cross-bridge anchor offset. The seqlock snapshot from the TX thread eliminates this gap, reducing offset to 1-16 samples (< 0.5ms).

### TMPDIR must be on a shared volume

The usrvclock protocol uses Unix datagram sockets in `$TMPDIR`. Docker containers need these on a shared volume for Statime to reach them.

## Future Work

- **FLAC support**: When sendspin-rs gains FLAC decoding
- **Drift compensation**: Sample insertion/dropping if fill deviates from target over long sessions
- **Prometheus metrics**: Production monitoring endpoint
- **Ring buffer sizing**: Currently 16384 samples (~341ms). Could be further tuned based on production latency requirements.

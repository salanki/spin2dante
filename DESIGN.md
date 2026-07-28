# spin2dante — Design Document

## Context

This bridge streams audio from Sendspin sources (e.g., Music Assistant) to DANTE receivers without going through the host's audio subsystem. It's a direct protocol-to-protocol bridge: receive audio via Sendspin's WebSocket protocol, write it into inferno_aoip's transmit ring buffers, and let the DANTE TX engine send it on the network. PCM is bit-exact except for logged single-frame repeats or drops applied under measured Sendspin/PTP clock drift.

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

1. **Pending queue** (`VecDeque<PendingChunk>`): Holds decoded PCM chunks keyed by server timestamp. Absorbs Sendspin's ahead-of-time buffering. Bounded by duration via `max_pending_frames` (derived from `server_buffer_ms`), with `MAX_PENDING_CHUNKS` as an absolute chunk-count backstop.

2. **Dante ring buffer** (`RBInput`): Final local playout queue. FlowsTransmitter reads from here at PTP-synchronized timestamps. Its size is derived per-bridge from `buffer_ms` + `server_buffer_ms` (≈ 2× the sum of the prebuffer and the send-ahead lead, rounded up to a power of 2, floored at 16384 samples / ~341ms) so the largest healthy write/read distance fits within one ring — see [Buffer capacity](#buffer-capacity).

The pending queue decouples chunk arrival from ring placement. Chunks are drained to the ring when their server-time target falls within the ring's writable horizon.

### Buffer capacity

The bridge advertises a `buffer_capacity` via the Sendspin `PlayerV1Support` handshake. This is the server-side **send-ahead credit** — the amount of audio the Sendspin server (e.g. Music Assistant) may queue ahead of the playout deadline before it throttles. It is **not** a prebuffer or start delay (that is `buffer_ms`): playout still begins at the chunk's scheduled playout time regardless of the credit.

The credit is configurable via `server_buffer_ms` (default **500 ms**; previously a fixed ~200 ms in code, though this section historically said ~500 ms). A larger credit lets the server run further ahead and absorb its own event-loop / writer stalls before it has to drop chunks (which it logs as `Late binary … skipping`). The advertised byte count is computed at 24-bit depth (the larger of the two depths offered), so a negotiated 16-bit stream gets proportionally *more* headroom, never less.

**Trade-off with control latency (why the default is 500 ms, not larger).** `server_buffer_ms` also bounds how quickly volume/mute changes take effect: Music Assistant delivers control commands over the same Sendspin connection, *behind* the buffered audio, so the worst-case control lag is roughly `server_buffer_ms` (observed on a 21-zone deployment: 2000 ms → ~2.2 s mute lag; 500 ms → effectively instant, with zero late-binary skips at either value — the deepest send-ahead seen in practice was ~360 ms). The gain itself is applied bridge-side at drain (after the pending queue), so the buffer never pre-attenuates audio — the lag is purely the control message waiting behind queued audio in MA. 500 ms keeps controls snappy while still giving ~2.5× the old 200 ms of stall headroom. Raise it for more skip tolerance at the cost of slower volume response; lower it for the opposite.

Because the scheduler writes a chunk to the ring only once its playout time is within one ring horizon, **and** the write/read realignment guard re-snaps whenever their distance exceeds the ring, the server's send-ahead lead must fit comfortably inside one ring — otherwise the front chunk is perpetually "too early," the ring underruns, and the bridge enters a snap/anchor thrash loop (which manifests as a fixed inter-bridge offset, breaking cross-bridge sync). The ring is therefore **sized from `buffer_ms` + `server_buffer_ms`** (≈ `2 × (buffer_ms + server_buffer_ms)`, rounded up to a power of 2, floored at ~341 ms — the anchor sits at `read + prebuffer` and chunks are placed up to the lead beyond that, so the ring must hold prebuffer + lead) rather than the credit being capped to a fixed ring. The lead lives across both the pending queue (bounded by `max_pending_frames` ≈ `2 × server_buffer_ms` + one ring) and the ring; neither can overflow, and the realignment guard never fires on a healthy lead. Ring memory scales with the credit (~0.5 MB/bridge at the 500 ms default; e.g. ~2 MB/bridge at 2000 ms), with no effect on latency — playout remains timestamp-driven.

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

The initial sync_key metric (`ring_pos - server_us * rate / 1M`) confirms this
anchor — it differs by only 1-16 samples across bridges. Drift corrections
subsequently move each bridge's anchor independently. Bridges sharing the same
PTP and Sendspin clocks should accrue corrections at the same average rate, but
their instantaneous schedules can differ by a few frames. That is tens of
microseconds at 48kHz and remains within the sub-millisecond sync budget; the
initial sync_key is no longer an exact invariant after correction begins.

### Chunk eligibility decisions

- `target + frames ≤ read_pos` → drop (entirely consumed)
- `target < read_pos < target + frames` → trim stale prefix, write remainder
- `target far ahead of write frontier` → scheduler activation (first chunk) or discontinuity (settled)
- `target behind write_pos by more than one chunk` → rebuffer (broken scheduler state)
- Otherwise → apply any scheduled one-frame repeat/drop, write at target, and
  advance the anchor by the actual correction so the next target is contiguous

### Sequential fallback

Before the PTP clock is available (`read_pos = 0`), the bridge writes chunks sequentially at `write_pos`. Once `read_pos` becomes valid and ClockSync converges, the anchor is established and timestamp-driven positioning activates.

## Clock Drift Correction

Once the anchor has settled, the bridge periodically compares the actual DANTE
read position with the position predicted from the Sendspin server timeline.
A three-sample median filters ordinary measurement jitter; a ring-scale raw
excursion bypasses that filter and immediately re-buffers for safety.

Ordinary error is passed to sendspin-rs `CorrectionPlanner`, which supplies the
deadband, hysteresis, correction direction/cadence, and reanchor decision. The
CLI correction budget is converted to a minimum frame interval and clamps the
planner cadence. `CorrectionState` carries that schedule across pending chunks:

- slow source timeline → repeat the preceding complete stereo frame;
- fast source timeline → drop one complete stereo frame;
- planner reanchor or ring-scale error → clear and rebuffer.

Each applied correction changes `anchor_ring_pos` by the same signed one-frame
amount during the current drain pass. This keeps later queued targets
contiguous. Cumulative inserted and dropped frame counts are emitted in the
periodic `[sync]` metric and are reconciled with capture analysis in the
deterministic drift test. The same record includes bridge attribution, a local
stream sequence, the first Sendspin timestamp, and the latest filtered/raw
read-position error against the shared Sendspin/PTP timeline. Pairwise
subtraction of `timeline_offset_frames` for equal `stream_start_us` values is
the operational inter-bridge skew measurement; lifetime correction-counter
differences are not used as a substitute.

The resulting stream is bit-perfect modulo these declared one-frame timing
events. At 48kHz each event is 20.8 microseconds; corrections are distributed
over time rather than emitted as a multi-frame zero gap.

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
3. `drain_pending()` computes target from anchor, applies any planned
   frame-level timing correction, and writes via `RBInput::write_from_at()`
4. FlowsTransmitter reads via `RBOutput::read_at()` at PTP-synchronized timestamps

## Sample Format Alignment

- **Sendspin PCM 24-bit**: 3 bytes LE signed → sign-extend to i32 → shift left 8
- **Sendspin PCM 16-bit**: 2 bytes LE signed → cast to i32 → shift left 16
- **Inferno `Sample`**: i32 with 24-bit value in upper 24 bits

The bridge currently advertises and accepts PCM `16-bit` and `24-bit` Sendspin streams. Both decode losslessly into Inferno's `Sample` representation; logged frame repeats/drops may still occur when clock-drift correction is active.

`TX_SOURCE_BIT_DEPTH` is intentionally fixed to `24`. This is not a statement that the bridge only supports 24-bit source audio; it reflects Inferno's 24-bit-oriented TX sample path and keeps TX dithering disabled for bit-perfect PCM transport.

This is an implementation choice, not a fundamental architectural limit. Supporting wider PCM formats in the future would require explicit protocol, decode, and TX-path validation, but the bridge design itself is not inherently restricted to only `16-bit` and `24-bit` PCM.

## Player Capabilities

By default, the bridge advertises `supported_commands: []` and is a transparent passthrough — audio is delivered to DANTE exactly as the server sends it, with no gain processing.

When `--volume-control=bridge` is enabled, the bridge advertises `supported_commands: ["volume", "mute"]` and applies software gain to the decoded PCM stream before writing to the DANTE ring buffer.

### Bridge-Side Gain Architecture

```
decode_pcm() → PendingChunk queue → BridgeGainRamp.apply() → RBInput ring buffer → DANTE TX
                                    ^^^^^^^^^^^^^^^^^^^^
                                    gain stage (when enabled)
```

Components:
- **`GainControl`** (from sendspin crate): Thread-safe atomic state for volume (0–100) and mute. The bridge uses it only for the raw volume/mute state (command plumbing, persistence, player-state reporting) — its `gain()` accessor (a `(volume/100)^1.5` curve) is deliberately not used.
- **`volume_to_gain`** (in `src/gain.rs`): Maps volume/mute to the ramp's gain target with a linear-in-dB taper (`dB = 40 × (vol/100 − 1)`, 0.4 dB per 1% step, −40 dB anchor), exact unity at 100%, and a linear fade to true zero below the 10% knee. Chosen over sendspin's `^1.5` curve so loudness changes evenly across the whole 0–100 range instead of compressing into 0–50%.
- **`BridgeGainRamp`** (in `src/gain.rs`): Per-frame gain ramping (20ms at 48kHz = 960 frames) adapted for per-channel `Vec<Sample>` (i32). Prevents clicks on volume changes. Uses f64 intermediate for sample multiplication to preserve 24-bit precision.

The gain is applied after scheduling (after the anchor/drift-correction layer decides where to place each chunk) and before the ring buffer write. Drift correction operates on timing/positions, not sample values, so gain is completely orthogonal.

At 100% volume with mute off, the gain path is a true no-op: `ramp_frames_remaining == 0 && current_gain == 1.0` returns immediately without touching any samples. It adds no sample modification beyond any independently logged timing corrections.

The gain ramp state is reset on stream transitions (StreamStart, StreamEnd, StreamClear, reconnection) to avoid carrying stale ramp state into new audio.

## DANTE Subscriber State Reporting

When `--report-dante-subscriber` is enabled, the bridge reports its DANTE receiver subscription status to the Sendspin server via `ClientSyncState`. This lets Music Assistant know whether audio sent to this bridge will actually reach a speaker.

### Protocol mapping

- **No subscriber** → `ClientSyncState::ExternalSource`: MA removes the player from its group (other group members keep playing) and places it in a solo stopped group.
- **Subscriber present** → `ClientSyncState::Synchronized`: MA treats the player as operational; it can rejoin groups and receive audio.

`ExternalSource` was chosen over `Error` because MA's aiosendspin server ignores `Error` entirely (no-op), while `ExternalSource` triggers a graceful group removal without stopping other players.

### Detection mechanism

Subscriber presence is detected via `read_pos` movement from inferno's FlowsTransmitter:

- `read_pos == 0` → PTP clock not ready (not a subscriber signal)
- `read_pos` advancing (changes between samples) → at least one DANTE receiver is consuming the flow
- `read_pos` stale for >10s while nonzero → subscriber lost

`read_pos` is per-flow, not per-channel. If a receiver subscribes to only one of the two stereo channels, `read_pos` still advances — the bridge correctly treats this as "connected."

### Critical constraint: timer-based detection

When `ExternalSource` is active, MA stops sending audio to the bridge. This means `handle_audio()` won't be called. Therefore, **subscriber detection must run from the periodic metrics timer** (every 5s), not from audio handlers. The audio handler supplements the timer with faster liveness updates when audio IS flowing, but the timer is the primary detection path that enables recovery.

### State transitions

```
connect → ExternalSource (always start unconfirmed)
                │
        read_pos movement detected (timer)
                ▼
          Synchronized
                │
        read_pos stale >10s (timer)
                ▼
          ExternalSource
                │
        read_pos movement detected (timer)
                ▼
          Synchronized
```

Subscriber detection requires observed `read_pos` movement from a stored baseline — a static nonzero `read_pos` is not sufficient. This prevents state flapping when a stale position lingers after subscriber loss.

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
- **Prometheus metrics**: Production monitoring endpoint
- **Ring buffer sizing**: Derived per-bridge from `buffer_ms` + `server_buffer_ms` (≈ 2× their sum, rounded up to a power of 2, floored at 16384 samples / ~341ms). Could be further tuned based on production latency requirements.

# Changelog

## Unreleased — 2026-06-23

### Changed
- Lowered the default `server_buffer_ms` from 2000 ms to **500 ms**. `buffer_capacity` also bounds volume/mute control latency — Music Assistant delivers control commands over the same Sendspin connection, *behind* the buffered audio, so worst-case control lag ≈ `server_buffer_ms`. Validated on a live 21-zone deployment: 2000 ms produced ~2.2 s mute/volume lag while 500 ms feels instant, with **zero** late-binary skips at either value (deepest send-ahead observed ~360 ms). 500 ms balances ~2.5× the old 200 ms skip headroom against snappy controls. Raise for more skip tolerance (slower volume); lower for the opposite. (The gain is applied bridge-side at drain, after the pending queue, so the buffer never pre-attenuates audio — the lag is purely the control message queued behind audio in MA.)

> Maintainer note: set the real image sha as the version in `config.yaml` and this header on build/release.

## sha-9b05ab9 — 2026-06-22

### Added
- New `server_buffer_ms` option (global, with optional per-bridge override) that sets the Sendspin `buffer_capacity` advertised in the player handshake — the server-side send-ahead credit, i.e. how far ahead Music Assistant may queue audio before it throttles. Default **2000 ms**.

### Changed
- The advertised `buffer_capacity` default is now **2 s**, up from a fixed ~200 ms in code (the design doc had said ~500 ms). spin2dante's previous 200 ms was far thinner than every other real Sendspin client (aiosendspin test client ~694 ms, benchmark client ~1.82 s, sendspin-rs default effectively unbounded), which left it uniquely prone to MA `Late binary … skipping` drops during MA event-loop / writer stalls > 200 ms. `buffer_capacity` is a send-ahead credit, **not** a prebuffer — raising it does not delay playback start or add steady-state latency.
- The pending queue is now bounded by duration (`max_pending_frames`, derived from `server_buffer_ms`) instead of a fixed chunk count, so the larger send-ahead credit cannot overflow it regardless of chunk size. `MAX_PENDING_CHUNKS` remains only as an absolute backstop.
- **The Dante ring buffer is now sized per-bridge from `buffer_ms` + `server_buffer_ms`** (≈ 2× their sum, rounded up to a power of 2, floored at the previous 16384 samples / ~341 ms). The scheduler write horizon and the write/read realignment guard are both keyed off the ring size, so the server's send-ahead lead must fit within one ring; with the old fixed ring a 2 s credit exceeded it and put the bridge into a snap/anchor thrash loop, breaking cross-bridge sync (a fixed ~100 ms inter-bridge offset). Sizing the ring from the credit fixes this. No latency impact (playout stays timestamp-driven); ring RAM scales with the credit (~2 MB/bridge at the 2 s default).

## sha-e1383bf — 2026-06-20

### Changed
- Bridge processes now cap their Tokio runtime to 2 worker threads by default (previously sized to the host CPU count). With one process per bridge this removes heavy thread oversubscription on the audio-bridge node (e.g. 19 bridges × 8 → ~152 worker threads on 8 cores), the overload root cause behind systemic multi-zone `flows_tx` "media clock jumped" dropouts. The timing-critical DANTE TX runs on inferno's own real-time thread, not the Tokio pool, so a small pool is sufficient. Overridable via the `SPIN2DANTE_WORKER_THREADS` environment variable.

## sha-c341162 — 2026-05-17

### Fixed
- Bridge-side volume and mute state now persists across add-on restarts. Previously the bridge always started at 100% unmuted, losing any volume the user had set. State is saved to `/data/volume_state_<id>.json` and restored on startup.

## sha-a03c65e — 2026-05-11

### Added
- Home Assistant add-on option `dante_bind` to bind DANTE and mDNS traffic to a specific interface name or IPv4 address. Use `auto` to keep Inferno's default local-address selection.

## sha-cd19e61 — 2026-05-11

### Added
- Configurable DANTE transmit latency (`--dante-latency` / `dante_latency`). Supported values: 0.5, 1, 2, 5, 10, 20 ms. Default: 10ms. Lower values reduce end-to-end audio delay; the value is advertised to Dante receivers as their minimum playout buffer.

### Fixed
- Bridge now reports current volume and mute state back to Sendspin on connect and after each volume/mute change. Previously the server had no visibility into bridge-side gain state, so Music Assistant could show stale values after a reconnect.

## sha-aa6fc63 — 2026-04-27

### Added
- Optional bridge-side software volume control (`volume_control: bridge`). When enabled, the bridge advertises volume/mute support to Music Assistant and applies click-free 20ms-ramped gain to decoded PCM before writing to the DANTE ring. 100% volume is a true no-op (bit-perfect).
- Optional DANTE subscriber state reporting (`report_dante_subscriber: true`). When enabled and no DANTE receiver is subscribed, the bridge reports `ExternalSource` to Music Assistant so it gracefully removes the player from its group (other members keep playing). Once a receiver subscribes, the player can rejoin.

### Changed
- Upgraded sendspin dependency from 0.1.2 to 0.2.0 for GainControl support.

## sha-c5899ed — 2026-04-20

### Changed
- DANTE TX channels now advertise as "Left" and "Right" instead of generic "TX 1" / "TX 2". Manual renames via Dante Controller still persist and take precedence.

## sha-904a789 — 2026-04-19

### Added
- Periodic clock-drift detection and in-place anchor correction. Once per `drift_check_interval_ms` (default 1000ms), the bridge compares DANTE read position against the Sendspin server clock; when the offset exceeds `drift_threshold_ms` (default 5ms), it shifts the scheduler anchor in place rather than forcing a full rebuffer. Single-tick corrections are capped at `max_correction_samples_per_tick` (default 48 samples / 1ms at 48kHz).
- Lifetime counters in the `[sync]` metrics log line: `drift_corrections`, `rebuffers`, `drift_checks_skipped`. Use these to track how often each bridge self-corrects.
- New add-on options: `drift_threshold_ms`, `drift_check_interval_ms`, `max_correction_samples_per_tick`.
- Warning on add-on start when bridges are configured with mixed `buffer_ms` values, since `buffer_ms` is real playout latency and mismatched values prevent sample alignment between bridges.

### Changed
- `buffer_ms` documented as real playout delay, not just jitter tolerance. Bridges that should stay in sync must share the same `buffer_ms`.
- Default Sendspin WebSocket URL changed to `ws://127.0.0.1:8927/sendspin` for the common same-host Music Assistant setup.

### Fixed
- Docs no longer reference the old 50ms prebuffer default.

## 2026-04-08

### Added
- DANTE routing state now persists across add-on restarts — subscribers no longer need to be re-assigned after an update.

### Changed
- Default prebuffer reduced from 50ms to 5ms for same-host deployments.

# Changelog

## sha-cd4754b — 2026-05-11

### Added
- Home Assistant add-on option `dante_bind` to bind DANTE and mDNS traffic to a specific interface name or IPv4 address. Use `auto` to keep Inferno's default local-address selection.

## sha-cd19e61 — 2026-05-11

### Added
- Configurable DANTE transmit latency (`--dante-latency` / `dante_latency`). Supported values: 0.5, 1, 2, 5, 10, 20 ms. Default: 10ms. Lower values reduce end-to-end audio delay; the value is advertised to Dante receivers as their minimum playout buffer.

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

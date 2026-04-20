# Changelog

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

# Changelog

## sha-e1383bf — 2026-06-20

### Changed
- The PTP daemon now runs at a real-time (`SCHED_FIFO`) priority so it isn't starved by normal-priority load on the audio-bridge node. Under CPU contention a starved clock daemon lets the shared media clock drift/jump, which surfaces as inferno `flows_tx` "media clock jumped, dropout occurs" across all zones at once. The daemon is scheduled at priority 80 — deliberately just below the audio TX threads (FIFO 81) so it can never preempt audio. Falls back to normal scheduling automatically if real-time scheduling is unavailable.

### Added
- `realtime: true` in the add-on config (grants `CAP_SYS_NICE` + the `rtprio` ulimit required for real-time scheduling).
- `util-linux` in the image (provides `chrt`, which the Home Assistant base busybox lacks).

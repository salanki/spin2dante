# spin2dante

This add-on runs one or more `spin2dante` bridge processes and advertises each
configured stream as its own DANTE transmitter on your local network.

## Requirements

- A DANTE-capable receiver on the same L2 network
- A PTP clock exported to `/share/usrvclock`
- A Sendspin source URL for each bridge, for example Music Assistant's Sendspin output

## Pairing with Statime

Use the companion `statime` add-on first. The intended setup is:

1. Start the `statime` add-on first.
2. Confirm it creates `/share/usrvclock`.
3. Configure one or more bridge entries in `spin2dante`.
4. Start `spin2dante`.

## Options

- `clock_path`: Path to the exported usrvclock socket
- `wait_for_clock_seconds`: How long to wait for the clock socket before failing startup
- `log_level`: Rust log level for all bridge processes
- `drift_threshold_ms`: Drift threshold in milliseconds before the bridge applies an in-place anchor correction
- `drift_check_interval_ms`: How often, in milliseconds, to sample drift between the Sendspin and PTP timelines
- `max_correction_samples_per_tick`: Maximum anchor shift, in samples, applied in one drift-correction tick
- `bridges`: List of bridge definitions

Each bridge entry contains:
- `id`: Stable identifier used to derive a unique shared temp directory
- `name`: DANTE device name to advertise
- `url`: Sendspin WebSocket URL
- `buffer_ms`: Playout buffer / latency in milliseconds. Larger values improve jitter tolerance, but they also delay audio by that amount.
- `process_id`: Unique Inferno process ID on the host IP
- `alt_port`: Unique Inferno base UDP port, spaced at least 10 apart from other bridges

### Sendspin URL

If the Music Assistant add-on is installed on the same Home Assistant instance
and its Sendspin server is bound to all interfaces (the default),
`ws://127.0.0.1:8927/sendspin` works directly — both add-ons run with host
networking, so they share the same loopback. `*.local` hostnames do **not**
resolve from inside add-on containers, so mDNS names will not work here.

For a remote Music Assistant, use its LAN IP or its Supervisor DNS name
(e.g. `ws://<slug>.local.hass.io:8927/sendspin`).

## Example Configuration

```yaml
clock_path: /share/usrvclock
wait_for_clock_seconds: 30
log_level: info
drift_threshold_ms: 5
drift_check_interval_ms: 1000
max_correction_samples_per_tick: 48
bridges:
  - id: kitchen
    name: Kitchen
    url: ws://127.0.0.1:8927/sendspin
    buffer_ms: 5
    process_id: 1
    alt_port: 14000
  - id: livingroom
    name: Living Room
    url: ws://127.0.0.1:8927/sendspin
    buffer_ms: 5
    process_id: 2
    alt_port: 14010
```

Use a unique `process_id` and `alt_port` for every bridge. Keep `alt_port`
values at least 10 apart. If multiple bridges should stay in sync with each
other, keep `buffer_ms` the same across all of them.

Bridges that share the same Sendspin timeline and PTP clock will stay tightly
synced even at higher buffer values such as `100ms`, as long as they all use
the same `buffer_ms` setting. Increasing `buffer_ms` raises latency for the
whole sync group, but does not by itself create an offset within that group.

If Sendspin and `spin2dante` run on the same host, values as low as `1ms` can
work well because there is very little upstream jitter between the source and
the bridge. For more general deployments, especially when Sendspin is remote,
`5ms` remains the recommended default.

## Clock Drift Correction

spin2dante periodically compares the DANTE read position against the Sendspin
server clock. When the two drift apart by more than `drift_threshold_ms`, the
bridge shifts its scheduler anchor in place instead of forcing a full rebuffer.

Defaults:
- `drift_threshold_ms: 5`
- `drift_check_interval_ms: 1000`
- `max_correction_samples_per_tick: 48`

This keeps long-running bridges aligned while capping each single correction to
about `1ms` at 48kHz. Large anomalies still fall back to a full rebuffer.

Keep `max_correction_samples_per_tick` conservative. The default `48` samples
is chosen to stay comfortably below the backward-target rebuffer path. Values
above roughly `100` samples increase the chance that a single correction could
force a full rebuffer when chunks are small.

## Notes

- The add-on uses host networking because DANTE discovery and multicast audio depend on it.
- Each configured bridge gets its own `TMPDIR` under `/share`, which matches Inferno's container requirements for `usrvclock`.
- All bridges share the same PTP clock source, but they still need unique `process_id` and `alt_port` values.
- `buffer_ms` is real playout delay. A bridge at `100ms` will play about `95ms` later than one at `5ms`.

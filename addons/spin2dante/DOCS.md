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
- `bridges`: List of bridge definitions

Each bridge entry contains:
- `id`: Stable identifier used to derive a unique shared temp directory
- `name`: DANTE device name to advertise
- `url`: Sendspin WebSocket URL
- `buffer_ms`: Jitter buffer size in milliseconds
- `process_id`: Unique Inferno process ID on the host IP
- `alt_port`: Unique Inferno base UDP port, spaced at least 10 apart from other bridges

## Example Configuration

```yaml
clock_path: /share/usrvclock
wait_for_clock_seconds: 30
log_level: info
bridges:
  - id: kitchen
    name: Kitchen
    url: ws://music-assistant.local:8927/sendspin
    buffer_ms: 5
    process_id: 1
    alt_port: 14000
  - id: livingroom
    name: Living Room
    url: ws://music-assistant.local:8927/sendspin
    buffer_ms: 5
    process_id: 2
    alt_port: 14010
```

Use a unique `process_id` and `alt_port` for every bridge. Keep `alt_port`
values at least 10 apart.

## Notes

- The add-on uses host networking because DANTE discovery and multicast audio depend on it.
- Each configured bridge gets its own `TMPDIR` under `/share`, which matches Inferno's container requirements for `usrvclock`.
- All bridges share the same PTP clock source, but they still need unique `process_id` and `alt_port` values.

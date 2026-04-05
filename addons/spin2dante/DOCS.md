# spin2dante

This add-on connects to a Sendspin-compatible WebSocket source, such as Music Assistant,
and advertises the incoming audio as a DANTE transmitter on your local network.

## Requirements

- A DANTE-capable receiver on the same L2 network
- A PTP clock exported to `/share/usrvclock`
- A Sendspin source URL, for example Music Assistant's Sendspin output

## Pairing with Statime

This repository also includes a `statime` add-on. The intended setup is:

1. Start the `statime` add-on first.
2. Confirm it creates `/share/usrvclock`.
3. Start `spin2dante` and point it at your Sendspin source.

## Options

- `url`: Sendspin WebSocket URL
- `name`: DANTE device name to advertise
- `buffer_ms`: Jitter buffer size in milliseconds
- `clock_path`: Path to the exported usrvclock socket
- `log_level`: Rust log level

## Notes

- The add-on uses host networking because DANTE discovery and multicast audio depend on it.
- The add-on mounts `/share` read-write so it can reach the exported clock socket and create its temporary clock client sockets there.

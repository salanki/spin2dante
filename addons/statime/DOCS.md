# statime

This add-on runs Statime as a PTP daemon and exports a usrvclock socket for
`spin2dante` and other inferno-based audio tools.

## Requirements

- Host networking enabled, which this add-on requests automatically
- Permission to adjust time, granted via the add-on configuration
- A network with an existing PTP grandmaster if you run it as a follower

## Options

- `ptp_interface`: Network interface to bind. Use `auto` to detect the default route interface.
- `clock_path`: Path to the exported usrvclock socket.
- `log_level`: Statime log level.

## Notes

- The add-on writes the clock socket into `/share` so companion add-ons can consume it.
- Start this add-on before `spin2dante`.

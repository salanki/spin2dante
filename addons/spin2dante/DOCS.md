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
- `dante_bind`: Interface name or IPv4 address to use for DANTE/mDNS traffic. Use `auto` to let Inferno choose the host's default local IPv4 address.
- `drift_threshold_ms`: Drift threshold in milliseconds before gradual single-frame correction begins
- `drift_check_interval_ms`: How often, in milliseconds, to sample drift between the Sendspin and PTP timelines
- `max_correction_samples_per_tick`: Maximum single-frame repeat/drop budget per drift-check interval. Set to `0` to disable correction.
- `sync_log_interval_seconds`: Seconds between attributed INFO-level sync summaries. Default: `120`. Five-second detail remains available at `debug`.
- `bridges`: List of bridge definitions

Each bridge entry contains:
- `id`: Stable identifier used to derive a unique shared temp directory
- `name`: DANTE device name to advertise
- `url`: Sendspin WebSocket URL
- `buffer_ms`: Playout buffer / latency in milliseconds. Larger values improve jitter tolerance, but they also delay audio by that amount.
- `dante_latency`: DANTE transmit latency in milliseconds, advertised to receivers as their minimum playout buffer. Allowed values: `0.5`, `1`, `2`, `5`, `10`, `20`. Default: `10`.
- `volume_control`: Volume control mode — `none` (default, passthrough) or `bridge` (software gain). See [Volume Control](#volume-control) below.
- `process_id`: Unique Inferno process ID on the host IP
- `alt_port`: Unique Inferno base UDP port, spaced at least 10 apart from other bridges
- `report_dante_subscriber`: (optional) When true, the bridge reports ExternalSource to Music Assistant when no DANTE receiver is subscribed, causing the player to leave its group. Default: false.

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
dante_bind: auto
drift_threshold_ms: 5
drift_check_interval_ms: 1000
max_correction_samples_per_tick: 48
sync_log_interval_seconds: 120
bridges:
  - id: kitchen
    name: Kitchen
    url: ws://127.0.0.1:8927/sendspin
    buffer_ms: 5
    volume_control: none
    process_id: 1
    alt_port: 14000
  - id: livingroom
    name: Living Room
    url: ws://127.0.0.1:8927/sendspin
    buffer_ms: 5
    volume_control: bridge
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

## DANTE Network Binding

By default, Inferno chooses the host's default local IPv4 address for DANTE and
mDNS traffic. If your DANTE devices are on a secondary NIC or VLAN without the
default route, set `dante_bind` to that interface name or to the IPv4 address on
that network.

Examples:
- `dante_bind: eth1`
- `dante_bind: eth1.20`
- `dante_bind: 192.168.50.2`

## Volume Control

When a DANTE zone does not expose controllable volume through Home Assistant or
the downstream amplifier, you can enable bridge-side software volume control as
a fallback. Set `volume_control: bridge` on the bridge entry.

When enabled:
- Music Assistant shows a volume slider and mute toggle for that player
- Volume and mute commands are applied as software gain inside the bridge
- At 100% volume with mute off, audio is bit-perfect (true no-op)
- Volume changes use a smooth 20ms ramp to prevent clicks

The volume slider uses a linear-in-dB (audio) taper: every step changes
loudness by the same 0.4 dB, with 100% = full (bit-perfect) level, 75% ≈
−10 dB, 50% ≈ −20 dB, and a smooth fade to true silence below 10%. This
makes the whole slider range useful — not just the bottom half.

### Converting volumes from older versions

Versions up to `sha-9b05ab9` used a different curve (`(volume/100)^1.5`)
where most of the audible range was crammed into 0–50%. With the new taper
a given volume % is quieter than before (50% went from −9 dB to −20 dB).

If you have **presets, automations, scenes, or scripts that set player
volume**, convert stored values to keep the same loudness:

`new = 100 − 75 × log10(100 / old)` (for old ≥ 7; round to nearest)

For old volumes below 7 (rare — these were barely audible on the old
curve), the converted value lands in the fade-to-silence region below the
new taper's 10% knee, so use this formula instead:

`new = 631 × (old / 100)^1.5` (for old < 7; round to nearest)

Both formulas agree at the crossover (old ≈ 6.3 → new = 10), and the
table below already uses the correct branch for every row.

| Old | New | Old | New |
|----:|----:|----:|----:|
| 100 | 100 | 40 | 70 |
| 90 | 97 | 30 | 61 |
| 80 | 93 | 25 | 55 |
| 75 | 91 | 20 | 48 |
| 70 | 88 | 15 | 38 |
| 60 | 83 | 10 | 25 |
| 50 | 77 | 5 | 7 |

The saved per-bridge volume state restored at startup is also affected the
same way: the number is kept, so the zone will sound quieter until you
nudge the slider up per the table.

When set to `none` (the default), the bridge is a transparent passthrough with
no volume capability advertised. This is the right choice when volume is
controlled by the amplifier or via Home Assistant.

Each bridge has independent volume state. You can enable `bridge` on zones that
need it and leave `none` on zones where the amplifier handles volume.

Volume and mute state persists across add-on restarts. When the bridge starts,
it restores the last-known volume instead of defaulting to 100%.

## Clock Drift Correction

spin2dante periodically compares the DANTE read position against the Sendspin
server clock. When the two drift apart by more than `drift_threshold_ms`, the
bridge uses the sendspin-rs correction planner to distribute isolated complete
stereo-frame repeats (slow source clock) or drops (fast source clock). It moves
the scheduler anchor by each correction actually applied, keeping later queued
chunks contiguous.

Defaults:
- `drift_threshold_ms: 5`
- `drift_check_interval_ms: 1000`
- `max_correction_samples_per_tick: 48`
- `sync_log_interval_seconds: 120`

The default budget allows at most 48 one-frame events per interval. Each event
is 20.8 microseconds at 48kHz, and the planner spreads them over time. Set the
budget to `0` to disable drift correction. Ring-scale anomalies and planner
reanchor requests still fall back to a full rebuffer.

PCM remains bit-exact except for these logged single-frame repeats/drops (and
software gain when bridge-side volume is enabled below 100%).

### Sync diagnostics

Every INFO-level `[sync]` record identifies its bridge and current local
stream session. `stream_start_us` is the first Sendspin audio timestamp seen in
that session; records with the same nonzero value carry the same source
timeline and can be compared.

`timeline_offset_frames` is the median-filtered signed DANTE read-position
error against the shared Sendspin/PTP prediction. Subtract the values from two
records in the same reporting window to estimate their electronic playout
skew. At 48 kHz, 48 frames = 1 ms. This is a direct current-timeline
measurement; do not infer skew from the process-lifetime
`drift_inserted_frames` and `drift_dropped_frames` counters.

Example:

```text
[sync] bridge_id=livingroom bridge_name="Living Room" ... session=1 \
stream_start_us=1842000000 drift_valid=1 timeline_offset_frames=-72 \
timeline_offset_us=-1500 raw_offset_frames=-65 anchor_correction_frames=174 ...
```

Use the bundled analyzer on saved App/container logs:

```bash
python3 test/common/sync_log_analyzer.py spin2dante.log
```

It reports maximum, median, and p95 pairwise skew; the two bridges at the
maximum; whether each stream's spread is growing or reconverging; and maximum
fault counters. Records from different `stream_start_us` values are never
compared.

INFO summaries default to every two minutes to preserve useful history on
multi-bridge installations. Correction start/stop events remain immediate at
INFO; cadence-only adjustments and five-second sync/buffer snapshots are
DEBUG-level. Warnings and errors are never rate-limited.

## Notes

- The add-on uses host networking because DANTE discovery and multicast audio depend on it.
- Each configured bridge gets its own `TMPDIR` under `/share`, which matches Inferno's container requirements for `usrvclock`.
- All bridges share the same PTP clock source, but they still need unique `process_id` and `alt_port` values.
- `buffer_ms` is real playout delay. A bridge at `100ms` will play about `95ms` later than one at `5ms`.

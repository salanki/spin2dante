# spin2dante

This add-on runs one or more `spin2dante` bridge processes and advertises each
configured stream as its own DANTE transmitter on your local network.

## Reference Deployment

The add-on is used continuously in a residential whole-house audio deployment
with approximately 20 independent stereo zones. Music Assistant, Statime, and
spin2dante run as Home Assistant apps inside a Home Assistant OS virtual
machine, with one bridge process per DANTE zone.

The reference deployment uses UniFi network infrastructure and DANTE-capable
amplifiers and receivers from Blaze Audio, Origin Acoustics, and Wisdom Audio.
All bridges share the same Sendspin timeline and DANTE PTP clock. Grouped
playback is used across multiple DANTE zones and has also been tested in mixed
Sonos/DANTE groups.

```text
Home Assistant OS VM
┌─────────────────────────────────────────────────┐
│ Music Assistant                                 │
│       │ Sendspin                                 │
│       ▼                                          │
│ spin2dante — one bridge process per DANTE zone  │
│       │ DANTE + PTP via PCI-passthrough NIC      │
└───────┼─────────────────────────────────────────┘
        ▼
Main-rack UniFi switch
├── Wisdom amplifier — elected PTP grandmaster
├── DANTE receivers on the main rack
└── UniFi inter-switch trunks
    ├── Downstream UniFi switch ── DANTE receivers
    ├── Downstream UniFi switch ── DANTE receivers
    └── Downstream UniFi switch ── additional zones
```

DANTE audio and PTP remain on one Layer-2 DANTE network across the inter-switch
trunks. This validates operation with receivers behind multiple switches, but
does not imply that arbitrary multicast, QoS, VLAN, or switch configurations
will work without appropriate setup.

The Home Assistant OS VM runs on Proxmox with vCPUs pinned 1:1 to isolated host
CPUs and scheduled with `SCHED_FIFO`. It receives a dedicated Broadcom DANTE
NIC through PCI passthrough, reducing host-scheduler and virtualized-network
variability. These are characteristics of the validated deployment, not strict
requirements for the add-on.

The Wisdom amplifier supplies the hardware PTP grandmaster. The Home Assistant
VM follows it through Statime. Although the NIC is passed through directly, it
exposes no PTP hardware clock, so timestamping and transmit timing remain
software-based. Every bridge in this deployment uses `dante_latency: 10`,
providing reasonable packet-jitter and scheduling headroom for the VM-based
transmitter. This adds a common latency floor; it is not a 10ms cross-zone
synchronization error.

This is an example of a validated real-hardware deployment, not a hardware or
topology requirement. Controlled tests measured the initial anchor-mapping
spread at 1-16 samples; long-running monitoring uses an operational pairwise
skew target below 2ms.

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

`drift_since_anchor_frames` is the median-filtered DANTE read-position error
against the bridge's scheduler mapping. That mapping pairs the globally shared
DANTE/PTP read clock with Sendspin server time, and applied corrections move
the anchor, so the metric retains both anchor-placement error and correction
effects. It is the input to the correction loop, and a healthy bridge holds it
near zero.

**`playout_offset_frames` is the number to compare between zones.** It is
`drift_since_anchor_frames` minus that bridge's own prebuffer (`buffer_ms`,
also logged as `prebuffer_frames`), which makes it the signed position of the
audio at the DANTE read head on the shared Sendspin timeline. A healthy bridge
sits at roughly `-prebuffer_frames`. Subtract simultaneous records from two
bridges on the same stream to get their electronic playout skew. At 48 kHz,
48 frames = 1 ms.

Use the offset rather than the drift because `buffer_ms` is real playout delay
(see Notes): two zones at `5` and `55` genuinely play 50 ms apart, and only the
offset shows that — both report drift near zero.

Do not infer current skew from process-lifetime inserted/dropped counters.

Example:

```text
[sync] bridge_id=livingroom bridge_name="Living Room" ... session=1 \
stream_start_us=1842000000 drift_valid=1 playout_offset_frames=-312 \
playout_offset_us=-6500 prebuffer_frames=240 drift_since_anchor_frames=-72 \
drift_since_anchor_us=-1500 \
raw_drift_since_anchor_frames=-65 anchor_correction_frames=174 ...
```

From a checkout of this repository, use the analyzer on saved App/container
logs (the analyzer is not included in the Home Assistant App image):

```bash
python3 test/common/sync_log_analyzer.py spin2dante.log
```

It reports maximum, median, and p95 pairwise skew; the two bridges at the
maximum; whether each stream's spread is growing or reconverging; and maximum
fault counters. Records from different `stream_start_us` values are never
compared. Its default 120-second, stream-relative windows tolerate normal
per-process logging phase differences, but records more than 10 seconds apart
are not treated as simultaneous. The analyzer keeps the largest coherent
subset, reports excluded bridges as paired ID/name objects and aggregate
counts, and only rejects the entire window when no valid pair remains.
`bridges_never_compared` calls out every valid zone that never participated in
a comparison, including a persistently late zone or one that reconnected with
a different `stream_start_us`, so a clean subset maximum cannot be mistaken for
all-zone health. No comparable records produce `null` skew fields rather than a
misleading zero. Output also distinguishes all seen records from valid and
skipped records. Window origins come from the earliest record available for
each stream, so rotated or truncated inputs can shift window boundaries; the
time-gap guard remains authoritative.

INFO summaries default to every two minutes to preserve useful history on
multi-bridge installations. Processes use shared wall-clock slots so periodic
records are emitted close enough together for comparison; track changes and
rebuffers preserve the cadence. Correction start/stop events remain immediate
at INFO; cadence-only adjustments and five-second sync/buffer snapshots are
DEBUG-level. Warnings and errors are never rate-limited.

## Notes

- The add-on uses host networking because DANTE discovery and multicast audio depend on it.
- Each configured bridge gets its own `TMPDIR` under `/share`, which matches Inferno's container requirements for `usrvclock`.
- All bridges share the same PTP clock source, but they still need unique `process_id` and `alt_port` values.
- `buffer_ms` is real playout delay. A bridge at `100ms` will play about `95ms` later than one at `5ms`.

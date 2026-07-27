# Plan: Linear-in-dB volume taper (bridge-side, no sendspin-rs changes)

## Problem

With `--volume-control=bridge`, the volume slider is effective 0–50% but nearly
flat 50–100%. Cause: the gain target comes from `sendspin-rs`'s
`GainControl::gain()`, which maps volume→amplitude via `(volume/100)^1.5`. In
dB terms 50→100% spans only ~9 dB, while the entire loud-to-silence journey
(−9 dB → −∞) is crammed into 0–50%. Human loudness perception is
logarithmic, so the top half of the fader is perceptually dead.

## Approach

Stop using `GainControl::gain()` as the ramp target. Keep `GainControl` as the
source of truth for the volume *number* (0–100) and mute state (it handles the
server command plumbing, atomics, clamping, persistence inputs), but compute
the gain in spin2dante with a **linear-in-dB (audio) taper**:

- dB-line anchored at **−40 dB**: `dB = 40·(vol/100 − 1)` for vol ≥ 10%,
  i.e. a constant 0.4 dB per 1% step. 100% = 0 dBFS = unity (bit-perfect
  no-op preserved). 50% = −20 dB. 75% = −10 dB.
- Knee: below **10%**, fade linearly from the curve's value at the knee
  (−36 dB) down to true zero, avoiding an abrupt jump to silence at 0%.
  Note: −40 dB is the *anchor* of the dB line, not a true floor — below
  the knee the output drops past −40 dB (≈−56 dB at 1%) and reaches exact
  zero at 0%. Docs and tests must not describe −40 dB as the minimum
  non-zero gain.
- Mute → gain 0.0 exactly (matches current `GainControl` behavior).

No protocol impact: volume taper is endpoint-local; the wire only carries the
0–100 number. `sendspin-rs` is untouched.

## Changes

### 1. `src/gain.rs` — new taper function

```rust
/// Map a 0-100 volume + mute state to linear gain using a linear-in-dB
/// (audio) taper: 0.4 dB per 1% step, unity at 100%, with a linear fade
/// to true zero below the knee.
pub(crate) fn volume_to_gain(volume: u8, muted: bool) -> f32 {
    const MIN_DB: f32 = -40.0;
    const KNEE: f32 = 0.1; // below 10%, fade linearly to silence

    if muted {
        return 0.0;
    }
    let vol = f32::from(volume.min(100)) / 100.0;
    if vol <= 0.0 {
        0.0
    } else if vol < KNEE {
        (vol / KNEE) * 10f32.powf(MIN_DB / 20.0 * (1.0 - KNEE))
    } else {
        10f32.powf(MIN_DB / 20.0 * (1.0 - vol))
    }
}
```

Notes:
- `volume == 100, !muted` must return exactly `1.0` so the bit-perfect
  fast path in `BridgeGainRamp::apply` (`current_gain == 1.0` early-return)
  still engages. `10f32.powf(0.0)` is exactly 1.0, but add a unit test to
  pin it.
- Bit-perfect scope: the guarantee is **post-ramp steady state** at 100%
  volume, mute off. During the 20 ms ramp after a mute-off or volume-up
  transition, samples are scaled; once the ramp completes, `current_gain`
  snaps exactly to 1.0 and the no-op fast path re-engages. This matches
  current behavior (unchanged by this plan) — docs should say "bit-perfect
  at steady state", not imply instantaneous exactness through transitions.
- Input is `u8`, so no NaN/negative concerns. `min(100)` is belt-and-braces
  (GainControl and state::load already clamp).
- Deterministic pure function: `BridgeGainRamp::update_target` compares the
  target's bit pattern, so recomputing per chunk is fine — the same volume
  always yields bit-identical f32.

### 2. `src/bridge.rs` — swap the 5 `gc.gain()` call sites

Add a tiny helper (module-level or inherent fn) to avoid repeating the pair
of atomic reads:

```rust
fn gain_target(gc: &GainControl) -> f32 {
    crate::gain::volume_to_gain(gc.volume(), gc.is_muted())
}
```

Call sites to update (replace `gc.gain()` with `gain_target(gc)`):
- `bridge.rs:202` — initial ramp gain at construction
  (`BridgeGainRamp::with_gain(...)`)
- `bridge.rs:882` — `gain_ramp.advance(dropped.frames, ...)` (dropped chunks)
- `bridge.rs:991` — `gain_ramp.advance(chunk.frames, ...)` (early chunks)
- `bridge.rs:1127` — `gain_ramp.apply(...)` in `write_samples_at`
- `bridge.rs:1144` — `let target = ...` in `write_trimmed_samples`

Everything else that touches `GainControl` stays as-is: `set_volume`/
`set_mute` command handling (bridge.rs:553–566), state persistence
(`save_volume_state`, bridge.rs:717), and `PlayerState` reporting
(bridge.rs:730–732) all use the raw volume/mute, not the gain.

Concurrency note: `gc.volume()` and `gc.is_muted()` are two separate atomic
reads, so a concurrent volume+mute change could be observed torn. This is the
same (documented) situation as inside `GainControl` itself — harmless because
the ramp smooths any one-chunk transient.

### 3. Unit tests (`src/gain.rs` tests module)

- `volume_to_gain(100, false) == 1.0` exactly (bit-perfect path).
- `volume_to_gain(x, true) == 0.0` for representative x.
- `volume_to_gain(0, false) == 0.0`.
- `volume_to_gain(50, false) ≈ 0.1` (−20 dB) within f32 tolerance.
- `volume_to_gain(75, false) ≈ 0.3162` (−10 dB).
- Continuity at the knee: `volume_to_gain(10)` from both branches agree
  (the `vol < KNEE` branch at `vol == KNEE` equals the curve value).
- Monotonic non-decreasing over 0..=100.
- Values >100 clamp to unity.

Existing `BridgeGainRamp` tests are unaffected (they pass explicit float
targets, not volumes).

### 4. E2E volume test harness

`test/volume/validate.py` hard-codes the old curve:
- Line 24: `MIN_REDUCTION_DB = 6.0` (comment says volume 50 → ~−9 dB).
  New expectation: volume 50 → **−20 dB**. Replace the single lower bound
  with a **mandatory two-sided tolerance check**: compute the expected
  reduction from the taper (mirrored in Python, incl. knee) and require
  `abs(reduction_db - expected_db) <= 3.0`. For volume 50 that means
  17–23 dB passes (RMS windowing gives ~±1 dB of slop; 3 dB is generous
  but still rejects both the old curve (−9 dB) and a near-mute (−40+ dB)).
- Line 127: `expected_ratio = (v/100) ** 1.5` → replace with the new taper
  function so the expected value used in the tolerance check is derived
  from the same curve as the Rust implementation.
- The `rms_after < 1.0` silence guard still works: −20 dB of a full-scale
  test tone is far above the floor.

`test/volume/test_server.py` (TARGET_VOLUME = 50) needs no change.

### 5. Docs

- `README.md:187` — replace the `^1.5` description with the new taper:
  linear-in-dB, 0.4 dB/% (−40 dB anchor), fading to true silence below
  10%; keep the "100% is bit-perfect" guarantee statement, scoped to
  steady state (after the 20 ms ramp settles).
- `addons/spin2dante/DOCS.md` Volume Control section — add one line on the
  taper feel; note that a given % is now quieter than before (50% goes from
  −9 dB to −20 dB), so users may want to nudge saved volumes up.
- **Migration guidance for existing volumes/presets** (README, DOCS.md, and
  the addon CHANGELOG/release notes): both curves are exact, so an old
  volume converts to a loudness-equivalent new volume in closed form:

  `new = 100 − 75·log₁₀(100 / old)`  (for old ≥ ~6.3%; round to nearest)

  Derivation: old gain `(v/100)^1.5` equals new gain `10^(2·(x/100−1))`
  ⇒ `2·(x/100 − 1) = 1.5·log₁₀(v/100)`. For old < ~6.3% the converted
  value lands below the 10% knee: `new ≈ 631·(old/100)^1.5`.

  Quick-reference table (old → new, same loudness):

  | Old | New | | Old | New |
  |----:|----:|-|----:|----:|
  | 100 | 100 | | 40 | 70 |
  | 90 | 97 | | 30 | 61 |
  | 80 | 93 | | 25 | 55 |
  | 75 | 91 | | 20 | 48 |
  | 70 | 88 | | 15 | 38 |
  | 60 | 83 | | 10 | 25 |
  | 50 | 77 | | 5 | 7 |

  Users with automations/scenes/presets that set player volume should map
  stored values through this table (or formula) to keep the same loudness.
- `addons/spin2dante/CHANGELOG.md` — entry under a new version noting the
  behavior change, including the conversion formula and table above.

### 6. Out of scope / decisions

- **No config flag** for the taper (anchor/knee constants in code). Can be
  made configurable later if someone wants a different curve.
- **sendspin-rs untouched** — its `gain()` remains available but unused by
  spin2dante's ramp path.
- **Persisted state unaffected** — state file stores the raw 0–100 volume,
  which is unchanged in meaning as a *position*; only its loudness mapping
  shifts.
- **Idle-then-stream-start ramp** (pre-existing): a volume change while no
  stream is playing updates `GainControl` but not the ramp; on the next
  stream start, `reset_scheduler` → `reset_to_current()` snaps to the *old*
  target and the first chunk ramps 20 ms to the new gain. Confirmed
  intended (inaudible: 20 ms at stream start, from prebuffered silence)
  and orthogonal to this change — behavior identical before/after.
- **E2E coverage scope**: the E2E harness exercises only volume=50. Knee
  (<10%), 0%, mute, and exact-unity-at-100% are covered by the unit tests
  in §3; the 100% bit-perfect path is additionally covered by the existing
  bit-perfect E2E harness. Extending the volume E2E to multiple volume
  steps is a possible follow-up, not part of this change.

## Verification

1. `cargo test` — new taper unit tests + existing ramp/state tests.
2. `cargo clippy` clean.
3. E2E: `test/docker-compose.volume.yml` run via `test/volume/validate.sh`
   → expect ~20 dB reduction at volume 50.
4. Bit-perfect regression: existing E2E sync/bit-perfect harness at 100%
   volume must still pass (no-op path unchanged).

#!/usr/bin/env python3
"""
Validate that bridge-side volume control actually reduces audio amplitude.

Reads a raw 24-bit stereo PCM capture and compares RMS amplitude in two
windows: before and after the volume change point. The post-change window
should have significantly lower RMS.
"""

import struct
import sys
import math
import os

SAMPLE_RATE = 48000
CHANNELS = 2
BYTES_PER_SAMPLE = 3
FRAME_SIZE = BYTES_PER_SAMPLE * CHANNELS

# Skip margins around the volume change point (seconds)
MARGIN_S = 2.0
# Measurement window length (seconds)
WINDOW_S = 3.0
# Allowed deviation from the taper's expected reduction (dB). RMS windowing
# contributes ~±1 dB of slop; 3 dB still rejects both the old ^1.5 curve
# (-9 dB at volume 50) and an over-attenuating/near-mute taper.
TOLERANCE_DB = 3.0


def volume_to_gain(volume):
    """Mirror of the bridge's linear-in-dB taper (src/gain.rs volume_to_gain).

    dB = 40 * (vol/100 - 1) above the 10% knee; linear fade to zero below.
    """
    anchor_db = -40.0
    knee = 0.1
    vol = min(volume, 100) / 100.0
    if vol <= 0.0:
        return 0.0
    if vol < knee:
        return (vol / knee) * 10 ** (anchor_db / 20.0 * (1.0 - knee))
    return 10 ** (anchor_db / 20.0 * (1.0 - vol))


def read_24bit_samples(data, start_frame, num_frames):
    """Read left-channel 24-bit LE samples from interleaved stereo data."""
    samples = []
    for i in range(num_frames):
        offset = (start_frame + i) * FRAME_SIZE
        if offset + 3 > len(data):
            break
        b = data[offset:offset + 3]
        raw = b[0] | (b[1] << 8) | (b[2] << 16)
        if raw & 0x800000:
            raw -= 0x1000000
        samples.append(raw)
    return samples


def rms(samples):
    if not samples:
        return 0.0
    return math.sqrt(sum(s * s for s in samples) / len(samples))


def main():
    capture_path = "/shared/capture.raw"
    meta_path = "/shared/volume_test_meta.txt"

    if not os.path.exists(capture_path):
        print("FAIL: capture file not found")
        sys.exit(1)

    if not os.path.exists(meta_path):
        print("FAIL: volume test metadata not found")
        sys.exit(1)

    meta = {}
    with open(meta_path) as f:
        for line in f:
            k, v = line.strip().split("=")
            meta[k] = int(v)

    with open(capture_path, "rb") as f:
        data = f.read()

    total_capture_frames = len(data) // FRAME_SIZE
    print(f"Capture: {total_capture_frames} frames ({total_capture_frames / SAMPLE_RATE:.1f}s)")
    print(f"Volume change at source frame {meta['volume_change_at_frame']}")
    print(f"Target volume: {meta['target_volume']}")

    # We don't know the exact alignment between source and capture,
    # so we measure RMS in the first third and last third of the capture.
    # The first third should be at full volume, the last third at reduced volume.
    third = total_capture_frames // 3
    margin_frames = int(MARGIN_S * SAMPLE_RATE)
    window_frames = int(WINDOW_S * SAMPLE_RATE)

    # "Before" window: early in capture (frames margin..margin+window)
    before_start = margin_frames
    if before_start + window_frames > third:
        print("FAIL: capture too short for before-window")
        sys.exit(1)

    # "After" window: late in capture (last third, skip margin)
    after_start = 2 * third + margin_frames
    if after_start + window_frames > total_capture_frames:
        after_start = total_capture_frames - window_frames - margin_frames
        if after_start < 2 * third:
            print("FAIL: capture too short for after-window")
            sys.exit(1)

    before_samples = read_24bit_samples(data, before_start, window_frames)
    after_samples = read_24bit_samples(data, after_start, window_frames)

    rms_before = rms(before_samples)
    rms_after = rms(after_samples)

    print(f"\nBefore window: frames {before_start}-{before_start + window_frames} "
          f"({before_start / SAMPLE_RATE:.1f}s-{(before_start + window_frames) / SAMPLE_RATE:.1f}s)")
    print(f"After window:  frames {after_start}-{after_start + window_frames} "
          f"({after_start / SAMPLE_RATE:.1f}s-{(after_start + window_frames) / SAMPLE_RATE:.1f}s)")
    print(f"RMS before: {rms_before:.1f}")
    print(f"RMS after:  {rms_after:.1f}")

    if rms_before < 1.0:
        print("FAIL: before-window RMS is near zero (no audio?)")
        sys.exit(1)

    if rms_after < 1.0:
        # Could be muted — that's fine for volume=0 but not volume=50
        if meta['target_volume'] > 0:
            print("FAIL: after-window RMS is near zero (volume not applied?)")
            sys.exit(1)

    reduction_db = 20 * math.log10(rms_before / rms_after) if rms_after > 0 else float('inf')
    print(f"Reduction: {reduction_db:.1f} dB")

    # Two-sided check against the taper: catches both under-attenuation
    # (e.g. the old ^1.5 curve) and over-attenuation (broken taper near-muting).
    expected_ratio = volume_to_gain(meta['target_volume'])
    expected_db = -20 * math.log10(expected_ratio) if expected_ratio > 0 else float('inf')
    print(f"Expected reduction for volume={meta['target_volume']}: {expected_db:.1f} dB "
          f"(tolerance ±{TOLERANCE_DB} dB)")

    if abs(reduction_db - expected_db) > TOLERANCE_DB:
        print(f"FAIL: expected {expected_db:.1f} ±{TOLERANCE_DB} dB reduction, "
              f"got {reduction_db:.1f} dB")
        sys.exit(1)

    print(f"\nPASS: volume at {meta['target_volume']}% reduced amplitude by {reduction_db:.1f} dB")


if __name__ == "__main__":
    main()

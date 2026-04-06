#!/usr/bin/env python3
"""Generate a deterministic reference signal for bit-perfect testing.

Produces:
  1. A 16-bit stereo WAV file (input to sendspin serve)
  2. A capture-domain raw file (expected output from inferno2pipe)

sendspin serve decodes all audio to s16 internally (hardcoded in PyAV
resampler), so we generate at 16-bit to match what actually gets sent.

The capture-domain reference matches the full conversion chain:
  s16 value → sendspin sends as 24-bit LE (zero-padded lower byte)
  → bridge parses 24-bit LE, sign-extends, shifts << 8
  → inferno writes upper-24-bit i32
  → inferno2pipe captures as native-endian i32

Net effect: s16 value ends up in upper 16 bits of i32, lower 16 bits zero.
  capture_i32 = s16_value << 16
"""
import argparse
import struct
import wave


def xorshift32(state: int) -> int:
    state &= 0xFFFFFFFF
    state ^= (state << 13) & 0xFFFFFFFF
    state ^= (state >> 17) & 0xFFFFFFFF
    state ^= (state << 5) & 0xFFFFFFFF
    return state & 0xFFFFFFFF


def to_signed_16(state: int) -> int:
    """Extract a signed 16-bit sample from PRBS state."""
    sample = (state >> 8) & 0xFFFF
    if sample >= 0x8000:
        sample -= 0x10000
    return sample


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate deterministic reference WAV + capture-domain raw")
    parser.add_argument("--wav-path", required=True)
    parser.add_argument("--capture-raw-path", required=True)
    parser.add_argument("--duration", type=int, default=30)
    parser.add_argument("--sample-rate", type=int, default=48000)
    args = parser.parse_args()

    left_state = 0x13579BDF
    right_state = 0x2468ACE1
    total_frames = args.duration * args.sample_rate

    with wave.open(args.wav_path, "wb") as wav_file, open(args.capture_raw_path, "wb") as raw_file:
        wav_file.setnchannels(2)
        wav_file.setsampwidth(2)  # 16-bit WAV (matches sendspin's s16 decode)
        wav_file.setframerate(args.sample_rate)

        for _ in range(total_frames):
            left_state = xorshift32(left_state)
            right_state = xorshift32(right_state)

            left_s16 = to_signed_16(left_state)
            right_s16 = to_signed_16(right_state)

            # WAV: 16-bit LE interleaved stereo
            wav_file.writeframesraw(struct.pack("<hh", left_s16, right_s16))

            # Capture domain: s16 value ends up in upper 16 bits of i32
            # Chain: s16 → s24 (zero-pad) → parse 24-bit LE → sign-extend → << 8
            # Net: s16 << 16
            raw_file.write(struct.pack("<i", left_s16 << 16))
            raw_file.write(struct.pack("<i", right_s16 << 16))

    print(
        f"Generated deterministic reference: {args.duration}s, {args.sample_rate}Hz, 16-bit stereo PRBS"
    )
    print(
        f"  WAV: {args.wav_path} (16-bit, matches sendspin serve s16 decode)"
    )
    print(
        f"  Raw: {args.capture_raw_path} (i32 upper-16-bit, matches inferno2pipe capture)"
    )


if __name__ == "__main__":
    main()

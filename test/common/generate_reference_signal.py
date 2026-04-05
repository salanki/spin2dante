#!/usr/bin/env python3
import argparse
import struct
import wave


def xorshift32(state: int) -> int:
    state &= 0xFFFFFFFF
    state ^= (state << 13) & 0xFFFFFFFF
    state ^= (state >> 17) & 0xFFFFFFFF
    state ^= (state << 5) & 0xFFFFFFFF
    return state & 0xFFFFFFFF


def to_signed_24(state: int) -> int:
    sample = (state >> 8) & 0xFFFFFF
    if sample >= 0x800000:
        sample -= 0x1000000
    return sample


def pack_s24le(sample: int) -> bytes:
    sample &= 0xFFFFFF
    return bytes((sample & 0xFF, (sample >> 8) & 0xFF, (sample >> 16) & 0xFF))


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
        wav_file.setsampwidth(3)
        wav_file.setframerate(args.sample_rate)

        for _ in range(total_frames):
            left_state = xorshift32(left_state)
            right_state = xorshift32(right_state)

            left_sample = to_signed_24(left_state)
            right_sample = to_signed_24(right_state)

            wav_file.writeframesraw(pack_s24le(left_sample) + pack_s24le(right_sample))
            raw_file.write(struct.pack("<i", left_sample << 8))
            raw_file.write(struct.pack("<i", right_sample << 8))

    print(
        f"Generated deterministic reference: {args.duration}s, {args.sample_rate}Hz, 24-bit stereo PRBS"
    )


if __name__ == "__main__":
    main()

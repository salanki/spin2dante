#!/usr/bin/env python3
import argparse
import json
import sys


def find_alignment(reference: bytes, capture: bytes, frame_bytes: int, window_frames: int, probe_frames: int):
    capture_frames = len(capture) // frame_bytes
    reference_frames = len(reference) // frame_bytes
    max_start = min(max(capture_frames - window_frames, 0), probe_frames)
    zero_window = b"\x00" * (window_frames * frame_bytes)

    for capture_start in range(max_start + 1):
        start_byte = capture_start * frame_bytes
        end_byte = start_byte + window_frames * frame_bytes
        window = capture[start_byte:end_byte]
        if len(window) < window_frames * frame_bytes or window == zero_window or not any(window):
            continue

        ref_byte = reference.find(window)
        while ref_byte != -1 and (ref_byte % frame_bytes) != 0:
            ref_byte = reference.find(window, ref_byte + 1)
        if ref_byte != -1:
            return capture_start, ref_byte // frame_bytes

    return None, None


def analyze(reference: bytes, capture: bytes, sample_rate: int, channels: int, min_run_seconds: float):
    frame_bytes = channels * 4
    if len(reference) < frame_bytes or len(capture) < frame_bytes:
        return {
            "alignment_found": False,
            "pass": False,
            "reason": "reference or capture too small",
        }

    capture_start, reference_start = find_alignment(
        reference=reference,
        capture=capture,
        frame_bytes=frame_bytes,
        window_frames=64,
        probe_frames=sample_rate * 10,
    )
    if capture_start is None:
        return {
            "alignment_found": False,
            "pass": False,
            "reason": "no exact alignment window found",
        }

    offset_frames = reference_start - capture_start
    capture_first = max(0, -offset_frames)
    reference_first = max(0, offset_frames)
    capture_frames = len(capture) // frame_bytes
    reference_frames = len(reference) // frame_bytes
    overlap_frames = min(capture_frames - capture_first, reference_frames - reference_first)

    longest_start = None
    longest_len = 0
    run_start = None
    run_len = 0
    matched_frames = 0
    run_count = 0

    for i in range(overlap_frames):
        c0 = (capture_first + i) * frame_bytes
        r0 = (reference_first + i) * frame_bytes
        exact = capture[c0:c0 + frame_bytes] == reference[r0:r0 + frame_bytes]
        if exact:
            matched_frames += 1
            if run_start is None:
                run_start = i
                run_len = 1
                run_count += 1
            else:
                run_len += 1
            if run_len > longest_len:
                longest_len = run_len
                longest_start = run_start
        else:
            run_start = None
            run_len = 0

    min_run_frames = int(min_run_seconds * sample_rate)
    longest_end = None if longest_start is None else longest_start + longest_len
    single_contiguous_match = matched_frames == longest_len

    result = {
        "alignment_found": True,
        "offset_frames": offset_frames,
        "offset_ms": offset_frames * 1000.0 / sample_rate,
        "capture_probe_start_frame": capture_start,
        "reference_probe_start_frame": reference_start,
        "capture_frames": capture_frames,
        "reference_frames": reference_frames,
        "overlap_frames": overlap_frames,
        "matched_frames": matched_frames,
        "match_ratio": (matched_frames / overlap_frames) if overlap_frames else 0.0,
        "run_count": run_count,
        "longest_run_frames": longest_len,
        "longest_run_seconds": longest_len / sample_rate,
        "longest_run_start_frame": longest_start,
        "longest_run_end_frame": longest_end,
        "single_contiguous_match": single_contiguous_match,
    }

    if longest_len < min_run_frames:
        result["pass"] = False
        result["reason"] = f"longest exact run too short ({longest_len} frames)"
    elif not single_contiguous_match:
        result["pass"] = False
        result["reason"] = "exact matches split into multiple runs"
    else:
        result["pass"] = True
        result["reason"] = "bit-perfect overlap found"

    return result


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare capture against deterministic reference")
    parser.add_argument("--reference", required=True)
    parser.add_argument("--capture", required=True)
    parser.add_argument("--label", default="capture")
    parser.add_argument("--sample-rate", type=int, default=48000)
    parser.add_argument("--channels", type=int, default=2)
    parser.add_argument("--min-run-seconds", type=float, default=5.0)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    with open(args.reference, "rb") as ref_file:
        reference = ref_file.read()
    with open(args.capture, "rb") as capture_file:
        capture = capture_file.read()

    result = analyze(reference, capture, args.sample_rate, args.channels, args.min_run_seconds)
    result["label"] = args.label

    if args.json:
        json.dump(result, sys.stdout)
        sys.stdout.write("\n")
    else:
        print(f"[{args.label}] alignment_found={result['alignment_found']}")
        if result["alignment_found"]:
            print(
                f"[{args.label}] offset={result['offset_frames']} frames ({result['offset_ms']:+.2f}ms), "
                f"longest_run={result['longest_run_frames']} frames ({result['longest_run_seconds']:.2f}s), "
                f"run_count={result['run_count']}, match_ratio={result['match_ratio']:.3f}"
            )
        print(f"[{args.label}] {result['reason']}")

    return 0 if result["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

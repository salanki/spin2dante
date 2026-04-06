#!/usr/bin/env python3
import argparse
import json
import sys


def is_nonzero_frame(capture: bytes, frame_start: int, frame_bytes: int) -> bool:
    start_byte = frame_start * frame_bytes
    frame = capture[start_byte:start_byte + frame_bytes]
    return len(frame) == frame_bytes and any(frame)


def try_match_window(reference: bytes, capture: bytes, frame_bytes: int, capture_start: int, window_frames: int):
    start_byte = capture_start * frame_bytes
    end_byte = start_byte + window_frames * frame_bytes
    window = capture[start_byte:end_byte]
    if len(window) < window_frames * frame_bytes:
        return None

    ref_byte = reference.find(window)
    while ref_byte != -1 and (ref_byte % frame_bytes) != 0:
        ref_byte = reference.find(window, ref_byte + 1)
    if ref_byte != -1:
        return capture_start, ref_byte // frame_bytes

    return None


def iter_candidate_starts(first_nonzero: int, max_start: int):
    yielded = set()

    # Search densely around the first audible region to keep the common case fast.
    local_end = min(max_start, first_nonzero + 4096)
    for step in (16, 1):
        for capture_start in range(first_nonzero, local_end + 1, step):
            if capture_start not in yielded:
                yielded.add(capture_start)
                yield capture_start

    # If startup garbage exists before valid aligned audio, probe deeper into the capture
    # at wider intervals so we can still find a later exact window without scanning every frame.
    for step, limit in (
        (256, min(max_start, first_nonzero + 65536)),
        (1024, max_start),
    ):
        start = local_end + 1
        if start > limit:
            continue
        for capture_start in range(start, limit + 1, step):
            if capture_start not in yielded:
                yielded.add(capture_start)
                yield capture_start


def find_alignment(reference: bytes, capture: bytes, frame_bytes: int, window_frames: int, probe_frames: int):
    capture_frames = len(capture) // frame_bytes
    max_start = min(max(capture_frames - window_frames, 0), probe_frames)

    first_nonzero = None
    for capture_start in range(max_start + 1):
        if not is_nonzero_frame(capture, capture_start, frame_bytes):
            continue
        first_nonzero = capture_start
        break

    if first_nonzero is None:
        return None, None

    for capture_start in iter_candidate_starts(first_nonzero, max_start):
        if not is_nonzero_frame(capture, capture_start, frame_bytes):
            continue
        match = try_match_window(reference, capture, frame_bytes, capture_start, window_frames)
        if match is not None:
            return match

    return None, None


def analyze(
    reference: bytes,
    capture: bytes,
    sample_rate: int,
    channels: int,
    min_run_seconds: float,
    probe_seconds: float,
):
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
        probe_frames=int(sample_rate * probe_seconds),
    )
    if capture_start is None:
        return {
            "alignment_found": False,
            "pass": False,
            "reason": "no exact alignment window found",
        }

    capture_frames = len(capture) // frame_bytes
    reference_frames = len(reference) // frame_bytes
    offset_frames = reference_start - capture_start
    capture_first = capture_start
    reference_first = reference_start
    overlap_frames = min(capture_frames - capture_first, reference_frames - reference_first)
    if overlap_frames <= 0:
        return {
            "alignment_found": False,
            "pass": False,
            "reason": "no overlapping frames after alignment",
        }

    min_run_frames = int(min_run_seconds * sample_rate)
    overlap_bytes = overlap_frames * frame_bytes
    ref_overlap = reference[reference_first * frame_bytes:(reference_first * frame_bytes) + overlap_bytes]
    cap_overlap = capture[capture_first * frame_bytes:(capture_first * frame_bytes) + overlap_bytes]
    exact_overlap = ref_overlap == cap_overlap

    matched_frames = overlap_frames if exact_overlap else 0

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
        "run_count": 1 if exact_overlap else 0,
        "longest_run_frames": overlap_frames if exact_overlap else 0,
        "longest_run_seconds": overlap_frames / sample_rate if exact_overlap else 0.0,
        "longest_run_start_frame": 0 if exact_overlap else None,
        "longest_run_end_frame": overlap_frames if exact_overlap else None,
        "single_contiguous_match": exact_overlap,
    }

    if overlap_frames < min_run_frames:
        result["pass"] = False
        result["reason"] = f"overlap too short ({overlap_frames} frames)"
    elif not exact_overlap:
        result["pass"] = False
        result["reason"] = "overlap differs from reference"
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
    parser.add_argument("--probe-seconds", type=float, default=30.0)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    with open(args.reference, "rb") as ref_file:
        reference = ref_file.read()
    with open(args.capture, "rb") as capture_file:
        capture = capture_file.read()

    result = analyze(
        reference,
        capture,
        args.sample_rate,
        args.channels,
        args.min_run_seconds,
        args.probe_seconds,
    )
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

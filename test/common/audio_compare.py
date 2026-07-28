#!/usr/bin/env python3
import argparse
import json
import sys


def find_window_in_reference(reference: bytes, window: bytes, frame_bytes: int):
    reference_byte = reference.find(window)
    while reference_byte != -1 and reference_byte % frame_bytes:
        reference_byte = reference.find(window, reference_byte + 1)
    return None if reference_byte == -1 else reference_byte // frame_bytes


def is_nonzero_frame(capture: bytes, frame_start: int, frame_bytes: int) -> bool:
    start_byte = frame_start * frame_bytes
    frame = capture[start_byte:start_byte + frame_bytes]
    return len(frame) == frame_bytes and any(frame)


def find_alignment(reference: bytes, capture: bytes, frame_bytes: int, window_frames: int, probe_frames: int):
    capture_frames = len(capture) // frame_bytes
    reference_frames = len(reference) // frame_bytes
    max_start = min(max(capture_frames - window_frames, 0), probe_frames)

    first_nonzero = None
    for capture_start in range(max_start + 1):
        if not is_nonzero_frame(capture, capture_start, frame_bytes):
            continue
        first_nonzero = capture_start
        break

    if first_nonzero is None:
        return None, None

    window_bytes = window_frames * frame_bytes

    # Preserve the exact earliest alignment in the common case. bytes.find()
    # performs this one whole-window lookup in optimized native code.
    first_byte = first_nonzero * frame_bytes
    first_window = capture[first_byte:first_byte + window_bytes]
    reference_start = find_window_in_reference(reference, first_window, frame_bytes)
    if reference_start is not None:
        return first_nonzero, reference_start

    # For very short candidate ranges, the sparse grid below might never cross
    # the reference grid. A bounded dense fallback keeps those captures exact.
    if max_start - first_nonzero < window_frames:
        for capture_start in range(first_nonzero + 1, max_start + 1):
            start_byte = capture_start * frame_bytes
            window = capture[start_byte:start_byte + window_bytes]
            reference_start = find_window_in_reference(
                reference,
                window,
                frame_bytes,
            )
            if reference_start is not None:
                return capture_start, reference_start
        return None, None

    # Index one frame out of every alignment-window length in the reference. Scanning capture frames
    # against this sparse index is O(reference + capture), while repeatedly
    # calling bytes.find() for every candidate becomes prohibitively expensive
    # on minute-long captures. A true alignment necessarily crosses a reference
    # grid frame; verify the full window before accepting it.
    reference_index = {}
    reference_last_start = reference_frames - window_frames
    for reference_start in range(0, reference_last_start + 1, window_frames):
        start_byte = reference_start * frame_bytes
        key = reference[start_byte:start_byte + frame_bytes]
        reference_index.setdefault(key, []).append(reference_start)

    for capture_start in range(first_nonzero, max_start + 1):
        start_byte = capture_start * frame_bytes
        key = capture[start_byte:start_byte + frame_bytes]
        for reference_start in reference_index.get(key, ()):
            reference_byte = reference_start * frame_bytes
            if (
                capture[start_byte:start_byte + window_bytes]
                == reference[reference_byte:reference_byte + window_bytes]
            ):
                # The grid may find an interior point of a longer exact run.
                # Walk back to report its actual first aligned frame.
                while capture_start > first_nonzero and reference_start > 0:
                    previous_capture = (capture_start - 1) * frame_bytes
                    previous_reference = (reference_start - 1) * frame_bytes
                    if (
                        capture[previous_capture:previous_capture + frame_bytes]
                        != reference[previous_reference:previous_reference + frame_bytes]
                    ):
                        break
                    capture_start -= 1
                    reference_start -= 1
                return capture_start, reference_start

    return None, None


def analyze(
    reference: bytes,
    capture: bytes,
    sample_rate: int,
    channels: int,
    min_run_seconds: float,
    probe_seconds: float,
    min_match_ratio: float = 0.99,
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
    if exact_overlap:
        matched_frames = overlap_frames
        run_count = 1
        longest_run_frames = overlap_frames
        longest_run_start = 0
    else:
        matched_frames = 0
        run_count = 0
        current_run_frames = 0
        current_run_start = 0
        longest_run_frames = 0
        longest_run_start = None

        for frame_index in range(overlap_frames):
            byte_start = frame_index * frame_bytes
            byte_end = byte_start + frame_bytes
            if ref_overlap[byte_start:byte_end] == cap_overlap[byte_start:byte_end]:
                matched_frames += 1
                if current_run_frames == 0:
                    current_run_start = frame_index
                    run_count += 1
                current_run_frames += 1
                if current_run_frames > longest_run_frames:
                    longest_run_frames = current_run_frames
                    longest_run_start = current_run_start
            else:
                current_run_frames = 0

    longest_run_end = (
        longest_run_start + longest_run_frames
        if longest_run_start is not None
        else None
    )

    match_ratio = (matched_frames / overlap_frames) if overlap_frames else 0.0
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
        "match_ratio": match_ratio,
        "min_match_ratio": min_match_ratio,
        "run_count": run_count,
        "longest_run_frames": longest_run_frames,
        "longest_run_seconds": longest_run_frames / sample_rate,
        "longest_run_start_frame": longest_run_start,
        "longest_run_end_frame": longest_run_end,
        "single_contiguous_match": exact_overlap,
    }

    if overlap_frames < min_run_frames:
        result["pass"] = False
        result["reason"] = f"overlap too short ({overlap_frames} frames)"
    elif longest_run_frames < min_run_frames:
        result["pass"] = False
        result["reason"] = (
            f"no bit-perfect run of at least {min_run_frames} frames"
        )
    elif match_ratio < min_match_ratio:
        result["pass"] = False
        result["reason"] = (
            f"match ratio {match_ratio:.6f} is below {min_match_ratio:.6f}"
        )
    else:
        result["pass"] = True
        result["reason"] = "bit-perfect run found"

    return result


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare capture against deterministic reference")
    parser.add_argument("--reference", required=True)
    parser.add_argument("--capture", required=True)
    parser.add_argument("--label", default="capture")
    parser.add_argument("--sample-rate", type=int, default=48000)
    parser.add_argument("--channels", type=int, default=2)
    parser.add_argument("--min-run-seconds", type=float, default=5.0)
    parser.add_argument("--min-match-ratio", type=float, default=0.99)
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
        args.min_match_ratio,
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

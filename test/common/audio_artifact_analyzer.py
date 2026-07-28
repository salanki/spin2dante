#!/usr/bin/env python3
"""Locate short insertions, drops, and mutations in an aligned PCM capture."""

import argparse
import json
import struct
import sys
from collections import Counter

from audio_compare import find_alignment


INT32_FULL_SCALE = (1 << 31) - 1


def frames_match(
    reference,
    reference_start,
    capture,
    capture_start,
    count,
    frame_bytes,
):
    reference_frames = len(reference) // frame_bytes
    capture_frames = len(capture) // frame_bytes
    if (
        reference_start + count > reference_frames
        or capture_start + count > capture_frames
    ):
        return False
    reference_byte = reference_start * frame_bytes
    capture_byte = capture_start * frame_bytes
    length = count * frame_bytes
    return (
        reference[reference_byte:reference_byte + length]
        == capture[capture_byte:capture_byte + length]
    )


def classify_insertion(capture, capture_start, count, frame_bytes):
    start_byte = capture_start * frame_bytes
    end_byte = start_byte + count * frame_bytes
    inserted = capture[start_byte:end_byte]
    if capture_start:
        previous_start = start_byte - frame_bytes
        previous_frame = capture[previous_start:start_byte]
        if inserted == previous_frame * count:
            return "duplicate"
    if not any(inserted):
        return "zero_gap"
    return "insertion"


def boundary_discontinuity(capture, boundary, frame_bytes, channels):
    capture_frames = len(capture) // frame_bytes
    if boundary <= 0 or boundary >= capture_frames:
        return 0
    frame_format = struct.Struct("<" + ("i" * channels))
    before = frame_format.unpack_from(capture, (boundary - 1) * frame_bytes)
    after = frame_format.unpack_from(capture, boundary * frame_bytes)
    return max(abs(after_sample - before_sample) for before_sample, after_sample in zip(before, after))


def find_resync(
    reference,
    capture,
    reference_pos,
    capture_pos,
    lookahead,
    resync_frames,
    frame_bytes,
):
    direction_priority = {"drop": 0, "insertion": 1, "mutation": 2}
    for skipped in range(1, lookahead + 1):
        candidates = []
        if frames_match(
            reference,
            reference_pos,
            capture,
            capture_pos + skipped,
            resync_frames,
            frame_bytes,
        ):
            candidates.append((skipped, "insertion"))
        if frames_match(
            reference,
            reference_pos + skipped,
            capture,
            capture_pos,
            resync_frames,
            frame_bytes,
        ):
            candidates.append((skipped, "drop"))
        if frames_match(
            reference,
            reference_pos + skipped,
            capture,
            capture_pos + skipped,
            resync_frames,
            frame_bytes,
        ):
            candidates.append((skipped, "mutation"))

        if candidates:
            return min(
                candidates,
                key=lambda candidate: direction_priority[candidate[1]],
            )
    return None


def find_distant_resync(
    reference,
    capture,
    reference_pos,
    capture_pos,
    frame_bytes,
    resync_frames,
    probe_frames,
):
    proof_frames = max(resync_frames, 64)
    proof_bytes = proof_frames * frame_bytes
    reference_byte = reference_pos * frame_bytes
    capture_byte = capture_pos * frame_bytes
    reference_window = reference[reference_byte:reference_byte + proof_bytes]
    capture_window = capture[capture_byte:capture_byte + proof_bytes]

    candidates = []
    if len(capture_window) == proof_bytes:
        search_end = min(
            len(reference),
            reference_byte + (probe_frames + proof_frames) * frame_bytes,
        )
        match = reference.find(
            capture_window,
            reference_byte + frame_bytes,
            search_end,
        )
        while match != -1 and match % frame_bytes:
            match = reference.find(capture_window, match + 1, search_end)
        if match != -1:
            candidates.append(((match - reference_byte) // frame_bytes, 0))

    if len(reference_window) == proof_bytes:
        search_end = min(
            len(capture),
            capture_byte + (probe_frames + proof_frames) * frame_bytes,
        )
        match = capture.find(
            reference_window,
            capture_byte + frame_bytes,
            search_end,
        )
        while match != -1 and match % frame_bytes:
            match = capture.find(reference_window, match + 1, search_end)
        if match != -1:
            candidates.append((0, (match - capture_byte) // frame_bytes))

    if candidates:
        return min(candidates, key=lambda candidate: max(candidate))

    # A long capture-path mutation often preserves timeline length: both sides
    # become bit-exact again at the same offset. Check that common desync shape
    # directly before rebuilding the general sparse alignment index.
    reference_frames = len(reference) // frame_bytes
    capture_frames = len(capture) // frame_bytes
    max_same_offset_skip = min(
        probe_frames,
        reference_frames - reference_pos - proof_frames,
        capture_frames - capture_pos - proof_frames,
    )
    for skipped in range(1, max_same_offset_skip + 1):
        if frames_match(
            reference,
            reference_pos + skipped,
            capture,
            capture_pos + skipped,
            proof_frames,
            frame_bytes,
        ):
            return skipped, skipped

    capture_skip, reference_skip = find_alignment(
        reference=reference[reference_pos * frame_bytes:],
        capture=capture[capture_pos * frame_bytes:],
        frame_bytes=frame_bytes,
        window_frames=proof_frames,
        probe_frames=probe_frames,
    )
    if capture_skip is None or (capture_skip == 0 and reference_skip == 0):
        return None
    return reference_skip, capture_skip


def coalesce_mutations(events):
    coalesced = []
    for event in events:
        if (
            coalesced
            and event["kind"] == "mutation"
            and coalesced[-1]["kind"] == "mutation"
            and event["capture_frame"]
            == coalesced[-1]["capture_frame"] + coalesced[-1]["frames"]
            and event["reference_frame"]
            == coalesced[-1]["reference_frame"] + coalesced[-1]["frames"]
        ):
            coalesced[-1]["frames"] += event["frames"]
            coalesced[-1]["peak_discontinuity"] = max(
                coalesced[-1]["peak_discontinuity"],
                event["peak_discontinuity"],
            )
        else:
            coalesced.append(event)
    return coalesced


def analyze_artifacts(
    reference_bytes: bytes,
    capture_bytes: bytes,
    sample_rate: int = 48000,
    channels: int = 2,
    probe_seconds: float = 30.0,
    lookahead_frames: int = 64,
    resync_frames: int = 8,
    allowed_event_frames: int = 1,
):
    frame_bytes = channels * 4
    if len(reference_bytes) % frame_bytes:
        raise ValueError(
            f"reference length is not a whole number of {frame_bytes}-byte frames"
        )
    # Capture files may be snapshotted while inferno2pipe is between sample
    # writes. A partial trailing frame has no analyzable audio information.
    capture_bytes = capture_bytes[:len(capture_bytes) // frame_bytes * frame_bytes]

    capture_start, reference_start = find_alignment(
        reference=reference_bytes,
        capture=capture_bytes,
        frame_bytes=frame_bytes,
        window_frames=64,
        probe_frames=int(sample_rate * probe_seconds),
    )
    if capture_start is None:
        return {
            "alignment_found": False,
            "quality_pass": False,
            "reason": "no exact alignment window found",
            "events": [],
        }

    reference_frames = len(reference_bytes) // frame_bytes
    capture_frames = len(capture_bytes) // frame_bytes
    reference_pos = reference_start
    capture_pos = capture_start
    events = []

    while reference_pos < reference_frames and capture_pos < capture_frames:
        reference_byte = reference_pos * frame_bytes
        capture_byte = capture_pos * frame_bytes
        if (
            reference_bytes[reference_byte:reference_byte + frame_bytes]
            == capture_bytes[capture_byte:capture_byte + frame_bytes]
        ):
            reference_pos += 1
            capture_pos += 1
            continue

        resync = find_resync(
            reference_bytes,
            capture_bytes,
            reference_pos,
            capture_pos,
            lookahead_frames,
            resync_frames,
            frame_bytes,
        )
        if resync is None:
            distant = find_distant_resync(
                reference_bytes,
                capture_bytes,
                reference_pos,
                capture_pos,
                frame_bytes,
                resync_frames,
                int(sample_rate * probe_seconds),
            )
            if distant is None:
                event_frames = 1
                kind = "mutation"
                next_reference_pos = reference_pos + 1
                next_capture_pos = capture_pos + 1
            else:
                reference_skip, capture_skip = distant
                next_reference_pos = reference_pos + reference_skip
                next_capture_pos = capture_pos + capture_skip
                if reference_skip > 0 and capture_skip == 0:
                    event_frames = reference_skip
                    kind = "drop"
                elif capture_skip > 0 and reference_skip == 0:
                    event_frames = capture_skip
                    kind = classify_insertion(
                        capture_bytes,
                        capture_pos,
                        capture_skip,
                        frame_bytes,
                    )
                else:
                    event_frames = max(reference_skip, capture_skip)
                    kind = "desync"
        else:
            event_frames, direction = resync
            if direction == "insertion":
                kind = classify_insertion(
                    capture_bytes,
                    capture_pos,
                    event_frames,
                    frame_bytes,
                )
                next_reference_pos = reference_pos
                next_capture_pos = capture_pos + event_frames
            else:
                kind = direction
                next_reference_pos = reference_pos + event_frames
                next_capture_pos = (
                    capture_pos + event_frames
                    if direction == "mutation"
                    else capture_pos
                )

        peak = max(
            boundary_discontinuity(
                capture_bytes,
                capture_pos,
                frame_bytes,
                channels,
            ),
            boundary_discontinuity(
                capture_bytes,
                next_capture_pos,
                frame_bytes,
                channels,
            ),
        )
        events.append(
            {
                "kind": kind,
                "capture_frame": capture_pos,
                "reference_frame": reference_pos,
                "frames": event_frames,
                "duration_ms": event_frames * 1000.0 / sample_rate,
                "peak_discontinuity": peak / INT32_FULL_SCALE,
            }
        )
        reference_pos = next_reference_pos
        capture_pos = next_capture_pos

    events = coalesce_mutations(events)
    for event in events:
        event["duration_ms"] = event["frames"] * 1000.0 / sample_rate
    counts = Counter()
    for event in events:
        counts[event["kind"]] += event["frames"]

    max_event_frames = max((event["frames"] for event in events), default=0)
    peak_discontinuity = max(
        (event["peak_discontinuity"] for event in events),
        default=0.0,
    )
    disallowed_event = any(
        event["kind"] not in {"duplicate", "drop"}
        or event["frames"] > allowed_event_frames
        for event in events
    )

    return {
        "alignment_found": True,
        "capture_start_frame": capture_start,
        "reference_start_frame": reference_start,
        "offset_frames": reference_start - capture_start,
        "offset_ms": (reference_start - capture_start) * 1000.0 / sample_rate,
        "analyzed_reference_frames": reference_pos - reference_start,
        "analyzed_capture_frames": capture_pos - capture_start,
        "event_count": len(events),
        "event_frames_by_kind": dict(sorted(counts.items())),
        "max_event_frames": max_event_frames,
        "peak_discontinuity": peak_discontinuity,
        "allowed_event_frames": allowed_event_frames,
        "quality_pass": not disallowed_event,
        "reason": (
            "no disallowed artifacts found"
            if not disallowed_event
            else "capture contains an unsafe timing artifact or oversized correction"
        ),
        "events": events,
    }


def main():
    parser = argparse.ArgumentParser(
        description="Classify local timing corrections and discontinuities in PCM audio"
    )
    parser.add_argument("--reference", required=True)
    parser.add_argument("--capture", required=True)
    parser.add_argument("--sample-rate", type=int, default=48000)
    parser.add_argument("--channels", type=int, default=2)
    parser.add_argument("--probe-seconds", type=float, default=30.0)
    parser.add_argument("--lookahead-frames", type=int, default=64)
    parser.add_argument("--resync-frames", type=int, default=8)
    parser.add_argument("--allowed-event-frames", type=int, default=1)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    with open(args.reference, "rb") as reference_file:
        reference = reference_file.read()
    with open(args.capture, "rb") as capture_file:
        capture = capture_file.read()

    result = analyze_artifacts(
        reference,
        capture,
        sample_rate=args.sample_rate,
        channels=args.channels,
        probe_seconds=args.probe_seconds,
        lookahead_frames=args.lookahead_frames,
        resync_frames=args.resync_frames,
        allowed_event_frames=args.allowed_event_frames,
    )
    if args.json:
        json.dump(result, sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        print(
            f"alignment_found={result['alignment_found']} "
            f"quality_pass={result['quality_pass']}"
        )
        if result["alignment_found"]:
            print(
                f"events={result['event_count']} "
                f"frames_by_kind={result['event_frames_by_kind']} "
                f"max_event_frames={result['max_event_frames']} "
                f"peak_discontinuity={result['peak_discontinuity']:.6f}"
            )
        print(result["reason"])

    return 0 if result["quality_pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Locate short insertions, drops, and mutations in an aligned PCM capture."""

import argparse
import json
import struct
import sys
from collections import Counter

from audio_compare import find_alignment


INT32_FULL_SCALE = (1 << 31) - 1


def decode_frames(audio: bytes, channels: int):
    frame_bytes = channels * 4
    if len(audio) % frame_bytes:
        raise ValueError(
            f"audio length {len(audio)} is not a whole number of {frame_bytes}-byte frames"
        )

    frame_format = struct.Struct("<" + ("i" * channels))
    return [
        frame_format.unpack_from(audio, offset)
        for offset in range(0, len(audio), frame_bytes)
    ]


def frames_match(reference, reference_start, capture, capture_start, count):
    if (
        reference_start + count > len(reference)
        or capture_start + count > len(capture)
    ):
        return False
    return (
        reference[reference_start:reference_start + count]
        == capture[capture_start:capture_start + count]
    )


def classify_insertion(inserted, previous_frame):
    zero_frame = (0,) * len(inserted[0])
    if all(frame == zero_frame for frame in inserted):
        return "zero_gap"
    if previous_frame is not None and all(frame == previous_frame for frame in inserted):
        return "duplicate"
    return "insertion"


def boundary_discontinuity(capture, boundary):
    if boundary <= 0 or boundary >= len(capture):
        return 0
    return max(
        abs(after - before)
        for before, after in zip(capture[boundary - 1], capture[boundary])
    )


def find_resync(reference, capture, reference_pos, capture_pos, lookahead, resync_frames):
    candidates = []
    for skipped in range(1, lookahead + 1):
        if frames_match(
            reference,
            reference_pos,
            capture,
            capture_pos + skipped,
            resync_frames,
        ):
            candidates.append((skipped, "insertion"))
        if frames_match(
            reference,
            reference_pos + skipped,
            capture,
            capture_pos,
            resync_frames,
        ):
            candidates.append((skipped, "drop"))

    if not candidates:
        return None
    return min(candidates, key=lambda candidate: (candidate[0], candidate[1]))


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

    reference = decode_frames(reference_bytes, channels)
    capture = decode_frames(capture_bytes, channels)
    reference_pos = reference_start
    capture_pos = capture_start
    events = []

    while reference_pos < len(reference) and capture_pos < len(capture):
        if reference[reference_pos] == capture[capture_pos]:
            reference_pos += 1
            capture_pos += 1
            continue

        resync = find_resync(
            reference,
            capture,
            reference_pos,
            capture_pos,
            lookahead_frames,
            resync_frames,
        )
        if resync is None:
            event_frames = 1
            kind = "mutation"
            next_reference_pos = reference_pos + 1
            next_capture_pos = capture_pos + 1
        else:
            event_frames, direction = resync
            if direction == "insertion":
                kind = classify_insertion(
                    capture[capture_pos:capture_pos + event_frames],
                    capture[capture_pos - 1] if capture_pos else None,
                )
                next_reference_pos = reference_pos
                next_capture_pos = capture_pos + event_frames
            else:
                kind = "drop"
                next_reference_pos = reference_pos + event_frames
                next_capture_pos = capture_pos

        peak = max(
            boundary_discontinuity(capture, capture_pos),
            boundary_discontinuity(capture, next_capture_pos),
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
            else "capture contains an unsafe insertion, mutation, or oversized correction"
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

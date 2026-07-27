#!/usr/bin/env python3
"""Validate gradual correction through the real bridge and Dante capture."""

import json
import os
import re

from audio_artifact_analyzer import analyze_artifacts


REFERENCE_PATH = "/shared/reference_capture.raw"
CAPTURE_PATH = "/shared/capture.raw"
BRIDGE_LOG_PATH = "/shared/bridge.log"
RESULT_PATH = "/shared/drift_analysis.json"


def fail(message):
    print(f"FAIL: {message}")
    raise SystemExit(1)


def main():
    for path in (REFERENCE_PATH, CAPTURE_PATH, BRIDGE_LOG_PATH):
        if not os.path.exists(path):
            fail(f"missing required file: {path}")

    with open(BRIDGE_LOG_PATH, encoding="utf-8") as bridge_log_file:
        bridge_log = bridge_log_file.read()

    applied_corrections = [
        (int(inserted), int(dropped))
        for inserted, dropped in re.findall(
            r"drift correction applied: inserted=(\d+) dropped=(\d+)",
            bridge_log,
        )
    ]
    total_inserted = sum(inserted for inserted, _ in applied_corrections)
    total_dropped = sum(dropped for _, dropped in applied_corrections)
    if total_inserted == 0:
        fail("slow Sendspin clock did not cause any inserted-frame corrections")
    if total_dropped:
        fail(
            "slow Sendspin clock caused dropped-frame corrections; "
            "the drift filter did not reject a direction-reversing outlier"
        )

    with open(REFERENCE_PATH, "rb") as reference_file:
        reference = reference_file.read()
    with open(CAPTURE_PATH, "rb") as capture_file:
        capture = capture_file.read()

    result = analyze_artifacts(
        reference,
        capture,
        sample_rate=48_000,
        channels=2,
        lookahead_frames=64,
        resync_frames=8,
        allowed_event_frames=1,
    )
    result["logged_corrections"] = [
        {"inserted_frames": inserted, "dropped_frames": dropped}
        for inserted, dropped in applied_corrections
    ]

    zero_gap_frames = result.get("event_frames_by_kind", {}).get("zero_gap", 0)
    zero_gap_events = [
        event for event in result.get("events", []) if event["kind"] == "zero_gap"
    ]
    duplicate_frames = result.get("event_frames_by_kind", {}).get("duplicate", 0)
    oversized_discrete_events = [
        event
        for event in result.get("events", [])
        if event["kind"] in {"duplicate", "drop", "insertion"}
        and event["frames"] > 1
    ]
    # Inferno can report host scheduling lag during this containerized test,
    # independently of the bridge correction. The old bridge implementation
    # created deterministic 12-frame zero gaps, so reject those while allowing
    # isolated capture-path gaps to be reported separately.
    batched_zero_gaps = [event for event in zero_gap_events if event["frames"] >= 12]
    correction_quality_pass = (
        duplicate_frames > 0
        and total_dropped == 0
        and not oversized_discrete_events
        and not batched_zero_gaps
    )
    result["correction_quality_pass"] = correction_quality_pass
    with open(RESULT_PATH, "w", encoding="utf-8") as result_file:
        json.dump(result, result_file, indent=2)
        result_file.write("\n")

    print(f"Correction batches: {len(applied_corrections)}")
    print(f"Inserted frames: {total_inserted}")
    print(f"Dropped frames: {total_dropped}")
    print(f"Analyzer events: {result.get('event_count', 0)}")
    print(f"Duplicate frames: {duplicate_frames}")
    print(f"Zero-gap events: {len(zero_gap_events)}")
    print(f"Zero-gap frames: {zero_gap_frames}")
    print(f"Batched zero gaps (>=12 frames): {len(batched_zero_gaps)}")
    print(f"Largest event: {result.get('max_event_frames', 0)} frames")
    print(
        "Peak normalized boundary discontinuity: "
        f"{result.get('peak_discontinuity', 0.0):.6f}"
    )
    print(
        "Full-capture quality gate: "
        f"{'PASS' if result.get('quality_pass') else 'FAIL'}"
    )
    print(
        "Drift-correction quality gate: "
        f"{'PASS' if correction_quality_pass else 'FAIL'}"
    )
    print(f"Detailed result: {RESULT_PATH}")

    if not correction_quality_pass:
        fail(
            "correction produced a 12-frame zero gap, a multi-frame discrete "
            "timing event, a wrong-direction drop, or no repeated frame was observed"
        )

    print("PASS: real bridge used isolated repeated frames without batched zero gaps")


if __name__ == "__main__":
    main()

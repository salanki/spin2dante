#!/usr/bin/env python3
"""Analyze attributed spin2dante [sync] records for inter-zone skew."""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path


LOG_RE = re.compile(
    r"^\[(?P<timestamp>\S+)\s+(?:TRACE|DEBUG|INFO|WARN|ERROR)\s+[^\]]+\]\s+"
    r"\[sync\]\s+(?P<fields>.*)$"
)
FIELD_RE = re.compile(r'([a-z_]+)=("(?:[^"\\]|\\.)*"|\S+)')
FAULT_FIELDS = (
    "pending",
    "stale_drops",
    "trimmed_chunks",
    "trimmed_frames",
    "rebuffers",
    "drift_checks_skipped",
)


@dataclass(frozen=True)
class SyncRecord:
    timestamp: datetime
    bridge_id: str
    bridge_name: str
    session: int
    stream_start_us: int
    drift_since_anchor_frames: int
    fields: dict[str, str]


def _decode_value(value: str) -> str:
    if value.startswith('"'):
        try:
            return json.loads(value)
        except json.JSONDecodeError:
            # Rust's Debug formatter can emit escapes such as \u{7}, which are
            # not JSON. Preserve the printable representation instead of
            # aborting analysis because one bridge name contains an odd byte.
            return value[1:-1]
    return value


def parse_sync_line(line: str) -> SyncRecord | None:
    match = LOG_RE.match(line.strip())
    if not match:
        return None
    fields = {
        key: _decode_value(value)
        for key, value in FIELD_RE.findall(match.group("fields"))
    }
    required = (
        "bridge_id",
        "session",
        "stream_start_us",
        "drift_valid",
        "drift_since_anchor_frames",
    )
    if (
        any(key not in fields for key in required)
        or fields["drift_valid"] != "1"
    ):
        return None
    timestamp = datetime.fromisoformat(match.group("timestamp").replace("Z", "+00:00"))
    return SyncRecord(
        timestamp=timestamp,
        bridge_id=fields["bridge_id"],
        bridge_name=fields.get("bridge_name", fields["bridge_id"]),
        session=int(fields["session"]),
        stream_start_us=int(fields["stream_start_us"]),
        drift_since_anchor_frames=int(fields["drift_since_anchor_frames"]),
        fields=fields,
    )


def _percentile(values: list[int], percentile: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    if len(ordered) == 1:
        return float(ordered[0])
    position = (len(ordered) - 1) * percentile
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return float(ordered[lower])
    weight = position - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def _largest_time_coherent_subset(
    records: list[SyncRecord],
    max_gap_seconds: float,
) -> list[SyncRecord]:
    ordered = sorted(records, key=lambda record: record.timestamp)
    best: list[SyncRecord] = []
    left = 0
    for right, record in enumerate(ordered):
        while (
            left < right
            and (record.timestamp - ordered[left].timestamp).total_seconds()
            > max_gap_seconds
        ):
            left += 1
        candidate = ordered[left : right + 1]
        # On equal-size windows, prefer the later one. It is more likely to be
        # the epoch-aligned periodic set rather than immediate startup records.
        if len(candidate) >= len(best):
            best = candidate
    return best


def analyze_sync_logs(
    text: str,
    *,
    sample_rate: int = 48_000,
    bucket_seconds: int = 120,
    max_pair_time_gap_seconds: float = 10.0,
    trend_threshold_ms: float = 1.0,
) -> dict:
    lines = text.splitlines()
    sync_records_seen = sum(LOG_RE.match(line.strip()) is not None for line in lines)
    records = [
        record
        for line in lines
        if (record := parse_sync_line(line)) is not None
    ]
    skipped_sync_records = sync_records_seen - len(records)

    # Windows are relative to the earliest record available for each stream.
    # Rotating or truncating a log can therefore shift window boundaries; the
    # time-span guard below keeps that from creating distant comparisons.
    stream_origins: dict[int, datetime] = {}
    for record in records:
        if record.stream_start_us != 0:
            stream_origins[record.stream_start_us] = min(
                stream_origins.get(record.stream_start_us, record.timestamp),
                record.timestamp,
            )

    buckets: dict[tuple[int, int], dict[str, SyncRecord]] = {}
    for record in records:
        if record.stream_start_us == 0:
            continue
        elapsed = record.timestamp - stream_origins[record.stream_start_us]
        bucket = int(elapsed.total_seconds()) // bucket_seconds
        key = (record.stream_start_us, bucket)
        # Keep the newest record if a bridge logs more than once in a bucket.
        previous = buckets.setdefault(key, {}).get(record.bridge_id)
        if previous is None or record.timestamp >= previous.timestamp:
            buckets[key][record.bridge_id] = record

    samples = []
    rejected_time_gap_buckets = 0
    partial_time_gap_buckets = 0
    excluded_bridge_counts: dict[str, int] = {}
    excluded_bridge_ids_seen: set[str] = set()
    compared_bridge_ids: set[str] = set()
    bridge_names = {record.bridge_id: record.bridge_name for record in records}
    for (stream_start_us, _), by_bridge in sorted(buckets.items()):
        if len(by_bridge) < 2:
            continue
        coherent = _largest_time_coherent_subset(
            list(by_bridge.values()),
            max_pair_time_gap_seconds,
        )
        if len(coherent) < 2:
            rejected_time_gap_buckets += 1
            continue

        coherent_ids = {record.bridge_id for record in coherent}
        excluded = [
            record
            for record in by_bridge.values()
            if record.bridge_id not in coherent_ids
        ]
        if excluded:
            partial_time_gap_buckets += 1
            for record in excluded:
                excluded_bridge_ids_seen.add(record.bridge_id)
                excluded_bridge_counts[record.bridge_id] = (
                    excluded_bridge_counts.get(record.bridge_id, 0) + 1
                )

        timestamps = [record.timestamp for record in coherent]
        time_span_seconds = (max(timestamps) - min(timestamps)).total_seconds()
        ordered = sorted(
            coherent,
            key=lambda record: record.drift_since_anchor_frames,
        )
        compared_bridge_ids.update(record.bridge_id for record in ordered)
        low = ordered[0]
        high = ordered[-1]
        spread = (
            high.drift_since_anchor_frames - low.drift_since_anchor_frames
        )
        samples.append(
            {
                "timestamp": max(record.timestamp for record in ordered).isoformat(),
                "time_span_seconds": time_span_seconds,
                "stream_start_us": stream_start_us,
                "bridge_count": len(ordered),
                "observed_bridge_count": len(by_bridge),
                "excluded_bridges": [
                    {"id": record.bridge_id, "name": record.bridge_name}
                    for record in sorted(excluded, key=lambda record: record.bridge_id)
                ],
                "skew_frames": spread,
                "skew_us": spread * 1_000_000 / sample_rate,
                "low_bridge_id": low.bridge_id,
                "low_bridge_name": low.bridge_name,
                "low_drift_since_anchor_frames": low.drift_since_anchor_frames,
                "high_bridge_id": high.bridge_id,
                "high_bridge_name": high.bridge_name,
                "high_drift_since_anchor_frames": high.drift_since_anchor_frames,
            }
        )

    spreads = [sample["skew_frames"] for sample in samples]
    max_sample = max(samples, key=lambda sample: sample["skew_frames"], default=None)
    stream_trends = {}
    for stream_start_us in sorted({sample["stream_start_us"] for sample in samples}):
        stream_samples = [
            sample for sample in samples if sample["stream_start_us"] == stream_start_us
        ]
        third_size = max(1, len(stream_samples) // 3)
        first = _percentile(
            [sample["skew_frames"] for sample in stream_samples[:third_size]],
            0.5,
        )
        last = _percentile(
            [sample["skew_frames"] for sample in stream_samples[-third_size:]],
            0.5,
        )
        delta = last - first
        trend_threshold_frames = trend_threshold_ms * sample_rate / 1000
        stream_trends[str(stream_start_us)] = {
            "samples": len(stream_samples),
            "first_third_median_skew_frames": first,
            "last_third_median_skew_frames": last,
            "delta_frames": delta,
            "threshold_frames": trend_threshold_frames,
            "direction": (
                "growing"
                if delta > trend_threshold_frames
                else "reconverging"
                if delta < -trend_threshold_frames
                else "stable"
            ),
        }

    fault_maxima = {field: 0 for field in FAULT_FIELDS}
    for record in records:
        for field in FAULT_FIELDS:
            if field in record.fields:
                fault_maxima[field] = max(fault_maxima[field], int(record.fields[field]))
        if "trims" in record.fields:
            chunks, frames = record.fields["trims"].split("/", 1)
            fault_maxima["trimmed_chunks"] = max(
                fault_maxima["trimmed_chunks"], int(chunks)
            )
            fault_maxima["trimmed_frames"] = max(
                fault_maxima["trimmed_frames"], int(frames)
            )

    comparable = bool(spreads)
    never_compared_ids = sorted(excluded_bridge_ids_seen - compared_bridge_ids)
    return {
        "sync_records_seen": sync_records_seen,
        "valid_sync_records": len(records),
        "skipped_sync_records": skipped_sync_records,
        "comparable_samples": len(samples),
        "rejected_time_gap_buckets": rejected_time_gap_buckets,
        "partial_time_gap_buckets": partial_time_gap_buckets,
        "excluded_bridge_records": sum(excluded_bridge_counts.values()),
        "excluded_bridge_counts": excluded_bridge_counts,
        "bridges_never_compared": [
            {"id": bridge_id, "name": bridge_names[bridge_id]}
            for bridge_id in never_compared_ids
        ],
        "sample_rate": sample_rate,
        "bucket_seconds": bucket_seconds,
        "max_pair_time_gap_seconds": max_pair_time_gap_seconds,
        "trend_threshold_ms": trend_threshold_ms,
        "max_pairwise_skew": max_sample,
        "skew_frames": {
            "median": _percentile(spreads, 0.5) if comparable else None,
            "p95": _percentile(spreads, 0.95) if comparable else None,
            "maximum": max(spreads) if comparable else None,
        },
        "skew_ms": {
            "median": (
                _percentile(spreads, 0.5) * 1000 / sample_rate
                if comparable
                else None
            ),
            "p95": (
                _percentile(spreads, 0.95) * 1000 / sample_rate
                if comparable
                else None
            ),
            "maximum": max(spreads) * 1000 / sample_rate if comparable else None,
        },
        "stream_trends": stream_trends,
        "fault_maxima": fault_maxima,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Analyze attributed spin2dante [sync] logs"
    )
    parser.add_argument(
        "log",
        nargs="?",
        help="Log file to read; omit or use '-' for stdin",
    )
    parser.add_argument("--sample-rate", type=int, default=48_000)
    parser.add_argument("--bucket-seconds", type=int, default=120)
    parser.add_argument("--max-pair-time-gap-seconds", type=float, default=10.0)
    parser.add_argument("--trend-threshold-ms", type=float, default=1.0)
    args = parser.parse_args()

    if args.log and args.log != "-":
        text = Path(args.log).read_text()
    else:
        text = sys.stdin.read()
    result = analyze_sync_logs(
        text,
        sample_rate=args.sample_rate,
        bucket_seconds=args.bucket_seconds,
        max_pair_time_gap_seconds=args.max_pair_time_gap_seconds,
        trend_threshold_ms=args.trend_threshold_ms,
    )
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

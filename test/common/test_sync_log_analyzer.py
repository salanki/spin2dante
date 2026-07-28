import unittest

from sync_log_analyzer import analyze_sync_logs, parse_sync_line


def sync_line(
    timestamp,
    bridge_id,
    offset,
    *,
    stream_start_us=1_000_000,
    session=1,
    stale_drops=0,
    rebuffers=0,
):
    return (
        f'[{timestamp} INFO  spin2dante::bridge] [sync] '
        f'bridge_id={bridge_id} bridge_name="{bridge_id} zone" '
        f"client_id=client-{bridge_id} session={session} "
        f"stream_start_us={stream_start_us} mode=scheduled "
        f"drift_valid=1 drift_since_anchor_frames={offset} "
        f"drift_since_anchor_us=0 raw_drift_since_anchor_frames={offset} "
        f"anchor_correction_frames=0 pending=0 "
        f"stale_drops={stale_drops} trims=0/0 high_water=1 "
        f"drift_corrections=0 drift_inserted_frames=0 "
        f"drift_dropped_frames=0 rebuffers={rebuffers} drift_checks_skipped=0"
    )


class SyncLogAnalyzerTest(unittest.TestCase):
    def test_parser_handles_quoted_zone_name(self):
        record = parse_sync_line(
            sync_line("2026-07-28T16:00:00Z", "livingroom", -72)
        )

        self.assertEqual(record.bridge_id, "livingroom")
        self.assertEqual(record.bridge_name, "livingroom zone")
        self.assertEqual(record.drift_since_anchor_frames, -72)

    def test_reports_largest_pair_and_percentiles(self):
        lines = [
            sync_line("2026-07-28T16:00:00Z", "livingroom", -70),
            sync_line("2026-07-28T16:00:01Z", "office", -60),
            sync_line("2026-07-28T16:00:02Z", "pooldeck", -50),
            sync_line("2026-07-28T16:02:00Z", "livingroom", -130),
            sync_line("2026-07-28T16:02:01Z", "office", -55),
            sync_line("2026-07-28T16:02:02Z", "pooldeck", -40),
        ]

        result = analyze_sync_logs("\n".join(lines))

        self.assertEqual(result["comparable_samples"], 2)
        self.assertEqual(result["skew_frames"]["maximum"], 90)
        self.assertAlmostEqual(result["skew_ms"]["maximum"], 1.875)
        self.assertEqual(
            result["max_pairwise_skew"]["low_bridge_id"], "livingroom"
        )
        self.assertEqual(result["max_pairwise_skew"]["high_bridge_id"], "pooldeck")
        self.assertEqual(
            result["stream_trends"]["1000000"]["direction"], "growing"
        )

    def test_does_not_compare_different_stream_timestamps(self):
        lines = [
            sync_line(
                "2026-07-28T16:00:00Z",
                "livingroom",
                -100,
                stream_start_us=1_000_000,
            ),
            sync_line(
                "2026-07-28T16:00:01Z",
                "office",
                100,
                stream_start_us=2_000_000,
            ),
        ]

        result = analyze_sync_logs("\n".join(lines))

        self.assertEqual(result["comparable_samples"], 0)
        self.assertIsNone(result["max_pairwise_skew"])
        self.assertIsNone(result["skew_frames"]["maximum"])
        self.assertIsNone(result["skew_ms"]["maximum"])

    def test_keeps_latest_bridge_record_within_bucket(self):
        lines = [
            sync_line("2026-07-28T16:00:00Z", "livingroom", -100),
            sync_line("2026-07-28T16:00:02Z", "livingroom", -50),
            sync_line("2026-07-28T16:00:01Z", "office", -40),
        ]

        result = analyze_sync_logs("\n".join(lines))

        self.assertEqual(result["max_pairwise_skew"]["skew_frames"], 10)

    def test_default_windows_compare_dephased_120_second_records(self):
        lines = []
        for cycle in range(3):
            base_minute = cycle * 2
            lines.extend(
                [
                    sync_line(
                        f"2026-07-28T16:{base_minute:02d}:58Z",
                        "livingroom",
                        -100,
                    ),
                    sync_line(
                        f"2026-07-28T16:{base_minute + 1:02d}:05Z",
                        "office",
                        20,
                    ),
                ]
            )

        result = analyze_sync_logs("\n".join(lines))

        self.assertEqual(result["bucket_seconds"], 120)
        self.assertEqual(result["comparable_samples"], 3)
        self.assertEqual(result["skew_frames"]["maximum"], 120)
        self.assertLessEqual(
            result["max_pairwise_skew"]["time_span_seconds"], 10
        )

    def test_rejects_records_too_far_apart_in_same_window(self):
        lines = [
            sync_line("2026-07-28T16:00:00Z", "livingroom", 0),
            sync_line("2026-07-28T16:01:53Z", "office", 1_130),
        ]

        result = analyze_sync_logs("\n".join(lines))

        self.assertEqual(result["comparable_samples"], 0)
        self.assertEqual(result["rejected_time_gap_buckets"], 1)
        self.assertIsNone(result["skew_ms"]["maximum"])

    def test_counts_invalid_sync_records_separately(self):
        valid = sync_line("2026-07-28T16:00:00Z", "livingroom", -72)
        invalid = sync_line("2026-07-28T16:00:05Z", "office", -70).replace(
            "drift_valid=1", "drift_valid=0"
        )

        result = analyze_sync_logs("\n".join([valid, invalid]))

        self.assertEqual(result["sync_records_seen"], 2)
        self.assertEqual(result["valid_sync_records"], 1)
        self.assertEqual(result["skipped_sync_records"], 1)

    def test_rust_debug_only_escape_does_not_abort_parser(self):
        line = sync_line("2026-07-28T16:00:00Z", "livingroom", -72).replace(
            '"livingroom zone"', '"Living\\u{7}Room"'
        )

        record = parse_sync_line(line)

        self.assertEqual(record.bridge_name, r"Living\u{7}Room")

    def test_reports_fault_counter_maxima(self):
        lines = [
            sync_line(
                "2026-07-28T16:00:00Z",
                "livingroom",
                -70,
                stale_drops=2,
            ),
            sync_line(
                "2026-07-28T16:00:01Z",
                "office",
                -60,
                rebuffers=1,
            ),
        ]

        result = analyze_sync_logs("\n".join(lines))

        self.assertEqual(result["fault_maxima"]["stale_drops"], 2)
        self.assertEqual(result["fault_maxima"]["rebuffers"], 1)


if __name__ == "__main__":
    unittest.main()

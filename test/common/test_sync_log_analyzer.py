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
        f"stream_start_us={stream_start_us} mode=scheduled drift_valid=1 "
        f"timeline_offset_frames={offset} timeline_offset_us=0 "
        f"raw_offset_frames={offset} anchor_correction_frames=0 pending=0 "
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
        self.assertEqual(record.timeline_offset_frames, -72)

    def test_reports_largest_pair_and_percentiles(self):
        lines = [
            sync_line("2026-07-28T16:00:00Z", "livingroom", -70),
            sync_line("2026-07-28T16:00:01Z", "office", -60),
            sync_line("2026-07-28T16:00:02Z", "pooldeck", -50),
            sync_line("2026-07-28T16:00:05Z", "livingroom", -100),
            sync_line("2026-07-28T16:00:06Z", "office", -55),
            sync_line("2026-07-28T16:00:07Z", "pooldeck", -40),
        ]

        result = analyze_sync_logs("\n".join(lines))

        self.assertEqual(result["comparable_samples"], 2)
        self.assertEqual(result["skew_frames"]["maximum"], 60)
        self.assertAlmostEqual(result["skew_ms"]["maximum"], 1.25)
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

    def test_keeps_latest_bridge_record_within_bucket(self):
        lines = [
            sync_line("2026-07-28T16:00:00Z", "livingroom", -100),
            sync_line("2026-07-28T16:00:02Z", "livingroom", -50),
            sync_line("2026-07-28T16:00:01Z", "office", -40),
        ]

        result = analyze_sync_logs("\n".join(lines))

        self.assertEqual(result["max_pairwise_skew"]["skew_frames"], 10)

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

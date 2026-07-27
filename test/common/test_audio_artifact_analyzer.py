import struct
import unittest

from audio_artifact_analyzer import analyze_artifacts


def encode(frames):
    return b"".join(struct.pack("<ii", *frame) for frame in frames)


def signal(frame_count=256):
    # Unique, non-zero stereo frames make local insertions and drops unambiguous.
    return [
        (
            100_000_000 + frame * 1_000_003,
            -200_000_000 - frame * 1_000_033,
        )
        for frame in range(frame_count)
    ]


class ArtifactAnalyzerTest(unittest.TestCase):
    def analyze(self, reference, capture, **kwargs):
        return analyze_artifacts(
            encode(reference),
            encode(capture),
            sample_rate=48000,
            channels=2,
            **kwargs,
        )

    def test_bit_perfect_capture_has_no_events(self):
        reference = signal()

        result = self.analyze(reference, reference)

        self.assertTrue(result["quality_pass"])
        self.assertEqual(result["events"], [])

    def test_alignment_skips_capture_startup_silence(self):
        reference = signal()
        capture = [(0, 0)] * 137 + reference

        result = self.analyze(reference, capture)

        self.assertTrue(result["quality_pass"])
        self.assertEqual(result["capture_start_frame"], 137)
        self.assertEqual(result["reference_start_frame"], 0)

    def test_current_anchor_shift_zero_fill_is_rejected(self):
        reference = signal()
        correction_frame = 128
        capture = (
            reference[:correction_frame]
            + [(0, 0)] * 12
            + reference[correction_frame:]
        )

        result = self.analyze(reference, capture)

        self.assertFalse(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"zero_gap": 12})
        self.assertEqual(result["max_event_frames"], 12)
        self.assertEqual(result["events"][0]["capture_frame"], correction_frame)
        self.assertGreater(result["peak_discontinuity"], 0.05)

    def test_single_duplicated_frame_is_classified_and_allowed(self):
        reference = signal()
        correction_frame = 128
        capture = (
            reference[:correction_frame]
            + [reference[correction_frame - 1]]
            + reference[correction_frame:]
        )

        result = self.analyze(reference, capture)

        self.assertTrue(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"duplicate": 1})

    def test_repeated_silent_frame_is_a_duplicate_not_a_zero_gap(self):
        reference = signal()
        correction_frame = 128
        reference[correction_frame - 1] = (0, 0)
        capture = (
            reference[:correction_frame]
            + [(0, 0)]
            + reference[correction_frame:]
        )

        result = self.analyze(reference, capture)

        self.assertTrue(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"duplicate": 1})

    def test_oversized_duplicate_is_rejected(self):
        reference = signal()
        correction_frame = 128
        capture = (
            reference[:correction_frame]
            + [reference[correction_frame - 1]] * 2
            + reference[correction_frame:]
        )

        result = self.analyze(reference, capture)

        self.assertFalse(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"duplicate": 2})

    def test_arbitrary_inserted_frame_is_rejected(self):
        reference = signal()
        correction_frame = 128
        capture = (
            reference[:correction_frame]
            + [(123, -456)]
            + reference[correction_frame:]
        )

        result = self.analyze(reference, capture)

        self.assertFalse(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"insertion": 1})

    def test_dropped_frame_is_classified(self):
        reference = signal()
        correction_frame = 128
        capture = reference[:correction_frame] + reference[correction_frame + 1:]

        result = self.analyze(reference, capture)

        self.assertTrue(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"drop": 1})

    def test_drop_larger_than_local_lookahead_reacquires_once(self):
        reference = signal(512)
        correction_frame = 128
        capture = reference[:correction_frame] + reference[correction_frame + 100:]

        result = self.analyze(reference, capture, lookahead_frames=64)

        self.assertFalse(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"drop": 100})
        self.assertEqual(result["event_count"], 1)

    def test_insertion_larger_than_local_lookahead_reacquires_once(self):
        reference = signal(512)
        correction_frame = 128
        inserted = [
            (300_000_000 + frame, -400_000_000 - frame)
            for frame in range(100)
        ]
        capture = (
            reference[:correction_frame]
            + inserted
            + reference[correction_frame:]
        )

        result = self.analyze(reference, capture, lookahead_frames=64)

        self.assertFalse(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"insertion": 100})
        self.assertEqual(result["event_count"], 1)

    def test_sample_mutation_is_rejected(self):
        reference = signal()
        capture = list(reference)
        capture[128] = (123, -456)

        result = self.analyze(reference, capture)

        self.assertFalse(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"mutation": 1})

    def test_mutation_larger_than_local_lookahead_reacquires_once(self):
        reference = signal(512)
        capture = list(reference)
        capture[128:228] = [
            (300_000_000 + frame, -400_000_000 - frame)
            for frame in range(100)
        ]

        result = self.analyze(reference, capture, lookahead_frames=64)

        self.assertFalse(result["quality_pass"])
        self.assertEqual(result["event_frames_by_kind"], {"desync": 100})
        self.assertEqual(result["event_count"], 1)

    def test_non_frame_aligned_reference_is_rejected_before_analysis(self):
        encoded = encode(signal())

        with self.assertRaisesRegex(ValueError, "reference length"):
            analyze_artifacts(
                encoded + b"\0",
                encoded,
                sample_rate=48000,
                channels=2,
            )

    def test_partial_trailing_capture_frame_is_ignored(self):
        encoded = encode(signal())

        result = analyze_artifacts(
            encoded,
            encoded + b"\x01\x02\x03",
            sample_rate=48000,
            channels=2,
        )

        self.assertTrue(result["quality_pass"])
        self.assertEqual(result["events"], [])


if __name__ == "__main__":
    unittest.main()

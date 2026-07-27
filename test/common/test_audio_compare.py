import struct
import unittest

from audio_compare import analyze


def encode_frames(frames):
    return b"".join(struct.pack("<ii", left, right) for left, right in frames)


class AudioCompareTest(unittest.TestCase):
    def test_exact_overlap_is_one_contiguous_run(self):
        audio = encode_frames(
            [(index + 1, -(index + 1)) for index in range(200)]
        )

        result = analyze(
            audio,
            audio,
            sample_rate=100,
            channels=2,
            min_run_seconds=1.0,
            probe_seconds=2.0,
        )

        self.assertTrue(result["pass"])
        self.assertTrue(result["single_contiguous_match"])
        self.assertEqual(result["run_count"], 1)
        self.assertEqual(result["longest_run_frames"], 200)

    def test_short_mutation_does_not_hide_long_bit_perfect_runs(self):
        reference_frames = [(index + 1, -(index + 1)) for index in range(300)]
        capture_frames = list(reference_frames)
        capture_frames[140:145] = [(999, -999)] * 5

        result = analyze(
            encode_frames(reference_frames),
            encode_frames(capture_frames),
            sample_rate=100,
            channels=2,
            min_run_seconds=1.0,
            probe_seconds=3.0,
        )

        self.assertTrue(result["pass"])
        self.assertFalse(result["single_contiguous_match"])
        self.assertEqual(result["run_count"], 2)
        self.assertEqual(result["matched_frames"], 295)
        self.assertEqual(result["longest_run_frames"], 155)


if __name__ == "__main__":
    unittest.main()

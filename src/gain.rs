use inferno_aoip::device_server::Sample;

use crate::bridge::SAMPLE_RATE;

const RAMP_DURATION_MS: u32 = 20;

/// dB attenuation at the knee-anchored end of the linear-in-dB taper (see
/// `volume_to_gain`). Not a true floor: below the knee the fade continues
/// past this value down to exact zero.
const TAPER_ANCHOR_DB: f32 = -40.0;
/// Below this fraction of full volume, gain fades linearly to true zero
/// instead of following the dB line (which never reaches silence).
const TAPER_KNEE: f32 = 0.1;

/// Map a 0-100 volume + mute state to a linear gain factor.
///
/// Uses a linear-in-dB (audio) taper so each volume step changes perceived
/// loudness by a constant amount: `dB = 40 * (volume/100 - 1)`, i.e. 0.4 dB
/// per 1% step, unity (exactly 1.0, bit-perfect) at 100%, -20 dB at 50%.
/// Below the 10% knee the gain fades linearly to exact zero at 0%, avoiding
/// an audible cliff between 1% and mute.
///
/// This intentionally replaces `GainControl::gain()`'s `(volume/100)^1.5`
/// perceptual curve, which compressed the audible range into 0-50% and left
/// 50-100% spanning only ~9 dB.
pub(crate) fn volume_to_gain(volume: u8, muted: bool) -> f32 {
    if muted {
        return 0.0;
    }
    let vol = f32::from(volume.min(100)) / 100.0;
    if vol <= 0.0 {
        0.0
    } else if vol < TAPER_KNEE {
        // Linear fade from zero up to the dB line's value at the knee.
        (vol / TAPER_KNEE) * 10f32.powf(TAPER_ANCHOR_DB / 20.0 * (1.0 - TAPER_KNEE))
    } else {
        10f32.powf(TAPER_ANCHOR_DB / 20.0 * (1.0 - vol))
    }
}

/// Per-frame gain ramp for i32 samples, avoiding clicks on volume changes.
///
/// Mirrors sendspin's `GainRamp` algorithm but adapted for per-channel
/// `Vec<Sample>` (i32) instead of interleaved f32 buffers.
pub(crate) struct BridgeGainRamp {
    ramp_duration_frames: u32,
    current_gain: f32,
    ramp_frames_remaining: u32,
    ramp_step: f32,
    last_target: f32,
}

impl BridgeGainRamp {
    pub(crate) fn new() -> Self {
        Self::with_gain(1.0)
    }

    pub(crate) fn with_gain(gain: f32) -> Self {
        Self {
            ramp_duration_frames: SAMPLE_RATE * RAMP_DURATION_MS / 1000,
            current_gain: gain,
            ramp_frames_remaining: 0,
            ramp_step: 0.0,
            last_target: gain,
        }
    }

    /// Apply gain with per-frame ramping to per-channel sample buffers.
    ///
    /// At unity gain with no active ramp, returns immediately (bit-perfect).
    pub(crate) fn apply(
        &mut self,
        channel_samples: &mut [Vec<Sample>],
        frames: usize,
        target: f32,
    ) {
        if frames == 0 || channel_samples.is_empty() {
            return;
        }

        self.update_target(target);

        if self.ramp_frames_remaining == 0 && self.current_gain == 1.0 {
            return;
        }

        let ramp_frames = (self.ramp_frames_remaining as usize).min(frames);

        // Ramp region: per-frame gain stepping.
        for frame in 0..ramp_frames {
            self.current_gain += self.ramp_step;
            self.ramp_frames_remaining -= 1;
            if self.ramp_frames_remaining == 0 {
                self.current_gain = target;
            }
            apply_gain_to_frame(channel_samples, frame, self.current_gain);
        }
        if ramp_frames > 0 && self.ramp_frames_remaining > 0 {
            self.current_gain = self.current_gain.clamp(0.0, 1.0);
        }

        // Steady-state region: constant gain.
        let gain = self.current_gain;
        if gain == 1.0 {
            return;
        }
        apply_gain_to_range(channel_samples, ramp_frames, frames, gain);
    }

    /// Apply gain to a sub-range `[start..start+len]` of each channel.
    pub(crate) fn apply_range(
        &mut self,
        channel_samples: &mut [Vec<Sample>],
        start: usize,
        len: usize,
        target: f32,
    ) {
        if len == 0 || channel_samples.is_empty() {
            return;
        }

        self.update_target(target);

        if self.ramp_frames_remaining == 0 && self.current_gain == 1.0 {
            return;
        }

        let ramp_frames = (self.ramp_frames_remaining as usize).min(len);

        for i in 0..ramp_frames {
            self.current_gain += self.ramp_step;
            self.ramp_frames_remaining -= 1;
            if self.ramp_frames_remaining == 0 {
                self.current_gain = target;
            }
            apply_gain_to_frame(channel_samples, start + i, self.current_gain);
        }
        if ramp_frames > 0 && self.ramp_frames_remaining > 0 {
            self.current_gain = self.current_gain.clamp(0.0, 1.0);
        }

        let gain = self.current_gain;
        if gain == 1.0 {
            return;
        }
        apply_gain_to_range(channel_samples, start + ramp_frames, start + len, gain);
    }

    /// Advance ramp state without touching samples (for trimmed/stale chunks).
    pub(crate) fn advance(&mut self, frames: usize, target: f32) {
        if frames == 0 {
            return;
        }

        self.update_target(target);

        let advance = u32::try_from(frames)
            .unwrap_or(u32::MAX)
            .min(self.ramp_frames_remaining);
        if advance > 0 {
            self.current_gain += self.ramp_step * advance as f32;
            self.ramp_frames_remaining -= advance;
            if self.ramp_frames_remaining == 0 {
                self.current_gain = target;
            } else {
                self.current_gain = self.current_gain.clamp(0.0, 1.0);
            }
        }
    }

    /// Snap to current target, clearing any in-progress ramp.
    /// Called on stream transitions to avoid carrying stale ramp state.
    pub(crate) fn reset_to_current(&mut self) {
        self.current_gain = self.last_target;
        self.ramp_frames_remaining = 0;
        self.ramp_step = 0.0;
    }

    fn update_target(&mut self, target: f32) {
        if !target.is_finite() {
            return;
        }
        if target.to_bits() != self.last_target.to_bits() {
            if self.ramp_duration_frames == 0 {
                self.current_gain = target;
            } else {
                self.ramp_frames_remaining = self.ramp_duration_frames;
                self.ramp_step = (target - self.current_gain) / self.ramp_duration_frames as f32;
            }
            self.last_target = target;
        }
    }
}

fn apply_gain_to_frame(channel_samples: &mut [Vec<Sample>], frame: usize, gain: f32) {
    if gain == 0.0 {
        for ch in channel_samples.iter_mut() {
            ch[frame] = 0;
        }
    } else {
        let g = gain as f64;
        for ch in channel_samples.iter_mut() {
            ch[frame] = (ch[frame] as f64 * g) as Sample;
        }
    }
}

fn apply_gain_to_range(channel_samples: &mut [Vec<Sample>], from: usize, to: usize, gain: f32) {
    if gain == 0.0 {
        for ch in channel_samples.iter_mut() {
            ch[from..to].fill(0);
        }
    } else {
        let g = gain as f64;
        for ch in channel_samples.iter_mut() {
            for s in ch[from..to].iter_mut() {
                *s = (*s as f64 * g) as Sample;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(left: &[Sample], right: &[Sample]) -> Vec<Vec<Sample>> {
        vec![left.to_vec(), right.to_vec()]
    }

    #[test]
    fn unity_gain_leaves_samples_unchanged() {
        let mut ramp = BridgeGainRamp::new();
        let original_l = vec![1000, -2000, 3000, -4000];
        let original_r = vec![5000, -6000, 7000, -8000];
        let mut samples = stereo(&original_l, &original_r);

        ramp.apply(&mut samples, 4, 1.0);

        assert_eq!(samples[0], original_l);
        assert_eq!(samples[1], original_r);
    }

    #[test]
    fn half_gain_scales_samples() {
        let mut ramp = BridgeGainRamp::new();
        // Set gain to 0.5 and complete the ramp
        let mut warmup = vec![vec![0; ramp.ramp_duration_frames as usize]; 2];
        ramp.apply(&mut warmup, ramp.ramp_duration_frames as usize, 0.5);

        let mut samples = stereo(&[10000, -20000], &[30000, -40000]);
        ramp.apply(&mut samples, 2, 0.5);

        assert_eq!(samples[0], vec![5000, -10000]);
        assert_eq!(samples[1], vec![15000, -20000]);
    }

    #[test]
    fn zero_gain_zeroes_all_samples() {
        let mut ramp = BridgeGainRamp::new();
        // Complete ramp to zero
        let frames = ramp.ramp_duration_frames as usize;
        let mut warmup = vec![vec![1; frames]; 2];
        ramp.apply(&mut warmup, frames, 0.0);

        let mut samples = stereo(&[12345, -67890], &[11111, -22222]);
        ramp.apply(&mut samples, 2, 0.0);

        assert_eq!(samples[0], vec![0, 0]);
        assert_eq!(samples[1], vec![0, 0]);
    }

    #[test]
    fn ramp_is_monotonic_decreasing() {
        let mut ramp = BridgeGainRamp::new();
        let frames = ramp.ramp_duration_frames as usize;
        let val = 1_000_000i32;
        let mut samples = vec![vec![val; frames]; 1];

        ramp.apply(&mut samples, frames, 0.0);

        // Each successive frame should be <= the previous (decreasing gain)
        for i in 1..frames {
            assert!(
                samples[0][i] <= samples[0][i - 1],
                "non-monotonic at frame {i}: {} > {}",
                samples[0][i],
                samples[0][i - 1]
            );
        }
        // Last frame should be exactly 0 (target reached)
        assert_eq!(samples[0][frames - 1], 0);
    }

    #[test]
    fn ramp_is_monotonic_increasing() {
        let mut ramp = BridgeGainRamp::new();
        // Start at gain 0
        let frames = ramp.ramp_duration_frames as usize;
        let mut warmup = vec![vec![0; frames]; 1];
        ramp.apply(&mut warmup, frames, 0.0);

        let val = 1_000_000i32;
        let mut samples = vec![vec![val; frames]; 1];
        ramp.apply(&mut samples, frames, 1.0);

        for i in 1..frames {
            assert!(
                samples[0][i] >= samples[0][i - 1],
                "non-monotonic at frame {i}: {} < {}",
                samples[0][i],
                samples[0][i - 1]
            );
        }
        assert_eq!(samples[0][frames - 1], val);
    }

    #[test]
    fn ramp_reaches_target_exactly() {
        let mut ramp = BridgeGainRamp::new();
        let frames = ramp.ramp_duration_frames as usize;
        let mut samples = vec![vec![100_000; frames]; 1];

        ramp.apply(&mut samples, frames, 0.5);

        assert!(
            (ramp.current_gain - 0.5).abs() < f32::EPSILON,
            "current_gain={}, expected 0.5",
            ramp.current_gain
        );
    }

    #[test]
    fn advance_matches_apply_state() {
        let mut ramp_apply = BridgeGainRamp::new();
        let mut ramp_advance = BridgeGainRamp::new();

        let frames = ramp_apply.ramp_duration_frames as usize;
        let mut buf = vec![vec![1000; frames]; 2];
        ramp_apply.apply(&mut buf, frames, 0.0);

        ramp_advance.advance(frames, 0.0);

        assert!(
            (ramp_apply.current_gain - ramp_advance.current_gain).abs() < f32::EPSILON,
            "apply={}, advance={}",
            ramp_apply.current_gain,
            ramp_advance.current_gain
        );
        assert_eq!(
            ramp_apply.ramp_frames_remaining,
            ramp_advance.ramp_frames_remaining
        );
    }

    #[test]
    fn trimmed_advance_then_apply() {
        // Simulate trimmed write: advance past 10 frames, then apply to remaining 10
        let mut ramp_full = BridgeGainRamp::new();
        let mut ramp_split = BridgeGainRamp::new();

        let val = 1_000_000i32;
        let frames = 20usize;

        // Full apply to 20 frames
        let mut full_samples = vec![vec![val; frames]; 1];
        ramp_full.apply(&mut full_samples, frames, 0.5);

        // Split: advance 10, then apply_range to [10..20]
        ramp_split.advance(10, 0.5);
        let mut split_samples = vec![vec![val; frames]; 1];
        ramp_split.apply_range(&mut split_samples, 10, 10, 0.5);

        // Frames 10-19 should match between full and split
        assert_eq!(
            full_samples[0][10..20],
            split_samples[0][10..20],
            "trimmed advance+apply produced different samples than full apply"
        );
        assert!((ramp_full.current_gain - ramp_split.current_gain).abs() < f32::EPSILON,);
    }

    #[test]
    fn reset_to_current_clears_ramp() {
        let mut ramp = BridgeGainRamp::new();
        // Start a ramp toward 0.5
        let mut samples = vec![vec![1000; 5]; 1];
        ramp.apply(&mut samples, 5, 0.5);
        assert!(ramp.ramp_frames_remaining > 0);

        ramp.reset_to_current();
        assert_eq!(ramp.ramp_frames_remaining, 0);
        assert!((ramp.current_gain - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn empty_input_is_noop() {
        let mut ramp = BridgeGainRamp::new();
        let gain_before = ramp.current_gain;
        ramp.apply(&mut [], 0, 0.5);
        assert_eq!(ramp.current_gain, gain_before);
    }

    #[test]
    fn apply_range_matches_apply_offset() {
        let mut ramp_a = BridgeGainRamp::new();
        let mut ramp_b = BridgeGainRamp::new();

        let val = 500_000i32;
        let total = 30usize;
        let start = 10;
        let len = 20;

        // Method A: apply to first 30 frames
        let mut buf_a = vec![vec![val; total]; 1];
        ramp_a.apply(&mut buf_a, total, 0.3);

        // Method B: advance 10, apply_range [10..30]
        ramp_b.advance(start, 0.3);
        let mut buf_b = vec![vec![val; total]; 1];
        ramp_b.apply_range(&mut buf_b, start, len, 0.3);

        assert_eq!(buf_a[0][start..total], buf_b[0][start..total]);
    }

    // ─── volume_to_gain taper ───────────────────────────────────────

    #[test]
    fn taper_full_volume_is_exact_unity() {
        // Must be exactly 1.0 so BridgeGainRamp's bit-perfect fast path engages.
        assert_eq!(volume_to_gain(100, false).to_bits(), 1.0f32.to_bits());
    }

    #[test]
    fn taper_mute_is_exact_zero() {
        for v in [0, 1, 10, 50, 100] {
            assert_eq!(volume_to_gain(v, true), 0.0);
        }
    }

    #[test]
    fn taper_zero_volume_is_exact_zero() {
        assert_eq!(volume_to_gain(0, false), 0.0);
    }

    #[test]
    fn taper_follows_db_line_above_knee() {
        // dB = 40 * (vol/100 - 1): 50% -> -20 dB, 75% -> -10 dB, 25% -> -30 dB
        let db = |v: u8| 20.0 * volume_to_gain(v, false).log10();
        assert!((db(50) - -20.0).abs() < 0.01, "50% = {} dB", db(50));
        assert!((db(75) - -10.0).abs() < 0.01, "75% = {} dB", db(75));
        assert!((db(25) - -30.0).abs() < 0.01, "25% = {} dB", db(25));
    }

    #[test]
    fn taper_knee_is_continuous() {
        // The fade branch at vol just below the knee must approach the dB
        // line's value at the knee (no discontinuity at 10%).
        let at_knee = volume_to_gain(10, false);
        let expected = 10f32.powf(TAPER_ANCHOR_DB / 20.0 * (1.0 - TAPER_KNEE));
        assert!((at_knee - expected).abs() < 1e-6);
        // 9% is on the fade branch: 0.9 * knee value.
        let below = volume_to_gain(9, false);
        assert!((below - 0.9 * expected).abs() < 1e-6);
    }

    #[test]
    fn taper_is_strictly_monotonic() {
        let mut prev = volume_to_gain(0, false);
        for v in 1..=100u8 {
            let g = volume_to_gain(v, false);
            assert!(g > prev, "not increasing at {v}%: {g} <= {prev}");
            prev = g;
        }
    }
}

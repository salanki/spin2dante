use log::{debug, info};
use std::time::Instant;

/// Tracks jitter buffer health metrics for console logging.
pub struct BufferMetrics {
    target_fill: usize,
    min_fill: isize,
    max_fill: isize,
    underruns: u64,
    overruns: u64,
    /// Recent (timestamp, fill_level) pairs for drift estimation.
    fill_history: Vec<(Instant, isize)>,
    /// Tracks whether a DANTE subscriber is actively consuming audio.
    last_read_pos: usize,
    /// How many consecutive log intervals read_pos has been stalled.
    stalled_intervals: u32,
    /// Whether we've logged a "subscriber active" recovery message.
    was_stalled: bool,
}

impl BufferMetrics {
    pub fn new(target_fill: usize) -> Self {
        Self {
            target_fill,
            min_fill: isize::MAX,
            max_fill: isize::MIN,
            underruns: 0,
            overruns: 0,
            fill_history: Vec::new(),
            last_read_pos: 0,
            stalled_intervals: 0,
            was_stalled: false,
        }
    }

    pub fn reset(&mut self) {
        self.min_fill = isize::MAX;
        self.max_fill = isize::MIN;
        self.underruns = 0;
        self.overruns = 0;
        self.fill_history.clear();
        self.last_read_pos = 0;
        self.stalled_intervals = 0;
        self.was_stalled = false;
    }

    /// Called on each audio write with current write and read positions.
    pub fn update(&mut self, write_pos: usize, read_pos: usize) {
        let fill = (write_pos as isize).wrapping_sub(read_pos as isize);

        if fill < self.min_fill {
            self.min_fill = fill;
        }
        if fill > self.max_fill {
            self.max_fill = fill;
        }
        if fill <= 0 {
            self.underruns += 1;
        }
        if fill as usize > super::bridge::RING_BUFFER_SIZE - 1024 {
            self.overruns += 1;
        }

        // Record fill level for drift estimation (keep last 60 entries)
        let now = Instant::now();
        self.fill_history.push((now, fill));
        if self.fill_history.len() > 60 {
            self.fill_history.remove(0);
        }
    }

    /// Log metrics to stderr. Called periodically.
    pub fn log(&mut self, write_pos: usize, read_pos: usize) {
        let fill = (write_pos as isize).wrapping_sub(read_pos as isize);

        // Detect whether read_pos is advancing (subscriber active)
        let read_advancing = read_pos != self.last_read_pos;
        self.last_read_pos = read_pos;

        if !read_advancing {
            self.stalled_intervals += 1;
            if self.stalled_intervals >= 2 {
                self.was_stalled = true;
                debug!(
                    "[buffer] read_pos not advancing (stalled for {} intervals, \
                     fill={}, write_pos={}, read_pos={})",
                    self.stalled_intervals, fill, write_pos, read_pos
                );
                return;
            }
        } else {
            if self.was_stalled {
                info!("[buffer] subscriber active; consumption resumed");
                self.was_stalled = false;
            }
            self.stalled_intervals = 0;
        }

        let drift_ppm = self.estimate_drift_ppm();

        let drift_str = match drift_ppm {
            Some(d) => format!("{:+.1}ppm", d),
            None => "n/a".to_string(),
        };

        info!(
            "[buffer] fill={} target={} drift={} min={} max={} underruns={} overruns={}",
            fill,
            self.target_fill,
            drift_str,
            if self.min_fill == isize::MAX {
                0
            } else {
                self.min_fill
            },
            if self.max_fill == isize::MIN {
                0
            } else {
                self.max_fill
            },
            self.underruns,
            self.overruns,
        );
    }

    /// Estimate clock drift in ppm from fill level trend.
    /// Positive = Sendspin faster than PTP, fill increasing.
    /// Negative = Sendspin slower, fill decreasing.
    fn estimate_drift_ppm(&self) -> Option<f64> {
        if self.fill_history.len() < 10 {
            return None;
        }

        let first = self.fill_history.first()?;
        let last = self.fill_history.last()?;

        let elapsed_secs = last.0.duration_since(first.0).as_secs_f64();
        if elapsed_secs < 5.0 {
            return None;
        }

        let fill_change = (last.1 - first.1) as f64;
        let drift_samples_per_sec = fill_change / elapsed_secs;
        let drift_ppm =
            (drift_samples_per_sec / super::bridge::SAMPLE_RATE as f64) * 1_000_000.0;

        Some(drift_ppm)
    }
}

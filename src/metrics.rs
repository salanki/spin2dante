use log::info;
use std::time::Instant;

/// Tracks bridge buffer metrics for console logging.
pub struct BufferMetrics {
    target_fill: usize,
    last_log_time: Option<Instant>,
    last_write_pos: usize,
    last_read_pos: usize,
}

impl BufferMetrics {
    pub fn new(target_fill: usize) -> Self {
        Self {
            target_fill,
            last_log_time: None,
            last_write_pos: 0,
            last_read_pos: 0,
        }
    }

    pub fn reset(&mut self) {
        self.last_log_time = None;
        self.last_write_pos = 0;
        self.last_read_pos = 0;
    }

    pub fn update(&mut self, _write_pos: usize, _read_pos: usize) {
        // No per-chunk tracking needed currently
    }

    pub fn log(&mut self, write_pos: usize, read_pos: usize) {
        let fill = (write_pos as isize).wrapping_sub(read_pos as isize);
        let target = self.target_fill as isize;

        let now = Instant::now();

        // Drift: how much fill deviates from target over time
        let drift_str = if read_pos > 0 {
            let deviation = fill - target;
            if let Some(last_time) = self.last_log_time {
                let interval = now.duration_since(last_time).as_secs_f64();
                let last_fill = (self.last_write_pos as isize).wrapping_sub(self.last_read_pos as isize);
                let fill_change = fill - last_fill;
                if interval > 0.0 {
                    let drift_samples_per_sec = fill_change as f64 / interval;
                    let drift_ppm = (drift_samples_per_sec / super::bridge::SAMPLE_RATE as f64) * 1_000_000.0;
                    format!("{:+.1}ppm", drift_ppm)
                } else {
                    "n/a".to_string()
                }
            } else {
                "n/a".to_string()
            }
        } else {
            "no-read".to_string()
        };

        if read_pos > 0 {
            info!(
                "[buffer] fill={} target={} drift={} write_pos={} read_pos={}",
                fill, self.target_fill, drift_str, write_pos, read_pos
            );
        } else {
            info!(
                "[buffer] writing at {} samples (read_pos not yet available)",
                write_pos
            );
        }

        self.last_write_pos = write_pos;
        self.last_read_pos = read_pos;
        self.last_log_time = Some(now);
    }
}

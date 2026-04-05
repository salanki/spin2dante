use log::info;
use std::time::Instant;

/// Tracks bridge activity metrics for console logging.
///
/// Note: inferno's ExternalBuffer::unconditional_read() means
/// PositionReportDestination is never updated, so we cannot observe
/// the actual read position or jitter buffer fill level. We only
/// track write-side metrics.
pub struct BufferMetrics {
    target_fill: usize,
    last_write_pos: usize,
    last_log_time: Option<Instant>,
    total_samples_written: u64,
    start_time: Instant,
}

impl BufferMetrics {
    pub fn new(target_fill: usize) -> Self {
        Self {
            target_fill,
            last_write_pos: 0,
            last_log_time: None,
            total_samples_written: 0,
            start_time: Instant::now(),
        }
    }

    pub fn reset(&mut self) {
        self.last_write_pos = 0;
        self.last_log_time = None;
        self.total_samples_written = 0;
        self.start_time = Instant::now();
    }

    /// Called on each audio write.
    pub fn update(&mut self, write_pos: usize, _read_pos: usize) {
        let delta = write_pos.wrapping_sub(self.last_write_pos);
        self.total_samples_written += delta as u64;
        self.last_write_pos = write_pos;
    }

    /// Log metrics. Called periodically.
    pub fn log(&mut self, write_pos: usize, _read_pos: usize) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.start_time).as_secs();

        // Calculate write rate over last interval
        let rate_str = if let Some(last_time) = self.last_log_time {
            let interval = now.duration_since(last_time).as_secs_f64();
            let delta = write_pos.wrapping_sub(self.last_write_pos) as f64;
            if interval > 0.0 {
                let rate_hz = delta / interval;
                format!("{:.0}Hz", rate_hz)
            } else {
                "n/a".to_string()
            }
        } else {
            "n/a".to_string()
        };

        let total_sec = self.total_samples_written / super::bridge::SAMPLE_RATE as u64;

        info!(
            "[buffer] writing at {} | {}s total | target_buffer={}ms",
            rate_str,
            total_sec,
            self.target_fill * 1000 / super::bridge::SAMPLE_RATE as usize,
        );

        self.last_write_pos = write_pos;
        self.last_log_time = Some(now);
    }
}

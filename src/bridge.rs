use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use parking_lot::Mutex;

use atomic::Atomic;
use inferno_aoip::device_server::{DeviceServer, OwnedBuffer, RBInput, ReadPositionSnapshot, Sample, Settings};
use log::{debug, error, info, warn};
use sendspin::protocol::client::AudioChunk;
use sendspin::protocol::messages::{AudioFormatSpec, Message, PlayerV1Support};
use sendspin::sync::clock::ClockSync;
use sendspin::ProtocolClientBuilder;
use tokio::sync::oneshot;

use crate::metrics::BufferMetrics;

pub const CHANNELS: usize = 2;
pub const RING_BUFFER_SIZE: usize = 16384; // ~341ms at 48kHz, power of 2
pub const SAMPLE_RATE: u32 = 48000;
const METRICS_INTERVAL_SECS: u64 = 5;
const HOLE_FIX_WAIT: usize = 4800; // ~100ms at 48kHz
const MAX_PENDING_CHUNKS: usize = 200; // ~5s at 25 frames/chunk — bounds RAM usage

fn wrapsub(a: usize, b: usize) -> isize {
    (a as isize).wrapping_sub(b as isize)
}

// ─── Bridge state machine ───────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum BridgeState {
    Idle,
    WaitingForSubscriber,
    Prebuffering,
    Running,
    Rebuffering,
}

// ─── Stream format ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct StreamFormat {
    codec: String,
    sample_rate: u32,
    channels: u8,
    bit_depth: u8,
}

// ─── Pending chunk ──────────────────────────────────────────────────

struct PendingChunk {
    timestamp_us: i64,
    frames: usize,
    channel_samples: Vec<Vec<Sample>>,
}

// ─── Bridge ─────────────────────────────────────────────────────────

pub struct SendspinBridge {
    url: String,
    device_name: String,
    client_id: String,
    buffer_ms: u32,
    state: BridgeState,
    // Device + TX state (persistent for process lifetime)
    rb_inputs: Option<Vec<RBInput<Sample, OwnedBuffer<Atomic<Sample>>>>>,
    device_server: Option<DeviceServer>,
    current_timestamp: Arc<AtomicUsize>,
    read_position: Arc<AtomicUsize>,
    read_position_snapshot: Arc<ReadPositionSnapshot>,
    // Stream state (reset per stream)
    write_pos: usize,
    prebuffer_target: usize,
    prebuffer_written: usize,
    stream_format: Option<StreamFormat>,
    metrics: BufferMetrics,
    last_read_pos: usize,
    waiting_since: Option<std::time::Instant>,
    // Two-stage queue: Sendspin pending → Dante ring
    clock_sync: Option<Arc<Mutex<ClockSync>>>,
    pending_chunks: VecDeque<PendingChunk>,
    // Server-now anchor: set once, maps server_time → ring_position.
    // All targets computed relative to this anchor for stable spacing.
    anchor_server_us: Option<i64>,
    anchor_ring_pos: Option<usize>,
    // Scheduler counters
    stale_drops: u64,
    trimmed_chunks: u64,
    trimmed_frames: u64,
    queued_high_water: usize,
    scheduler_settled: bool,
}

impl SendspinBridge {
    pub fn new(url: String, device_name: String, buffer_ms: u32, client_id: String) -> Self {
        let prebuffer_target = (SAMPLE_RATE as usize * buffer_ms as usize) / 1000;
        Self {
            url,
            device_name,
            client_id,
            buffer_ms,
            state: BridgeState::Idle,
            rb_inputs: None,
            device_server: None,
            current_timestamp: Arc::new(AtomicUsize::new(usize::MAX)),
            read_position: Arc::new(AtomicUsize::new(usize::MAX)),
            read_position_snapshot: Arc::new(ReadPositionSnapshot::new()),
            write_pos: 0,
            prebuffer_target,
            prebuffer_written: 0,
            stream_format: None,
            metrics: BufferMetrics::new(prebuffer_target),
            last_read_pos: 0,
            waiting_since: None,
            clock_sync: None,
            pending_chunks: VecDeque::new(),
            anchor_server_us: None,
            anchor_ring_pos: None,
            stale_drops: 0,
            trimmed_chunks: 0,
            trimmed_frames: 0,
            queued_high_water: 0,
            scheduler_settled: false,
        }
    }

    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.start_device().await;
        loop {
            match self.run_session().await {
                Ok(()) => {
                    info!("session ended cleanly (ctrl-c), exiting");
                    self.shutdown().await;
                    return Ok(());
                }
                Err(e) => {
                    warn!("session ended with error: {e}, reconnecting in 2s...");
                    self.enter_idle();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }

    async fn start_device(&mut self) {
        let short_name = self.device_name.chars().take(14).collect::<String>();
        let mut config = std::collections::BTreeMap::new();
        config.insert("NAME".to_string(), self.device_name.clone());
        config.insert("TX_SOURCE_BIT_DEPTH".to_string(), "24".to_string());
        let mut settings = Settings::new(&self.device_name, &short_name, None, &config);
        settings.make_tx_channels(CHANNELS);
        settings.make_rx_channels(0);

        info!(
            "starting DANTE device: {} (waiting for PTP clock...)",
            self.device_name
        );
        let mut server = DeviceServer::start(settings).await;
        info!("DANTE device started, clock ready");

        let (start_tx, start_rx) = oneshot::channel();
        self.current_timestamp
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
        self.read_position
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);

        let rb_inputs = server
            .transmit_from_owned_buffer(
                CHANNELS,
                RING_BUFFER_SIZE,
                HOLE_FIX_WAIT,
                start_rx,
                self.current_timestamp.clone(),
                self.read_position.clone(),
                Some(self.read_position_snapshot.clone()),
                None,
            )
            .await;

        info!("FlowsTransmitter started (start_time=0, idle with silence)");
        let _ = start_tx.send(0);

        self.rb_inputs = Some(rb_inputs);
        self.device_server = Some(server);
        self.write_pos = 0;
        self.state = BridgeState::Idle;
    }

    async fn run_session(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let client = loop {
            info!("connecting to Sendspin server at {}", self.url);
            // Advertise a small buffer_capacity so the server doesn't send
            // audio too far ahead of real-time. The pending queue + ring need
            // the server lead to fit within RING_BUFFER_SIZE (~341ms).
            let player_support = PlayerV1Support {
                supported_formats: vec![
                    AudioFormatSpec {
                        codec: "pcm".to_string(),
                        channels: CHANNELS as u8,
                        sample_rate: SAMPLE_RATE,
                        bit_depth: 24,
                    },
                    AudioFormatSpec {
                        codec: "pcm".to_string(),
                        channels: CHANNELS as u8,
                        sample_rate: SAMPLE_RATE,
                        bit_depth: 16,
                    },
                ],
                buffer_capacity: (SAMPLE_RATE as u32 * CHANNELS as u32 * 3 / 2), // ~500ms stereo 24-bit
                supported_commands: vec!["volume".to_string(), "mute".to_string()],
            };
            match ProtocolClientBuilder::builder()
                .client_id(self.client_id.clone())
                .name(self.device_name.clone())
                .product_name(Some("spin2dante".to_string()))
                .manufacturer(Some("spin2dante".to_string()))
                .software_version(Some(env!("CARGO_PKG_VERSION").to_string()))
                .player_v1_support(player_support)
                .build()
                .connect(&self.url)
                .await
            {
                Ok(client) => break client,
                Err(e) => {
                    warn!("connection failed: {e}, retrying in 2s...");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        };

        info!("connected to Sendspin server");

        let (mut messages, mut audio, clock_sync, _sender, _guard) = client.split();
        self.clock_sync = Some(clock_sync);

        let mut metrics_interval =
            tokio::time::interval(std::time::Duration::from_secs(METRICS_INTERVAL_SECS));

        loop {
            tokio::select! {
                msg = messages.recv() => {
                    match msg {
                        Some(msg) => self.handle_message(msg),
                        None => return Err("Sendspin connection closed".into()),
                    }
                }
                chunk = audio.recv() => {
                    match chunk {
                        Some(chunk) => self.handle_audio(chunk),
                        None => return Err("Sendspin audio stream ended".into()),
                    }
                }
                _ = metrics_interval.tick() => {
                    self.log_metrics();
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("shutting down");
                    return Ok(());
                }
            }
        }
    }

    fn handle_message(&mut self, msg: Message) {
        match msg {
            Message::StreamStart(start) => {
                if let Some(player) = start.player {
                    let format = StreamFormat {
                        codec: player.codec.clone(),
                        sample_rate: player.sample_rate,
                        channels: player.channels,
                        bit_depth: player.bit_depth,
                    };
                    info!(
                        "stream start: codec={} rate={} ch={} bits={}",
                        format.codec, format.sample_rate, format.channels, format.bit_depth
                    );

                    if format.sample_rate != SAMPLE_RATE {
                        error!(
                            "rejecting stream: sample rate {}Hz, requires {}Hz",
                            format.sample_rate, SAMPLE_RATE
                        );
                        return;
                    }
                    if format.channels != CHANNELS as u8 {
                        error!(
                            "rejecting stream: {} channels, requires {} (stereo)",
                            format.channels, CHANNELS
                        );
                        return;
                    }
                    if format.codec != "pcm" {
                        error!(
                            "rejecting stream: codec '{}', only 'pcm' supported",
                            format.codec
                        );
                        return;
                    }
                    if format.bit_depth != 16 && format.bit_depth != 24 {
                        error!(
                            "rejecting stream: PCM bit depth {}, only 16 or 24 supported",
                            format.bit_depth
                        );
                        return;
                    }

                    if self.state == BridgeState::Running
                        && self.stream_format.as_ref() == Some(&format)
                    {
                        info!("stream/start with same format: clearing and rebuffering");
                        self.clear_and_rebuffer();
                        return;
                    }

                    self.stream_format = Some(format);
                    self.reset_scheduler();

                    let read_pos = self.get_read_pos();
                    if read_pos != 0 && read_pos != self.last_read_pos {
                        info!(
                            "subscriber already active (read_pos={}), snapping to live",
                            read_pos
                        );
                        self.snap_to_live();
                    } else {
                        self.state = BridgeState::WaitingForSubscriber;
                        self.last_read_pos = read_pos;
                        self.waiting_since = Some(std::time::Instant::now());
                        self.metrics.reset();
                        info!("waiting for DANTE subscriber...");
                    }
                }
            }
            Message::StreamEnd(_) => {
                info!("stream ended, entering idle (device stays on network)");
                self.enter_idle();
            }
            Message::StreamClear(_) => {
                info!("stream cleared, discarding buffered audio");
                self.clear_and_rebuffer();
            }
            _ => {
                debug!("unhandled message type");
            }
        }
    }

    // ─── Helpers ────────────────────────────────────────────────────

    fn get_read_pos(&self) -> usize {
        let pos = self
            .read_position
            .load(std::sync::atomic::Ordering::Relaxed);
        if pos == usize::MAX {
            0
        } else {
            pos
        }
    }

    /// Get current server time in microseconds via ClockSync.
    fn server_now_us(&self) -> Option<i64> {
        let cs = self.clock_sync.as_ref()?;
        let sync = cs.lock();
        if !sync.is_synchronized() {
            return None;
        }
        let now_us = sync.instant_to_client_micros(std::time::Instant::now())?;
        sync.client_to_server_micros(now_us)
    }

    /// Read a consistent (read_pos, Instant) pair from the TX thread's seqlock snapshot.
    /// Returns None if the snapshot hasn't been written yet or if a write is in progress.
    fn get_read_pos_snapshot(&self) -> Option<(usize, std::time::Instant)> {
        self.read_position_snapshot.try_read()
    }

    /// Get a consistent (read_pos, server_now_us) pair by reading the TX snapshot
    /// and converting the snapshot's monotonic instant to server time via ClockSync.
    /// This eliminates the timing gap between sampling read_pos and server_now separately.
    fn get_synced_pair(&self) -> Option<(usize, i64)> {
        let (read_pos, snapshot_instant) = self.get_read_pos_snapshot()?;
        let cs = self.clock_sync.as_ref()?;
        let sync = cs.lock();
        if !sync.is_synchronized() {
            return None;
        }
        let client_us = sync.instant_to_client_micros(snapshot_instant)?;
        let server_us = sync.client_to_server_micros(client_us)?;
        Some((read_pos, server_us))
    }

    fn reset_scheduler(&mut self) {
        self.pending_chunks.clear();
        self.anchor_server_us = None;
        self.anchor_ring_pos = None;
        self.stale_drops = 0;
        self.trimmed_chunks = 0;
        self.trimmed_frames = 0;
        self.queued_high_water = 0;
        self.scheduler_settled = false;
    }

    // ─── State transitions ──────────────────────────────────────────

    fn enter_idle(&mut self) {
        if let Some(inputs) = &mut self.rb_inputs {
            let half = RING_BUFFER_SIZE / 2;
            for rb in inputs.iter_mut() {
                let silence: Vec<Sample> = vec![0; half];
                rb.write_from_at(self.write_pos, silence.clone().into_iter());
                rb.write_from_at(self.write_pos.wrapping_add(half), silence.into_iter());
            }
            self.write_pos = self.write_pos.wrapping_add(RING_BUFFER_SIZE);
        }
        self.stream_format = None;
        self.prebuffer_written = 0;
        self.last_read_pos = 0;
        self.reset_scheduler();
        self.state = BridgeState::Idle;
        self.metrics.reset();
    }

    fn snap_to_live(&mut self) {
        let read_pos = self.get_read_pos();
        if let Some(inputs) = &mut self.rb_inputs {
            let silence: Vec<Sample> = vec![0; self.prebuffer_target];
            for rb in inputs.iter_mut() {
                rb.write_from_at(read_pos, silence.clone().into_iter());
            }
        }
        self.write_pos = read_pos.wrapping_add(self.prebuffer_target);
        info!(
            "snapped to live: read_pos={}, write_pos={}",
            read_pos, self.write_pos
        );
        self.prebuffer_written = 0;
        self.state = BridgeState::Prebuffering;
        self.metrics.reset();
        info!(
            "prebuffering {}ms ({} samples)",
            self.buffer_ms, self.prebuffer_target
        );
    }

    fn clear_and_rebuffer(&mut self) {
        let read_pos = self.get_read_pos();
        if let Some(inputs) = &mut self.rb_inputs {
            let silence: Vec<Sample> = vec![0; self.prebuffer_target];
            for rb in inputs.iter_mut() {
                rb.write_from_at(read_pos, silence.clone().into_iter());
            }
        }
        self.write_pos = read_pos.wrapping_add(self.prebuffer_target);
        info!(
            "cleared stale audio, entering Rebuffering (read_pos={}, write_pos={})",
            read_pos, self.write_pos
        );
        self.prebuffer_written = 0;
        self.reset_scheduler();
        self.state = BridgeState::Rebuffering;
        self.metrics.reset();
    }

    // ─── Audio handling: enqueue + drain ─────────────────────────────

    fn handle_audio(&mut self, chunk: AudioChunk) {
        let format = match &self.stream_format {
            Some(f) => f.clone(),
            None => return,
        };

        if self.state == BridgeState::Idle {
            return;
        }

        // Decode PCM samples per channel
        let (frames, channel_samples) = self.decode_pcm(&chunk.data, &format);
        if frames == 0 {
            return;
        }

        let read_pos = self.get_read_pos();

        // ── Auto-realignment: detect PTP domain mismatch ──
        if read_pos != 0 {
            let distance = if wrapsub(self.write_pos, read_pos) > 0 {
                wrapsub(self.write_pos, read_pos) as usize
            } else {
                wrapsub(read_pos, self.write_pos) as usize
            };
            if distance > RING_BUFFER_SIZE {
                info!(
                    "write/read misalignment (write={}, read={}, dist={}), snapping",
                    self.write_pos, read_pos, distance
                );
                self.snap_to_live();
                self.reset_scheduler();
            }
        }

        // Enqueue the chunk (bounded: drop oldest if queue overflows)
        self.pending_chunks.push_back(PendingChunk {
            timestamp_us: chunk.timestamp,
            frames,
            channel_samples,
        });
        while self.pending_chunks.len() > MAX_PENDING_CHUNKS {
            self.stale_drops += 1;
            self.pending_chunks.pop_front();
        }
        if self.pending_chunks.len() > self.queued_high_water {
            self.queued_high_water = self.pending_chunks.len();
        }

        // WaitingForSubscriber: handle subscriber detection before draining
        // to avoid anchoring + writing chunks that snap_to_live will discard.
        if self.state == BridgeState::WaitingForSubscriber {
            if read_pos != self.last_read_pos && read_pos != 0 {
                info!(
                    "subscriber detected (read_pos={}), snapping to live",
                    read_pos
                );
                self.waiting_since = None;
                self.snap_to_live();
                self.reset_scheduler();
            } else if self
                .waiting_since
                .map_or(false, |t| t.elapsed().as_secs() >= 5)
            {
                info!("subscriber wait timed out, entering prebuffering");
                self.waiting_since = None;
                self.prebuffer_written = 0;
                self.state = BridgeState::Prebuffering;
                self.metrics.reset();
            }
            self.last_read_pos = read_pos;
        }

        // Drain eligible chunks to ring
        self.drain_pending(read_pos);
    }

    // ─── Drain: move eligible chunks from pending queue to ring ──────

    fn drain_pending(&mut self, read_pos: usize) {
        // Before FlowsTransmitter has a PTP clock, read_pos is 0 and ring
        // positions are meaningless — write sequentially to keep audio flowing.
        if read_pos == 0 {
            self.drain_sequential();
            return;
        }

        // Set anchor on first scheduled drain. Uses server_now_us() so that
        // anchor_server_us is from the shared Sendspin timeline. anchor_ring_pos
        // still depends on each bridge's local read_pos at anchor time, so
        // cross-bridge sync accuracy depends on how close the anchor instants are.
        if self.anchor_server_us.is_none() {
            // Use the TX snapshot for a consistent (read_pos, server_time) pair.
            // This eliminates the timing gap between sampling read_pos and server_now
            // separately, which was the source of cross-bridge anchor offset.
            match self.get_synced_pair() {
                Some((snap_read_pos, snap_server_us)) => {
                    let ring_pos = snap_read_pos.wrapping_add(self.prebuffer_target);
                    self.anchor_server_us = Some(snap_server_us);
                    self.anchor_ring_pos = Some(ring_pos);
                    let sync_key = ring_pos.wrapping_sub(
                        (snap_server_us as u128 * SAMPLE_RATE as u128 / 1_000_000) as usize,
                    );
                    info!(
                        "scheduler anchored: server_us={}, ring_pos={}, snap_read_pos={}, read_pos={}, sync_key={}",
                        snap_server_us, ring_pos, snap_read_pos, read_pos, sync_key,
                    );
                    // Write sync_key to shared volume for test harness
                    if std::env::var("SPIN2DANTE_WRITE_SYNC_KEY").is_ok() {
                        let _ = std::fs::write(
                            format!("/shared/sync_key_{}.txt", self.device_name),
                            format!("{}", sync_key),
                        );
                    }
                }
                None => {
                    // Snapshot or ClockSync not ready yet — write sequentially
                    self.drain_sequential();
                    return;
                }
            }
        }
        // Once anchored, targets use only anchor fields + chunk timestamps.
        // No need to check ClockSync availability — transient loss shouldn't stall audio.

        let anchor_us = self.anchor_server_us.unwrap();
        let anchor_pos = self.anchor_ring_pos.unwrap();

        while let Some(chunk) = self.pending_chunks.front() {
            // Target = anchor position + delta from anchor timestamp.
            // This gives stable spacing: consecutive chunks are exactly
            // their timestamp delta apart, unaffected by wall-clock jitter.
            let delta_us = chunk.timestamp_us - anchor_us;
            let delta_samples = (delta_us * SAMPLE_RATE as i64 / 1_000_000) as isize;
            let target = anchor_pos.wrapping_add_signed(delta_samples);
            let chunk_end = target.wrapping_add(chunk.frames);

            // Too early: target beyond writable ring horizon
            let distance_from_read = wrapsub(target, read_pos);
            if distance_from_read > (RING_BUFFER_SIZE - chunk.frames) as isize {
                break; // leave queued, try next drain
            }

            // Entirely stale: chunk_end behind read_pos
            if wrapsub(chunk_end, read_pos) <= 0 {
                self.stale_drops += 1;
                debug!(
                    "dropped stale chunk: ts={}, target={}, read_pos={}",
                    chunk.timestamp_us, target, read_pos
                );
                self.pending_chunks.pop_front();
                continue;
            }

            // Partial overlap: target behind read_pos but chunk_end ahead
            if wrapsub(target, read_pos) < 0 && wrapsub(chunk_end, read_pos) > 0 {
                let trim = read_pos.wrapping_sub(target);
                let remaining = chunk.frames - trim;
                self.trimmed_chunks += 1;
                self.trimmed_frames += trim as u64;
                info!(
                    "trimming {} stale samples, writing {} at read_pos={}",
                    trim, remaining, read_pos
                );
                let chunk = self.pending_chunks.pop_front().unwrap();
                self.write_trimmed_samples(&chunk.channel_samples, trim, remaining, read_pos);
                if wrapsub(chunk_end, self.write_pos) > 0 {
                    self.write_pos = chunk_end;
                }
                self.update_state_after_write(remaining, read_pos);
                continue;
            }

            // Large gap handling
            if wrapsub(target, self.write_pos) > (RING_BUFFER_SIZE / 2) as isize {
                if !self.scheduler_settled {
                    // Scheduler activation: first chunks land far ahead of write_pos.
                    // The gap is just silence from snap_to_live — advance past it.
                    info!(
                        "scheduler activation: advancing write_pos {} -> {} (gap={} samples)",
                        self.write_pos,
                        target,
                        wrapsub(target, self.write_pos),
                    );
                    self.write_pos = target;
                    self.scheduler_settled = true;
                } else {
                    // Settled scheduler: real discontinuity
                    info!(
                        "discontinuity (target={}, write_pos={}, settled={}), snapping",
                        target, self.write_pos, self.scheduler_settled
                    );
                    self.snap_to_live();
                    self.reset_scheduler();
                    break;
                }
            }

            // Backward write handling: target behind write_pos
            let backward = wrapsub(self.write_pos, target);
            if backward > chunk.frames as isize {
                // Significant backward overwrite — treat as discontinuity.
                // clear_and_rebuffer() calls reset_scheduler() which discards the
                // entire pending queue, not just this chunk. This is intentional:
                // a large backward target implies broken scheduler state, so all
                // queued positions are suspect.
                warn!(
                    "significant backward target: target={} behind write_pos={} by {} samples, rebuffering (dropping {} queued chunks)",
                    target, self.write_pos, backward, self.pending_chunks.len()
                );
                self.clear_and_rebuffer();
                break;
            } else if backward > 0 {
                debug!(
                    "backward jitter: target={} behind write_pos={} by {} samples",
                    target, self.write_pos, backward
                );
            }

            // Normal: write chunk at target
            let chunk = self.pending_chunks.pop_front().unwrap();
            self.write_samples_at(&chunk.channel_samples, chunk.frames, target);
            if wrapsub(chunk_end, self.write_pos) > 0 {
                self.write_pos = chunk_end;
            }
            self.update_state_after_write(chunk.frames, read_pos);
            if !self.scheduler_settled {
                self.scheduler_settled = true;
            }
        }
    }

    /// Fallback: write pending chunks sequentially (clock sync not ready or read_pos=0).
    fn drain_sequential(&mut self) {
        let read_pos = self.get_read_pos();
        while let Some(chunk) = self.pending_chunks.pop_front() {
            self.write_samples_at(&chunk.channel_samples, chunk.frames, self.write_pos);
            self.write_pos = self.write_pos.wrapping_add(chunk.frames);
            self.update_state_after_write(chunk.frames, read_pos);
        }
    }

    // ─── PCM decode ─────────────────────────────────────────────────

    fn decode_pcm(&self, data: &[u8], format: &StreamFormat) -> (usize, Vec<Vec<Sample>>) {
        let (bytes_per_sample, frames) = match format.bit_depth {
            24 => (3, data.len() / (3 * CHANNELS)),
            16 => (2, data.len() / (2 * CHANNELS)),
            _ => return (0, vec![]),
        };

        let frame_size = bytes_per_sample * CHANNELS;
        let mut channels = vec![Vec::with_capacity(frames); CHANNELS];

        for frame in 0..frames {
            for ch in 0..CHANNELS {
                let offset = frame * frame_size + ch * bytes_per_sample;
                let sample = if bytes_per_sample == 3 {
                    let b = &data[offset..offset + 3];
                    let raw = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                    let sign_extended = (raw << 8) >> 8;
                    sign_extended << 8
                } else {
                    let b = &data[offset..offset + 2];
                    let raw = i16::from_le_bytes([b[0], b[1]]) as i32;
                    raw << 16
                };
                channels[ch].push(sample);
            }
        }

        (frames, channels)
    }

    // ─── Ring buffer writes ─────────────────────────────────────────

    fn write_samples_at(&mut self, channel_samples: &[Vec<Sample>], _frames: usize, pos: usize) {
        if let Some(inputs) = &mut self.rb_inputs {
            for (ch, samples) in channel_samples.iter().enumerate() {
                inputs[ch].write_from_at(pos, samples.iter().copied());
            }
        }
    }

    fn write_trimmed_samples(
        &mut self,
        channel_samples: &[Vec<Sample>],
        trim: usize,
        remaining: usize,
        pos: usize,
    ) {
        if let Some(inputs) = &mut self.rb_inputs {
            for (ch, samples) in channel_samples.iter().enumerate() {
                inputs[ch].write_from_at(pos, samples[trim..trim + remaining].iter().copied());
            }
        }
    }

    fn update_state_after_write(&mut self, frames: usize, read_pos: usize) {
        if self.state == BridgeState::Prebuffering || self.state == BridgeState::Rebuffering {
            self.prebuffer_written += frames;
            if self.prebuffer_written >= self.prebuffer_target {
                self.state = BridgeState::Running;
                let fill = wrapsub(self.write_pos, read_pos);
                info!(
                    "prebuffer complete ({} samples), fill={}, read_pos={}, now transmitting",
                    self.prebuffer_written, fill, read_pos
                );
            }
        }
        self.metrics.update(self.write_pos, read_pos);
    }

    // ─── Metrics ────────────────────────────────────────────────────

    fn log_metrics(&mut self) {
        match self.state {
            BridgeState::Idle => {}
            BridgeState::WaitingForSubscriber => {
                info!("[buffer] waiting for DANTE subscriber");
            }
            BridgeState::Running => {
                let mode = if self.anchor_server_us.is_some() {
                    "scheduled"
                } else {
                    "sequential"
                };
                info!(
                    "[sync] mode={} pending={} stale_drops={} trims={}/{} high_water={}",
                    mode,
                    self.pending_chunks.len(),
                    self.stale_drops,
                    self.trimmed_chunks,
                    self.trimmed_frames,
                    self.queued_high_water,
                );
                self.metrics.log(self.write_pos, self.get_read_pos());
            }
            _ => {}
        }
    }

    async fn shutdown(&mut self) {
        if let Some(mut server) = self.device_server.take() {
            info!("stopping DANTE device");
            server.stop_transmitter().await;
            server.shutdown().await;
        }
        self.rb_inputs = None;
        self.state = BridgeState::Idle;
        info!("bridge shutdown complete");
    }
}

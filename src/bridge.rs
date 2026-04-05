use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use parking_lot::Mutex;

use atomic::Atomic;
use inferno_aoip::device_server::{
    DeviceServer, OwnedBuffer, RBInput, Sample, Settings,
};
use log::{debug, error, info, warn};
use sendspin::protocol::client::AudioChunk;
use sendspin::protocol::messages::Message;
use sendspin::sync::clock::ClockSync;
use sendspin::ProtocolClientBuilder;
use tokio::sync::oneshot;

use crate::metrics::BufferMetrics;

pub const CHANNELS: usize = 2;
pub const RING_BUFFER_SIZE: usize = 131072; // ~2.7s at 48kHz, power of 2
pub const SAMPLE_RATE: u32 = 48000;
const METRICS_INTERVAL_SECS: u64 = 5;
const HOLE_FIX_WAIT: usize = 4800; // ~100ms at 48kHz

/// Wrap-aware signed difference: (a - b) as isize with wrapping.
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
    // Stream state (reset per stream)
    write_pos: usize,
    prebuffer_target: usize,
    prebuffer_written: usize,
    stream_format: Option<StreamFormat>,
    metrics: BufferMetrics,
    last_read_pos: usize,
    waiting_since: Option<std::time::Instant>,
    // Sendspin timestamp sync
    clock_sync: Option<Arc<Mutex<ClockSync>>>,
    anchor_server_us: Option<i64>,
    anchor_ring_pos: Option<usize>,
    sync_active: bool,
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
            write_pos: 0,
            prebuffer_target,
            prebuffer_written: 0,
            stream_format: None,
            metrics: BufferMetrics::new(prebuffer_target),
            last_read_pos: 0,
            waiting_since: None,
            clock_sync: None,
            anchor_server_us: None,
            anchor_ring_pos: None,
            sync_active: false,
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
        config.insert("TX_DITHER_24BIT".to_string(), "false".to_string());
        let mut settings = Settings::new(&self.device_name, &short_name, None, &config);
        settings.make_tx_channels(CHANNELS);
        settings.make_rx_channels(0);

        info!("starting DANTE device: {} (waiting for PTP clock...)", self.device_name);
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
            match ProtocolClientBuilder::builder()
                .client_id(self.client_id.clone())
                .name(self.device_name.clone())
                .product_name(Some("spin2dante".to_string()))
                .manufacturer(Some("spin2dante".to_string()))
                .software_version(Some(env!("CARGO_PKG_VERSION").to_string()))
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
                        error!("rejecting stream: sample rate {}Hz, requires {}Hz", format.sample_rate, SAMPLE_RATE);
                        return;
                    }
                    if format.channels != CHANNELS as u8 {
                        error!("rejecting stream: {} channels, requires {} (stereo)", format.channels, CHANNELS);
                        return;
                    }
                    if format.codec != "pcm" {
                        error!("rejecting stream: codec '{}', only 'pcm' supported", format.codec);
                        return;
                    }
                    if format.bit_depth != 16 && format.bit_depth != 24 {
                        error!("rejecting stream: PCM bit depth {}, only 16 or 24 supported", format.bit_depth);
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
                    self.reset_sync();

                    let read_pos = self.get_read_pos();
                    if read_pos != 0 && read_pos != self.last_read_pos {
                        info!("subscriber already active (read_pos={}), snapping to live", read_pos);
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

    fn get_read_pos(&self) -> usize {
        let pos = self.read_position.load(std::sync::atomic::Ordering::Relaxed);
        if pos == usize::MAX { 0 } else { pos }
    }

    fn reset_sync(&mut self) {
        self.anchor_server_us = None;
        self.anchor_ring_pos = None;
        self.sync_active = false;
    }

    fn is_clock_synced(&self) -> bool {
        self.clock_sync
            .as_ref()
            .map_or(false, |cs| cs.lock().is_synchronized())
    }

    /// Try to establish the anchor: maps a Sendspin timestamp to a ring position.
    /// Called on the first chunk after clock_sync becomes valid AND read_pos is available.
    fn try_set_anchor(&mut self, chunk_timestamp: i64) {
        let read_pos = self.get_read_pos();
        if read_pos == 0 {
            return; // read_pos not yet available
        }
        self.anchor_server_us = Some(chunk_timestamp);
        self.anchor_ring_pos = Some(read_pos.wrapping_add(self.prebuffer_target));
        self.sync_active = true;
        self.write_pos = self.anchor_ring_pos.unwrap();
        info!(
            "sync anchored: server_us={}, ring_pos={}, read_pos={}",
            chunk_timestamp,
            self.anchor_ring_pos.unwrap(),
            read_pos,
        );
    }

    /// Compute the target ring position for a chunk based on its Sendspin timestamp.
    fn compute_target(&self, chunk_timestamp: i64) -> Option<usize> {
        let anchor_us = self.anchor_server_us?;
        let anchor_pos = self.anchor_ring_pos?;
        let delta_us = chunk_timestamp - anchor_us;
        let delta_samples = (delta_us * SAMPLE_RATE as i64 / 1_000_000) as isize;
        Some(anchor_pos.wrapping_add_signed(delta_samples))
    }

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
        self.reset_sync();
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
        info!("snapped to live: read_pos={}, write_pos={}", read_pos, self.write_pos);
        self.prebuffer_written = 0;
        self.state = BridgeState::Prebuffering;
        self.metrics.reset();
        info!("prebuffering {}ms ({} samples)", self.buffer_ms, self.prebuffer_target);
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
        self.prebuffer_written = 0;
        self.reset_sync();
        self.state = BridgeState::Rebuffering;
        self.metrics.reset();
    }

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
                    "write/read misalignment detected (write_pos={}, read_pos={}, distance={}), snapping to live",
                    self.write_pos, read_pos, distance
                );
                self.snap_to_live();
                // Re-anchor sync since we just moved
                self.reset_sync();
            }
        }

        // ── Sendspin timestamp sync ──
        if !self.sync_active {
            // Try to activate sync
            if self.is_clock_synced() && read_pos != 0 {
                self.try_set_anchor(chunk.timestamp);
            }
        }

        if self.sync_active {
            if let Some(target) = self.compute_target(chunk.timestamp) {
                let chunk_end = target.wrapping_add(frames);

                // Case: entirely consumed (target + frames <= read_pos)
                if wrapsub(chunk_end, read_pos) <= 0 {
                    debug!("dropped stale chunk (target={}, chunk_end={}, read_pos={})", target, chunk_end, read_pos);
                    return;
                }

                // Case: partially stale (target < read_pos < chunk_end)
                if wrapsub(target, read_pos) < 0 && wrapsub(chunk_end, read_pos) > 0 {
                    let trim = read_pos.wrapping_sub(target);
                    let remaining = frames - trim;
                    info!("trimming {} stale samples from chunk, writing {} at read_pos={}", trim, remaining, read_pos);
                    self.write_trimmed_samples(&channel_samples, trim, remaining, read_pos);
                    if wrapsub(chunk_end, self.write_pos) > 0 {
                        self.write_pos = chunk_end;
                    }
                    self.update_state_after_write(remaining, read_pos);
                    return;
                }

                // Case: large forward skip (target far ahead of write frontier)
                if wrapsub(target, self.write_pos) > (RING_BUFFER_SIZE / 2) as isize {
                    info!("large forward skip detected (target={}, write_pos={}), re-anchoring", target, self.write_pos);
                    self.snap_to_live();
                    self.try_set_anchor(chunk.timestamp);
                    return;
                }

                // Normal case: write at target
                self.write_samples_at(&channel_samples, frames, target);
                if wrapsub(chunk_end, self.write_pos) > 0 {
                    self.write_pos = chunk_end;
                }
                self.update_state_after_write(frames, read_pos);
                return;
            }
        }

        // ── Fallback: write sequentially (no sync yet) ──
        self.write_samples_at(&channel_samples, frames, self.write_pos);
        self.write_pos = self.write_pos.wrapping_add(frames);

        // WaitingForSubscriber timeout
        if self.state == BridgeState::WaitingForSubscriber {
            if read_pos != self.last_read_pos && read_pos != 0 {
                info!("subscriber detected (read_pos={}), snapping to live", read_pos);
                self.waiting_since = None;
                self.snap_to_live();
                self.reset_sync();
            } else if self.waiting_since.map_or(false, |t| t.elapsed().as_secs() >= 5) {
                info!("subscriber wait timed out (5s), entering prebuffering without alignment");
                self.waiting_since = None;
                self.prebuffer_written = 0;
                self.state = BridgeState::Prebuffering;
                self.metrics.reset();
            }
            self.last_read_pos = read_pos;
            return;
        }

        self.update_state_after_write(frames, read_pos);
    }

    /// Decode PCM chunk into per-channel sample vectors.
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

    /// Write all decoded samples at a specific ring position.
    fn write_samples_at(&mut self, channel_samples: &[Vec<Sample>], _frames: usize, pos: usize) {
        if let Some(inputs) = &mut self.rb_inputs {
            for (ch, samples) in channel_samples.iter().enumerate() {
                inputs[ch].write_from_at(pos, samples.iter().copied());
            }
        }
    }

    /// Write trimmed samples (skipping first `trim` samples) at a specific position.
    fn write_trimmed_samples(&mut self, channel_samples: &[Vec<Sample>], trim: usize, remaining: usize, pos: usize) {
        if let Some(inputs) = &mut self.rb_inputs {
            for (ch, samples) in channel_samples.iter().enumerate() {
                inputs[ch].write_from_at(pos, samples[trim..trim + remaining].iter().copied());
            }
        }
    }

    /// Common state update after writing frames.
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

    fn log_metrics(&mut self) {
        match self.state {
            BridgeState::Idle => {}
            BridgeState::WaitingForSubscriber => {
                info!("[buffer] waiting for DANTE subscriber");
            }
            BridgeState::Running => {
                let synced = if self.sync_active { "synced" } else { "fallback" };
                info!("[sync] mode={}", synced);
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

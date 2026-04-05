use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, RwLock};

use atomic::{Atomic, Ordering};
use inferno_aoip::device_server::{
    DeviceServer, ExternalBufferParameters, PositionReportDestination, Sample, Settings,
};
use log::{debug, error, info, warn};
use sendspin::protocol::client::AudioChunk;
use sendspin::protocol::messages::Message;
use sendspin::ProtocolClientBuilder;
use tokio::sync::oneshot;

use crate::metrics::BufferMetrics;

pub const CHANNELS: usize = 2;
pub const RING_BUFFER_SIZE: usize = 131072; // ~2.7s at 48kHz, power of 2
pub const SAMPLE_RATE: u32 = 48000;
const METRICS_INTERVAL_SECS: u64 = 5;

// ─── Bridge state machine ───────────────────────────────────────────
//
// Device + TX alive in ALL states (except before start_device).
// States only control what audio is in the ring buffer.

#[derive(Debug, Clone, PartialEq)]
enum BridgeState {
    /// Device + TX alive. Ring is explicitly zeroed (silence).
    Idle,
    /// Stream active, writing discardable scratch audio to ring.
    /// Waiting for read_pos to advance (subscriber + clock ready).
    WaitingForSubscriber,
    /// Subscriber detected, filling jitter buffer with fresh audio.
    Prebuffering,
    /// Actively transmitting live audio to subscriber.
    Running,
    /// Stream cleared (seek), realigning to live position.
    Rebuffering,
}

// ─── Ring buffer ────────────────────────────────────────────────────

struct AudioRingBuffer {
    buffers: Vec<Vec<Atomic<i32>>>,
    valid: Arc<RwLock<bool>>,
    write_pos: usize,
    pos_report_tab: Arc<Vec<AtomicUsize>>,
}

impl AudioRingBuffer {
    fn new() -> Self {
        let pos_report_tab = Arc::new(
            (0..CHANNELS)
                .map(|_| AtomicUsize::new(0))
                .collect::<Vec<_>>(),
        );
        let buffers = (0..CHANNELS)
            .map(|_| {
                (0..RING_BUFFER_SIZE)
                    .map(|_| Atomic::new(0i32))
                    .collect()
            })
            .collect();
        Self {
            buffers,
            valid: Arc::new(RwLock::new(true)),
            write_pos: 0,
            pos_report_tab,
        }
    }

    fn as_external_params(&self) -> Vec<ExternalBufferParameters<Sample>> {
        self.buffers
            .iter()
            .enumerate()
            .map(|(ch, buf)| {
                let pos_dest = PositionReportDestination::new(self.pos_report_tab.clone(), ch);
                unsafe {
                    ExternalBufferParameters::new(
                        buf.as_ptr(),
                        buf.len(),
                        1,
                        self.valid.clone(),
                        Some(pos_dest),
                    )
                }
            })
            .collect()
    }

    fn read_pos(&self) -> usize {
        self.pos_report_tab[0].load(std::sync::atomic::Ordering::Relaxed)
    }

    fn write_pcm_24bit_le(&mut self, data: &[u8]) -> usize {
        let bytes_per_sample = 3;
        let frame_size = bytes_per_sample * CHANNELS;
        let frames = data.len() / frame_size;
        for frame in 0..frames {
            let ring_pos = self.write_pos % RING_BUFFER_SIZE;
            let frame_offset = frame * frame_size;
            for ch in 0..CHANNELS {
                let offset = frame_offset + ch * bytes_per_sample;
                let b = &data[offset..offset + 3];
                let raw = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                let sign_extended = (raw << 8) >> 8;
                self.buffers[ch][ring_pos].store(sign_extended << 8, Ordering::Release);
            }
            self.write_pos = self.write_pos.wrapping_add(1);
        }
        frames
    }

    fn write_pcm_16bit_le(&mut self, data: &[u8]) -> usize {
        let bytes_per_sample = 2;
        let frame_size = bytes_per_sample * CHANNELS;
        let frames = data.len() / frame_size;
        for frame in 0..frames {
            let ring_pos = self.write_pos % RING_BUFFER_SIZE;
            let frame_offset = frame * frame_size;
            for ch in 0..CHANNELS {
                let offset = frame_offset + ch * bytes_per_sample;
                let b = &data[offset..offset + 2];
                let raw = i16::from_le_bytes([b[0], b[1]]) as i32;
                self.buffers[ch][ring_pos].store(raw << 16, Ordering::Release);
            }
            self.write_pos = self.write_pos.wrapping_add(1);
        }
        frames
    }

    /// Zero-fill a range of ring buffer positions [start, start+count).
    fn zero_range(&mut self, start: usize, count: usize) {
        for i in 0..count {
            let ring_pos = start.wrapping_add(i) % RING_BUFFER_SIZE;
            for ch in 0..CHANNELS {
                self.buffers[ch][ring_pos].store(0, Ordering::Release);
            }
        }
    }

    /// Zero the entire ring buffer. Used on StreamEnd and reconnect
    /// to ensure no stale audio loops via unconditional_read().
    fn zero_all(&mut self) {
        for ch in 0..CHANNELS {
            for i in 0..RING_BUFFER_SIZE {
                self.buffers[ch][i].store(0, Ordering::Release);
            }
        }
    }

    fn invalidate(&self) {
        if let Ok(mut v) = self.valid.write() {
            *v = false;
        }
    }
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
    ring_buffer: Option<AudioRingBuffer>,
    device_server: Option<DeviceServer>,
    current_timestamp: Arc<AtomicUsize>,
    // Stream state (reset per stream)
    prebuffer_target: usize,
    prebuffer_written: usize,
    stream_format: Option<StreamFormat>,
    metrics: BufferMetrics,
    last_read_pos: usize,
    /// When WaitingForSubscriber started (for timeout fallback).
    waiting_since: Option<std::time::Instant>,
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
            ring_buffer: None,
            device_server: None,
            current_timestamp: Arc::new(AtomicUsize::new(usize::MAX)),
            prebuffer_target,
            prebuffer_written: 0,
            stream_format: None,
            metrics: BufferMetrics::new(prebuffer_target),
            last_read_pos: 0,
            waiting_since: None,
        }
    }

    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Start DANTE device + TX once for the entire process lifetime.
        // This blocks until the PTP clock is available.
        self.start_device().await;

        // Outer reconnect loop for Sendspin
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

    /// Start DANTE device and TX. Called once at process startup.
    /// Blocks until PTP clock is available.
    async fn start_device(&mut self) {
        let mut ring_buffer = AudioRingBuffer::new();
        ring_buffer.zero_all(); // start silent
        let params = ring_buffer.as_external_params();

        let short_name = self.device_name.chars().take(14).collect::<String>();
        let mut config = std::collections::BTreeMap::new();
        config.insert("NAME".to_string(), self.device_name.clone());
        let mut settings = Settings::new(&self.device_name, &short_name, None, &config);
        settings.make_tx_channels(CHANNELS);
        settings.make_rx_channels(0);

        info!("starting DANTE device: {} (waiting for PTP clock...)", self.device_name);
        let mut server = DeviceServer::start(settings).await;
        info!("DANTE device started, clock ready");

        let (start_tx, start_rx) = oneshot::channel();
        self.current_timestamp
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);

        server
            .transmit_from_external_buffer(
                params,
                start_rx,
                self.current_timestamp.clone(),
                None,
            )
            .await;

        info!("FlowsTransmitter started (start_time=0, idle with silence)");
        let _ = start_tx.send(0);

        self.ring_buffer = Some(ring_buffer);
        self.device_server = Some(server);
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

        let (mut messages, mut audio, _clock_sync, _sender, _guard) = client.split();

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

                    // If already running with same format, treat as stream boundary
                    if self.state == BridgeState::Running
                        && self.stream_format.as_ref() == Some(&format)
                    {
                        info!("stream/start with same format: clearing and rebuffering");
                        self.clear_and_rebuffer();
                        return;
                    }

                    self.stream_format = Some(format);

                    // Check if subscriber is already active (pre-existing subscription)
                    let read_pos = self.ring_buffer.as_ref().map_or(0, |rb| rb.read_pos());
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

    /// Enter idle: zero ring, reset stream state. Device + TX stay alive.
    fn enter_idle(&mut self) {
        if let Some(rb) = &mut self.ring_buffer {
            rb.zero_all();
        }
        self.stream_format = None;
        self.prebuffer_written = 0;
        self.last_read_pos = 0;
        self.state = BridgeState::Idle;
        self.metrics.reset();
    }

    /// Snap write_pos to live position and enter prebuffering.
    fn snap_to_live(&mut self) {
        if let Some(rb) = &mut self.ring_buffer {
            let read_pos = rb.read_pos();
            rb.zero_range(read_pos, self.prebuffer_target);
            rb.write_pos = read_pos.wrapping_add(self.prebuffer_target);
            info!(
                "snapped to live: read_pos={}, write_pos={}",
                read_pos, rb.write_pos
            );
        }
        self.prebuffer_written = 0;
        self.state = BridgeState::Prebuffering;
        self.metrics.reset();
        info!(
            "prebuffering {}ms ({} samples)",
            self.buffer_ms, self.prebuffer_target
        );
    }

    /// Discard stale buffered audio and enter rebuffer mode.
    fn clear_and_rebuffer(&mut self) {
        if let Some(rb) = &mut self.ring_buffer {
            let read_pos = rb.read_pos();
            rb.zero_range(read_pos, self.prebuffer_target);
            rb.write_pos = read_pos.wrapping_add(self.prebuffer_target);
            info!(
                "cleared stale audio: zeroed [{}, +{}), write_pos={}",
                read_pos, self.prebuffer_target, rb.write_pos
            );
        }
        self.prebuffer_written = 0;
        self.state = BridgeState::Rebuffering;
        self.metrics.reset();
    }

    fn handle_audio(&mut self, chunk: AudioChunk) {
        let format = match &self.stream_format {
            Some(f) => f.clone(),
            None => return, // Idle or no format — drop silently
        };

        let rb = match &mut self.ring_buffer {
            Some(rb) => rb,
            None => return,
        };

        let frames_written = match format.bit_depth {
            24 => rb.write_pcm_24bit_le(&chunk.data),
            16 => rb.write_pcm_16bit_le(&chunk.data),
            _ => 0,
        };

        // In WaitingForSubscriber, check if read_pos started advancing.
        // If the clock never becomes available (common in Docker with fake clock),
        // fall back to Prebuffering after a timeout so audio still flows.
        if self.state == BridgeState::WaitingForSubscriber {
            let read_pos = self.ring_buffer.as_ref().unwrap().read_pos();
            if read_pos != self.last_read_pos && read_pos != 0 {
                // Subscriber active + clock working — snap to live
                info!("subscriber detected (read_pos={}), snapping to live", read_pos);
                self.waiting_since = None;
                self.snap_to_live();
            } else if self.waiting_since.map_or(false, |t| t.elapsed().as_secs() >= 5) {
                // Timeout: clock may not be delivering read_pos updates.
                // Fall back to prebuffering from current write_pos.
                info!("subscriber wait timed out (5s), entering prebuffering (clock may still be warming up)");
                self.waiting_since = None;
                self.prebuffer_written = 0;
                self.state = BridgeState::Prebuffering;
                self.metrics.reset();
            }
            self.last_read_pos = read_pos;
            return;
        }

        // Prebuffering/Rebuffering: accumulate fresh audio
        if self.state == BridgeState::Prebuffering || self.state == BridgeState::Rebuffering {
            self.prebuffer_written += frames_written;
            if self.prebuffer_written >= self.prebuffer_target {
                self.state = BridgeState::Running;
                if let Some(rb) = &self.ring_buffer {
                    let read = rb.read_pos();
                    let fill = rb.write_pos.wrapping_sub(read) as isize;
                    info!(
                        "prebuffer complete ({} samples), fill={}, now transmitting",
                        self.prebuffer_written, fill
                    );
                }
            }
        }

        // Update metrics
        if let Some(rb) = &self.ring_buffer {
            self.metrics.update(rb.write_pos, rb.read_pos());
        }
    }

    fn log_metrics(&mut self) {
        match self.state {
            BridgeState::Idle => {}
            BridgeState::WaitingForSubscriber => {
                info!("[buffer] waiting for DANTE subscriber");
            }
            BridgeState::Running => {
                if let Some(rb) = &self.ring_buffer {
                    self.metrics.log(rb.write_pos, rb.read_pos());
                }
            }
            _ => {}
        }
    }

    /// Full shutdown — only on process exit.
    async fn shutdown(&mut self) {
        if let Some(rb) = &self.ring_buffer {
            rb.invalidate();
        }
        if let Some(mut server) = self.device_server.take() {
            info!("stopping DANTE device");
            server.stop_transmitter().await;
            server.shutdown().await;
        }
        self.ring_buffer = None;
        self.state = BridgeState::Idle;
        info!("bridge shutdown complete");
    }
}

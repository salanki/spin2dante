use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use atomic::Atomic;
use inferno_aoip::device_server::{
    DeviceServer, OwnedBuffer, RBInput, Sample, Settings,
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
const HOLE_FIX_WAIT: usize = 4800; // ~100ms at 48kHz

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
    /// The actual ring buffer position the FlowsTransmitter reads from.
    /// This is `start_ts = next_ts + timestamp_shift` — the true consumer cursor.
    read_position: Arc<AtomicUsize>,
    // Stream state (reset per stream)
    write_pos: usize,
    prebuffer_target: usize,
    prebuffer_written: usize,
    stream_format: Option<StreamFormat>,
    metrics: BufferMetrics,
    last_read_pos: usize,
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

        // RBInput starts at position 0 with nothing written — readable_pos is 0.
        // FlowsTransmitter reads at 0 and gets zeros (hole_fix fills with default).

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

                    if self.state == BridgeState::Running
                        && self.stream_format.as_ref() == Some(&format)
                    {
                        info!("stream/start with same format: clearing and rebuffering");
                        self.clear_and_rebuffer();
                        return;
                    }

                    self.stream_format = Some(format);

                    // Check if subscriber is already active
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

    /// Get the actual ring buffer read position from the FlowsTransmitter.
    /// This is `start_ts = next_ts + timestamp_shift` — the true consumer cursor.
    /// Returns 0 if the transmitter hasn't started reading yet.
    fn get_read_pos(&self) -> usize {
        let pos = self.read_position.load(std::sync::atomic::Ordering::Relaxed);
        if pos == usize::MAX { 0 } else { pos }
    }

    fn enter_idle(&mut self) {
        // Write silence to clear stale audio. Write in two halves to avoid
        // triggering RBInput's assertion (input_len must be < items_size).
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
        self.state = BridgeState::Idle;
        self.metrics.reset();
    }

    fn snap_to_live(&mut self) {
        let read_pos = self.get_read_pos();
        // Write silence for the prebuffer region
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
            "cleared stale audio: zeroed [{}, +{}), write_pos={}",
            read_pos, self.prebuffer_target, self.write_pos
        );
        self.prebuffer_written = 0;
        self.state = BridgeState::Rebuffering;
        self.metrics.reset();
    }

    fn handle_audio(&mut self, chunk: AudioChunk) {
        let format = match &self.stream_format {
            Some(f) => f.clone(),
            None => return,
        };

        let inputs = match &mut self.rb_inputs {
            Some(inputs) => inputs,
            None => return,
        };

        // Decode PCM and write per-channel via RBInput::write_from_at
        let frames = match format.bit_depth {
            24 => {
                let bytes_per_sample = 3;
                let frame_size = bytes_per_sample * CHANNELS;
                let frames = chunk.data.len() / frame_size;
                let write_pos = self.write_pos;

                for ch in 0..CHANNELS {
                    let samples = (0..frames).map(|frame| {
                        let offset = frame * frame_size + ch * bytes_per_sample;
                        let b = &chunk.data[offset..offset + 3];
                        let raw = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                        let sign_extended = (raw << 8) >> 8;
                        sign_extended << 8
                    });
                    inputs[ch].write_from_at(write_pos, samples);
                }
                frames
            }
            16 => {
                let bytes_per_sample = 2;
                let frame_size = bytes_per_sample * CHANNELS;
                let frames = chunk.data.len() / frame_size;
                let write_pos = self.write_pos;

                for ch in 0..CHANNELS {
                    let samples = (0..frames).map(|frame| {
                        let offset = frame * frame_size + ch * bytes_per_sample;
                        let b = &chunk.data[offset..offset + 2];
                        let raw = i16::from_le_bytes([b[0], b[1]]) as i32;
                        raw << 16
                    });
                    inputs[ch].write_from_at(write_pos, samples);
                }
                frames
            }
            _ => 0,
        };

        self.write_pos = self.write_pos.wrapping_add(frames);

        // WaitingForSubscriber: check if read_pos started advancing
        if self.state == BridgeState::WaitingForSubscriber {
            let read_pos = self.get_read_pos();
            if read_pos != self.last_read_pos && read_pos != 0 {
                info!("subscriber detected (read_pos={}), snapping to live", read_pos);
                self.waiting_since = None;
                self.snap_to_live();
            } else if self.waiting_since.map_or(false, |t| t.elapsed().as_secs() >= 5) {
                info!("subscriber wait timed out (5s), entering prebuffering");
                self.waiting_since = None;
                self.prebuffer_written = 0;
                self.state = BridgeState::Prebuffering;
                self.metrics.reset();
            }
            self.last_read_pos = read_pos;
            return;
        }

        // Prebuffering/Rebuffering
        if self.state == BridgeState::Prebuffering || self.state == BridgeState::Rebuffering {
            self.prebuffer_written += frames;
            if self.prebuffer_written >= self.prebuffer_target {
                self.state = BridgeState::Running;
                let read_pos = self.get_read_pos();
                let fill = self.write_pos.wrapping_sub(read_pos) as isize;
                info!(
                    "prebuffer complete ({} samples), fill={}, read_pos={}, now transmitting",
                    self.prebuffer_written, fill, read_pos
                );
            }
        }

        // Update metrics
        self.metrics.update(self.write_pos, self.get_read_pos());
    }

    fn log_metrics(&mut self) {
        match self.state {
            BridgeState::Idle => {}
            BridgeState::WaitingForSubscriber => {
                info!("[buffer] waiting for DANTE subscriber");
            }
            BridgeState::Running => {
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

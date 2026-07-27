use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use atomic::Atomic;
use inferno_aoip::device_server::{
    DeviceServer, OwnedBuffer, RBInput, ReadPositionSnapshot, Sample, Settings,
};
use log::{debug, error, info, warn};
use sendspin::audio::sync_correction::{CorrectionPlanner, CorrectionSchedule};
use sendspin::protocol::client::{AudioChunk, Connection, WsSender};
use sendspin::protocol::messages::{
    AudioFormatSpec, ClientState, ClientSyncState, Message, PlayerCommandType, PlayerState,
    PlayerV1Support,
};
use sendspin::sync::clock::ClockSync;
use sendspin::{GainControl, ProtocolClientBuilder};
use tokio::sync::oneshot;

use crate::gain::BridgeGainRamp;
use crate::metrics::BufferMetrics;
use crate::VolumeControlMode;

pub const CHANNELS: usize = 2;
// Floor for the Dante playout ring. The actual size is derived per-bridge from
// server_buffer_ms (see `SendspinBridge::new`): the server's send-ahead lead must
// fit within one ring, because the scheduler horizon and the write/read
// realignment guard are both keyed off the ring size. 16384 ≈ 341ms at 48kHz.
pub const MIN_RING_BUFFER_SIZE: usize = 16384; // ~341ms at 48kHz, power of 2
pub const SAMPLE_RATE: u32 = 48000;
const METRICS_INTERVAL_SECS: u64 = 5;
const HOLE_FIX_WAIT: usize = 4800; // ~100ms at 48kHz

// Absolute backstop on the pending-queue chunk count, independent of duration.
// The real bound is duration/frames-based (see `max_pending_frames`, derived from
// server_buffer_ms); this only guards against a pathological flood of tiny chunks.
const MAX_PENDING_CHUNKS: usize = 4096;

fn wrapsub(a: usize, b: usize) -> isize {
    (a as isize).wrapping_sub(b as isize)
}

fn ms_to_samples(ms: u32) -> usize {
    (SAMPLE_RATE as usize * ms as usize) / 1000
}

fn micros_to_samples(delta_us: i64) -> isize {
    (delta_us * SAMPLE_RATE as i64 / 1_000_000) as isize
}

fn expected_read_pos(anchor_ring_pos: usize, prebuffer_target: usize, elapsed_us: i64) -> usize {
    anchor_ring_pos
        .wrapping_sub(prebuffer_target)
        .wrapping_add_signed(micros_to_samples(elapsed_us))
}

fn median_drift_sample(mut samples: [isize; 3]) -> isize {
    samples.sort_unstable();
    samples[1]
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

#[derive(Default)]
struct CorrectionState {
    schedule: CorrectionSchedule,
    insert_counter: u32,
    drop_counter: u32,
    last_frame: Option<[Sample; CHANNELS]>,
}

impl CorrectionState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn set_schedule(&mut self, schedule: CorrectionSchedule) {
        if schedule == self.schedule {
            return;
        }
        let continuing_insert =
            self.schedule.insert_every_n_frames > 0 && schedule.insert_every_n_frames > 0;
        let continuing_drop =
            self.schedule.drop_every_n_frames > 0 && schedule.drop_every_n_frames > 0;
        self.schedule = schedule;
        if continuing_insert {
            self.insert_counter = self
                .insert_counter
                .min(schedule.insert_every_n_frames)
                .max(1);
        } else {
            self.insert_counter = schedule.insert_every_n_frames;
        }
        if continuing_drop {
            self.drop_counter = self.drop_counter.min(schedule.drop_every_n_frames).max(1);
        } else {
            self.drop_counter = schedule.drop_every_n_frames;
        }
    }

    fn apply(&mut self, channel_samples: &mut [Vec<Sample>]) -> (usize, usize) {
        if channel_samples.len() != CHANNELS || channel_samples[0].is_empty() {
            return (0, 0);
        }
        let frames = channel_samples[0].len();
        if !self.schedule.is_correcting() || self.schedule.reanchor {
            self.last_frame = Some(std::array::from_fn(|ch| channel_samples[ch][frames - 1]));
            return (0, 0);
        }

        let mut corrected: [Vec<Sample>; CHANNELS] =
            std::array::from_fn(|_| Vec::with_capacity(frames + 1));
        let mut inserted = 0;
        let mut dropped = 0;

        for frame_index in 0..frames {
            if self.schedule.drop_every_n_frames > 0 {
                self.drop_counter = self.drop_counter.saturating_sub(1);
                if self.drop_counter == 0 {
                    self.drop_counter = self.schedule.drop_every_n_frames;
                    dropped += 1;
                    continue;
                }
            }

            if self.schedule.insert_every_n_frames > 0 {
                self.insert_counter = self.insert_counter.saturating_sub(1);
                if self.insert_counter == 0 {
                    self.insert_counter = self.schedule.insert_every_n_frames;
                    if let Some(last_frame) = self.last_frame {
                        for ch in 0..CHANNELS {
                            corrected[ch].push(last_frame[ch]);
                        }
                        inserted += 1;
                    }
                }
            }

            let frame = std::array::from_fn(|ch| channel_samples[ch][frame_index]);
            for ch in 0..CHANNELS {
                corrected[ch].push(frame[ch]);
            }
            self.last_frame = Some(frame);
        }

        for (destination, source) in channel_samples.iter_mut().zip(corrected) {
            *destination = source;
        }
        (inserted, dropped)
    }
}

// ─── Bridge ─────────────────────────────────────────────────────────

pub struct SendspinBridge {
    url: String,
    device_name: String,
    client_id: String,
    buffer_ms: u32,
    // Advertised Sendspin buffer_capacity expressed in ms of audio (send-ahead
    // credit the server may queue before throttling). Converted to bytes in the
    // PlayerV1Support handshake. See `max_pending_frames`.
    server_buffer_ms: u32,
    // Frames-based ceiling on the pending queue, derived from server_buffer_ms.
    // Bounds how much ahead-of-time audio (and thus RAM) we hold regardless of
    // chunk size, which is data-dependent.
    max_pending_frames: usize,
    // Dante playout ring size (power of 2), derived from server_buffer_ms so the
    // server's send-ahead lead fits within one ring. Used as the scheduler write
    // horizon and the write/read realignment threshold.
    ring_buffer_size: usize,
    dante_latency_ns: u32,
    drift_threshold_samples: usize,
    drift_check_interval: Duration,
    max_correction_samples: usize,
    correction_planner: CorrectionPlanner,
    correction_state: CorrectionState,
    drift_history: VecDeque<isize>,
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
    // Running total of frames held in pending_chunks (kept in sync with every
    // push/pop) so the queue can be bounded by duration, not chunk count.
    pending_frames: usize,
    // Server-now anchor: set once, maps server_time → ring_position.
    // All targets computed relative to this anchor for stable spacing.
    anchor_server_us: Option<i64>,
    anchor_ring_pos: Option<usize>,
    anchor_set_at: Option<Instant>,
    // Scheduler counters
    stale_drops: u64,
    trimmed_chunks: u64,
    trimmed_frames: u64,
    queued_high_water: usize,
    scheduler_settled: bool,
    // Cumulative frame-level corrections since process start. These counters
    // are intentionally not reset with the per-stream scheduler.
    drift_corrections: u64,
    drift_inserted_frames: u64,
    drift_dropped_frames: u64,
    rebuffers: u64,
    drift_checks_skipped: u64,
    // Volume control
    gain_control: Option<GainControl>,
    gain_ramp: BridgeGainRamp,
    // DANTE subscriber state reporting
    report_dante_subscriber: bool,
    sender: Option<WsSender>,
    has_subscriber: bool,
    last_read_pos_change: Option<Instant>,
    pending_sync_state: Option<ClientSyncState>,
    pending_player_state: bool,
    state_file: Option<std::path::PathBuf>,
}

impl SendspinBridge {
    pub fn new(
        url: String,
        device_name: String,
        buffer_ms: u32,
        server_buffer_ms: u32,
        dante_latency_ns: u32,
        drift_threshold_ms: u32,
        drift_check_interval_ms: u64,
        max_correction_samples_per_tick: usize,
        client_id: String,
        volume_control: VolumeControlMode,
        state_file: Option<std::path::PathBuf>,
        report_dante_subscriber: bool,
    ) -> Self {
        let prebuffer_target = ms_to_samples(buffer_ms);
        // Size the Dante ring so the largest healthy write/read distance fits
        // inside one ring. The scheduler only writes a chunk once its playout time
        // is within one ring horizon, and the write/read realignment guard snaps
        // when their distance exceeds the ring; both are keyed off the ring size,
        // so if the lead exceeds one ring the bridge thrashes (snap loop). The
        // anchor sits at read + prebuffer_target and chunks are placed up to the
        // send-ahead lead (≈ server_buffer_ms) beyond that, so the max distance is
        // prebuffer + lead. Use 2x that, rounded up to a power of 2 (inferno
        // requires pow2), with the historical 341ms ring as a floor.
        let ring_buffer_size = ((prebuffer_target + ms_to_samples(server_buffer_ms)) * 2)
            .next_power_of_two()
            .max(MIN_RING_BUFFER_SIZE);
        // Allow the pending queue to hold up to 2x the advertised send-ahead
        // (a well-behaved server stays within buffer_capacity; the extra factor
        // absorbs jitter) plus one ring horizon. This bounds RAM by duration
        // rather than by a fixed chunk count, which is data-dependent.
        let max_pending_frames = ms_to_samples(server_buffer_ms) * 2 + ring_buffer_size;
        let (gain_control, gain_ramp) = if volume_control == VolumeControlMode::Bridge {
            let vs = state_file
                .as_ref()
                .map(|p| crate::state::load(p))
                .unwrap_or_default();
            info!("initial volume: {}%, muted: {}", vs.volume, vs.muted);
            let gc = GainControl::new(vs.volume, vs.muted);
            let ramp = BridgeGainRamp::with_gain(gc.gain());
            (Some(gc), ramp)
        } else {
            (None, BridgeGainRamp::new())
        };
        Self {
            url,
            device_name,
            client_id,
            buffer_ms,
            server_buffer_ms,
            max_pending_frames,
            ring_buffer_size,
            dante_latency_ns,
            drift_threshold_samples: ms_to_samples(drift_threshold_ms),
            drift_check_interval: Duration::from_millis(drift_check_interval_ms),
            max_correction_samples: max_correction_samples_per_tick,
            correction_planner: CorrectionPlanner::new(),
            correction_state: CorrectionState::default(),
            drift_history: VecDeque::with_capacity(3),
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
            pending_frames: 0,
            anchor_server_us: None,
            anchor_ring_pos: None,
            anchor_set_at: None,
            stale_drops: 0,
            trimmed_chunks: 0,
            trimmed_frames: 0,
            queued_high_water: 0,
            scheduler_settled: false,
            drift_corrections: 0,
            drift_inserted_frames: 0,
            drift_dropped_frames: 0,
            rebuffers: 0,
            drift_checks_skipped: 0,
            gain_control,
            gain_ramp,
            report_dante_subscriber,
            sender: None,
            has_subscriber: false,
            last_read_pos_change: None,
            pending_sync_state: None,
            pending_player_state: false,
            state_file,
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
        config.insert("SAMPLE_RATE".to_string(), SAMPLE_RATE.to_string());
        config.insert(
            "TX_LATENCY_NS".to_string(),
            self.dante_latency_ns.to_string(),
        );
        config.insert(
            "RX_LATENCY_NS".to_string(),
            self.dante_latency_ns.to_string(),
        );
        let mut settings = Settings::new(&self.device_name, &short_name, None, &config);
        settings.make_tx_channels(CHANNELS);
        settings.make_rx_channels(0);
        // Sendspin sends interleaved stereo PCM (ch0=Left, ch1=Right).
        for (idx, name) in ["Left", "Right"].iter().enumerate() {
            *settings.self_info.tx_channels[idx]
                .friendly_name
                .write()
                .unwrap() = (*name).to_string();
        }

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
                self.ring_buffer_size,
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
            // buffer_capacity is the server-side send-ahead credit (bytes of
            // audio the server may queue before throttling) — NOT a prebuffer or
            // start delay (that is buffer_ms). A larger value lets the Sendspin
            // server (e.g. Music Assistant) run further ahead, absorbing its own
            // event-loop / writer stalls before it emits "Late binary … skipping"
            // drops. The send-ahead lead is bounded in the pending queue by
            // `max_pending_frames` and in the Dante ring by `ring_buffer_size`,
            // both derived from server_buffer_ms: `drain_pending` only writes a
            // chunk to the ring once its playout time is within one ring horizon,
            // and the ring is sized (≈ 2x (prebuffer + lead)) so the credit fits. If the
            // ring were too small for the lead, the write/read realignment guard
            // would snap repeatedly (a startup thrash loop), so the ring must
            // scale with the credit rather than the credit being capped to a
            // fixed ~341ms ring.
            //
            // We compute bytes at 24-bit depth (3 B/sample), the larger of the
            // two depths we offer. If the server negotiates 16-bit, the same byte
            // count is ~1.5x longer in time — i.e. always >= server_buffer_ms of
            // headroom, never less. u64 math avoids overflow at large values.
            let buffer_capacity =
                (SAMPLE_RATE as u64 * CHANNELS as u64 * 3 * self.server_buffer_ms as u64 / 1000)
                    as u32;
            info!(
                "advertising Sendspin buffer_capacity = {} bytes (~{} ms @24-bit)",
                buffer_capacity, self.server_buffer_ms
            );
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
                buffer_capacity,
                supported_commands: if self.gain_control.is_some() {
                    vec!["volume".to_string(), "mute".to_string()]
                } else {
                    vec![]
                },
            };
            let builder = ProtocolClientBuilder::builder()
                .client_id(self.client_id.clone())
                .name(self.device_name.clone())
                .product_name(Some("spin2dante".to_string()))
                .manufacturer(Some("spin2dante".to_string()))
                .software_version(Some(env!("CARGO_PKG_VERSION").to_string()))
                .player_v1_support(player_support);

            let connect_result = if let Some(player_state) = self.current_player_state() {
                builder
                    .initial_player_state(player_state)
                    .build()
                    .connect(&self.url)
                    .await
            } else {
                builder.build().connect(&self.url).await
            };

            match connect_result {
                Ok(client) => break client,
                Err(e) => {
                    warn!("connection failed: {e}, retrying in 2s...");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        };

        info!("connected to Sendspin server");

        let Connection {
            mut messages,
            mut audio,
            clock_sync,
            sender,
            guard: _guard,
            ..
        } = client.split();
        self.clock_sync = Some(clock_sync);
        self.sender = Some(sender);

        self.has_subscriber = false;
        self.last_read_pos_change = None;
        self.last_read_pos = self.get_read_pos();
        self.queue_sync_state(ClientSyncState::ExternalSource);

        let mut metrics_interval =
            tokio::time::interval(std::time::Duration::from_secs(METRICS_INTERVAL_SECS));
        let mut drift_interval = tokio::time::interval(self.drift_check_interval);

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
                _ = drift_interval.tick() => {
                    self.check_drift();
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("shutting down");
                    return Ok(());
                }
            }

            if let Some(state) = self.take_pending_client_state() {
                if let Some(ref sender) = self.sender {
                    let msg = Message::ClientState(state);
                    if let Err(e) = sender.send_message(msg).await {
                        warn!("failed to send client state: {e}");
                    }
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

                    self.queue_player_state();

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
                self.queue_player_state();
                info!(
                    "drift correction totals: drift_inserted_frames={} drift_dropped_frames={}",
                    self.drift_inserted_frames, self.drift_dropped_frames
                );
                info!("stream ended, entering idle (device stays on network)");
                self.enter_idle();
            }
            Message::StreamClear(_) => {
                self.queue_player_state();
                info!("stream cleared, discarding buffered audio");
                self.clear_and_rebuffer();
            }
            Message::ServerCommand(cmd) => {
                if let (Some(gc), Some(player_cmd)) = (self.gain_control.clone(), cmd.player) {
                    match player_cmd.command {
                        PlayerCommandType::Volume => {
                            if let Some(vol) = player_cmd.volume {
                                gc.set_volume(vol);
                                self.save_volume_state(&gc);
                                self.queue_player_state();
                                info!("bridge volume set to {}", vol);
                            }
                        }
                        PlayerCommandType::Mute => {
                            if let Some(muted) = player_cmd.mute {
                                gc.set_mute(muted);
                                self.save_volume_state(&gc);
                                self.queue_player_state();
                                info!("bridge mute set to {}", muted);
                            }
                        }
                        _ => {}
                    }
                }
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
        let now_us = sync.instant_to_client_micros(std::time::Instant::now());
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
        let client_us = sync.instant_to_client_micros(snapshot_instant);
        let server_us = sync.client_to_server_micros(client_us)?;
        Some((read_pos, server_us))
    }

    /// Pop the oldest pending chunk, keeping `pending_frames` in sync.
    fn pop_pending_front(&mut self) -> Option<PendingChunk> {
        let chunk = self.pending_chunks.pop_front();
        if let Some(ref c) = chunk {
            self.pending_frames = self.pending_frames.saturating_sub(c.frames);
        }
        chunk
    }

    fn reset_scheduler(&mut self) {
        self.pending_chunks.clear();
        self.pending_frames = 0;
        self.anchor_server_us = None;
        self.anchor_ring_pos = None;
        self.anchor_set_at = None;
        self.stale_drops = 0;
        self.trimmed_chunks = 0;
        self.trimmed_frames = 0;
        self.queued_high_water = 0;
        self.scheduler_settled = false;
        self.correction_state.reset();
        self.drift_history.clear();
        self.gain_ramp.reset_to_current();
    }

    fn count_rebuffer_if_running_anchored(&mut self) {
        if self.state == BridgeState::Running && self.anchor_server_us.is_some() {
            self.rebuffers += 1;
        }
    }

    fn check_drift(&mut self) {
        let (anchor_us, anchor_pos, anchor_set_at) = match (
            self.anchor_server_us,
            self.anchor_ring_pos,
            self.anchor_set_at,
        ) {
            (Some(anchor_us), Some(anchor_pos), Some(anchor_set_at)) => {
                (anchor_us, anchor_pos, anchor_set_at)
            }
            _ => return,
        };

        if anchor_set_at.elapsed() < Duration::from_secs(10) {
            return;
        }

        let Some((actual_read_pos, server_now_us)) = self.get_synced_pair() else {
            self.drift_checks_skipped += 1;
            return;
        };

        let elapsed_us = server_now_us - anchor_us;
        let expected_read_pos = expected_read_pos(anchor_pos, self.prebuffer_target, elapsed_us);
        let raw_drift_samples = wrapsub(actual_read_pos, expected_read_pos);

        // A ring-scale excursion means scheduler state is unsafe. Do not wait
        // for the median filter to fill before taking the rebuffer safety path.
        if raw_drift_samples.abs() > (self.ring_buffer_size / 4) as isize {
            warn!(
                "drift anomaly: raw={} samples actual_read_pos={} expected_read_pos={}, rebuffering",
                raw_drift_samples, actual_read_pos, expected_read_pos
            );
            self.count_rebuffer_if_running_anchored();
            self.clear_and_rebuffer();
            return;
        }

        self.drift_history.push_back(raw_drift_samples);
        if self.drift_history.len() > 3 {
            self.drift_history.pop_front();
        }
        if self.drift_history.len() < 3 {
            debug!(
                "collecting drift filter samples: raw_drift={} count={}/3",
                raw_drift_samples,
                self.drift_history.len(),
            );
            return;
        }
        let drift_samples = median_drift_sample([
            self.drift_history[0],
            self.drift_history[1],
            self.drift_history[2],
        ]);

        if drift_samples.abs() > (self.ring_buffer_size / 4) as isize {
            warn!(
                "drift anomaly: filtered={} raw={} samples actual_read_pos={} expected_read_pos={}, rebuffering",
                drift_samples, raw_drift_samples, actual_read_pos, expected_read_pos
            );
            self.count_rebuffer_if_running_anchored();
            self.clear_and_rebuffer();
            return;
        }

        let currently_correcting = self.correction_state.schedule.is_correcting();
        if !currently_correcting && drift_samples.abs() as usize <= self.drift_threshold_samples {
            debug!(
                "drift within threshold: filtered={} raw={} samples threshold={} actual_read_pos={} expected_read_pos={}",
                drift_samples,
                raw_drift_samples,
                self.drift_threshold_samples,
                actual_read_pos,
                expected_read_pos
            );
            return;
        }

        // CorrectionPlanner's positive error means content is late and must be
        // dropped. Here positive drift means the Dante read head is ahead of
        // the server timeline, so content is early and must be repeated.
        let planner_error_us =
            -(drift_samples as i64).saturating_mul(1_000_000) / SAMPLE_RATE as i64;
        let mut schedule =
            self.correction_planner
                .plan(planner_error_us, SAMPLE_RATE, currently_correcting);

        if schedule.reanchor {
            warn!(
                "drift planner requested reanchor: drift={} samples error_us={}, rebuffering",
                drift_samples, planner_error_us
            );
            self.count_rebuffer_if_running_anchored();
            self.clear_and_rebuffer();
            return;
        }

        // Preserve the CLI's maximum-correction budget as an average cadence.
        // For example, 12 samples per 250ms permits at most 48 corrections/s,
        // or one correction per 1000 frames at 48kHz.
        if self.max_correction_samples > 0 {
            let max_per_second =
                self.max_correction_samples as f64 / self.drift_check_interval.as_secs_f64();
            let min_interval = (SAMPLE_RATE as f64 / max_per_second).ceil() as u32;
            if schedule.insert_every_n_frames > 0 {
                schedule.insert_every_n_frames = schedule.insert_every_n_frames.max(min_interval);
            }
            if schedule.drop_every_n_frames > 0 {
                schedule.drop_every_n_frames = schedule.drop_every_n_frames.max(min_interval);
            }
        } else {
            schedule = CorrectionSchedule::default();
        }

        if schedule != self.correction_state.schedule {
            info!(
                "drift correction schedule: filtered={} raw={} samples error_us={} insert_every={} drop_every={}",
                drift_samples,
                raw_drift_samples,
                planner_error_us,
                schedule.insert_every_n_frames,
                schedule.drop_every_n_frames,
            );
            self.correction_state.set_schedule(schedule);
        }
    }

    // ─── State transitions ──────────────────────────────────────────

    fn queue_sync_state(&mut self, state: ClientSyncState) {
        if !self.report_dante_subscriber {
            return;
        }
        info!("reporting player sync state: {:?}", state);
        self.pending_sync_state = Some(state);
    }

    fn save_volume_state(&self, gc: &GainControl) {
        if let Some(ref path) = self.state_file {
            crate::state::save(path, gc.volume(), gc.is_muted());
        }
    }

    fn queue_player_state(&mut self) {
        if self.gain_control.is_some() {
            self.pending_player_state = true;
        }
    }

    fn current_player_state(&self) -> Option<PlayerState> {
        self.gain_control.as_ref().map(|gc| PlayerState {
            volume: Some(gc.volume()),
            muted: Some(gc.is_muted()),
            static_delay_ms: None,
            supported_commands: None,
        })
    }

    fn take_pending_client_state(&mut self) -> Option<ClientState> {
        let state = self.pending_sync_state.take();
        let include_player = self.pending_player_state || state.is_some();
        self.pending_player_state = false;

        if state.is_none() && !include_player {
            return None;
        }

        Some(ClientState {
            state,
            player: if include_player {
                self.current_player_state()
            } else {
                None
            },
        })
    }

    fn enter_idle(&mut self) {
        let ring = self.ring_buffer_size;
        if let Some(inputs) = &mut self.rb_inputs {
            let half = ring / 2;
            for rb in inputs.iter_mut() {
                let silence: Vec<Sample> = vec![0; half];
                rb.write_from_at(self.write_pos, silence.clone().into_iter());
                rb.write_from_at(self.write_pos.wrapping_add(half), silence.into_iter());
            }
            self.write_pos = self.write_pos.wrapping_add(ring);
        }
        self.stream_format = None;
        self.prebuffer_written = 0;
        self.last_read_pos = 0;
        self.has_subscriber = false;
        self.last_read_pos_change = None;
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

        if self.report_dante_subscriber
            && self.has_subscriber
            && read_pos != 0
            && read_pos != self.last_read_pos
        {
            self.last_read_pos_change = Some(Instant::now());
            self.last_read_pos = read_pos;
        }

        // ── Auto-realignment: detect PTP domain mismatch ──
        if read_pos != 0 {
            let distance = if wrapsub(self.write_pos, read_pos) > 0 {
                wrapsub(self.write_pos, read_pos) as usize
            } else {
                wrapsub(read_pos, self.write_pos) as usize
            };
            if distance > self.ring_buffer_size {
                info!(
                    "write/read misalignment (write={}, read={}, dist={}), snapping",
                    self.write_pos, read_pos, distance
                );
                self.count_rebuffer_if_running_anchored();
                self.snap_to_live();
                self.reset_scheduler();
            }
        }

        // Enqueue the chunk (bounded: drop oldest if queue overflows). The bound
        // is primarily duration-based (max_pending_frames, from server_buffer_ms);
        // MAX_PENDING_CHUNKS is an absolute backstop against tiny-chunk floods.
        self.pending_chunks.push_back(PendingChunk {
            timestamp_us: chunk.timestamp,
            frames,
            channel_samples,
        });
        self.pending_frames += frames;
        while self.pending_frames > self.max_pending_frames
            || self.pending_chunks.len() > MAX_PENDING_CHUNKS
        {
            match self.pop_pending_front() {
                Some(dropped) => {
                    self.stale_drops += 1;
                    if let Some(gc) = &self.gain_control {
                        self.gain_ramp.advance(dropped.frames, gc.gain());
                    }
                }
                None => break,
            }
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
                    self.anchor_set_at = Some(Instant::now());
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
        // Corrections change the anchor while this loop is draining. Keep this
        // local copy current so every subsequent queued chunk is targeted after
        // the inserted/dropped frame instead of overwriting it.
        let mut anchor_pos = self.anchor_ring_pos.unwrap();

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
            if distance_from_read > (self.ring_buffer_size - chunk.frames) as isize {
                break; // leave queued, try next drain
            }

            // Entirely stale: chunk_end behind read_pos
            if wrapsub(chunk_end, read_pos) <= 0 {
                self.stale_drops += 1;
                if let Some(gc) = &self.gain_control {
                    self.gain_ramp.advance(chunk.frames, gc.gain());
                }
                debug!(
                    "dropped stale chunk: ts={}, target={}, read_pos={}",
                    chunk.timestamp_us, target, read_pos
                );
                self.pop_pending_front();
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
                let mut chunk = self.pop_pending_front().unwrap();
                self.correction_state.last_frame = Some(std::array::from_fn(|ch| {
                    chunk.channel_samples[ch][chunk.frames - 1]
                }));
                self.write_trimmed_samples(&mut chunk.channel_samples, trim, remaining, read_pos);
                if wrapsub(chunk_end, self.write_pos) > 0 {
                    self.write_pos = chunk_end;
                }
                self.update_state_after_write(remaining, read_pos);
                continue;
            }

            // Large gap handling
            if wrapsub(target, self.write_pos) > (self.ring_buffer_size / 2) as isize {
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
                    self.count_rebuffer_if_running_anchored();
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
                self.count_rebuffer_if_running_anchored();
                self.clear_and_rebuffer();
                break;
            } else if backward > 0 {
                debug!(
                    "backward jitter: target={} behind write_pos={} by {} samples",
                    target, self.write_pos, backward
                );
            }

            // Normal: write chunk at target
            let mut chunk = self.pop_pending_front().unwrap();
            let (inserted, dropped) = self.correction_state.apply(&mut chunk.channel_samples);
            let corrected_frames = chunk.frames + inserted - dropped;
            self.write_samples_at(&mut chunk.channel_samples, corrected_frames, target);

            let net_correction = inserted as isize - dropped as isize;
            if net_correction != 0 {
                anchor_pos = anchor_pos.wrapping_add_signed(net_correction);
                self.anchor_ring_pos = Some(anchor_pos);
                self.drift_corrections += net_correction.unsigned_abs() as u64;
                self.drift_inserted_frames += inserted as u64;
                self.drift_dropped_frames += dropped as u64;
                debug!(
                    "drift correction applied: inserted={} dropped={} anchor_ring_pos={} total={}",
                    inserted, dropped, anchor_pos, self.drift_corrections,
                );
            }

            let corrected_end = target.wrapping_add(corrected_frames);
            if wrapsub(corrected_end, self.write_pos) > 0 {
                self.write_pos = corrected_end;
            }
            self.update_state_after_write(corrected_frames, read_pos);
            if !self.scheduler_settled {
                self.scheduler_settled = true;
            }
        }
    }

    /// Fallback: write pending chunks sequentially (clock sync not ready or read_pos=0).
    fn drain_sequential(&mut self) {
        let read_pos = self.get_read_pos();
        // No scheduler anchor means no correction schedule can be active, so
        // sequential writes intentionally bypass CorrectionState.
        while let Some(mut chunk) = self.pop_pending_front() {
            self.write_samples_at(&mut chunk.channel_samples, chunk.frames, self.write_pos);
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

    fn write_samples_at(&mut self, channel_samples: &mut [Vec<Sample>], frames: usize, pos: usize) {
        if let Some(gc) = &self.gain_control {
            self.gain_ramp.apply(channel_samples, frames, gc.gain());
        }
        if let Some(inputs) = &mut self.rb_inputs {
            for (ch, samples) in channel_samples.iter().enumerate() {
                inputs[ch].write_from_at(pos, samples.iter().copied());
            }
        }
    }

    fn write_trimmed_samples(
        &mut self,
        channel_samples: &mut [Vec<Sample>],
        trim: usize,
        remaining: usize,
        pos: usize,
    ) {
        if let Some(gc) = &self.gain_control {
            let target = gc.gain();
            self.gain_ramp.advance(trim, target);
            self.gain_ramp
                .apply_range(channel_samples, trim, remaining, target);
        }
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
                    "[sync] mode={} pending={} stale_drops={} trims={}/{} high_water={} drift_corrections={} drift_inserted_frames={} drift_dropped_frames={} rebuffers={} drift_checks_skipped={}",
                    mode,
                    self.pending_chunks.len(),
                    self.stale_drops,
                    self.trimmed_chunks,
                    self.trimmed_frames,
                    self.queued_high_water,
                    self.drift_corrections,
                    self.drift_inserted_frames,
                    self.drift_dropped_frames,
                    self.rebuffers,
                    self.drift_checks_skipped,
                );
                self.metrics.log(self.write_pos, self.get_read_pos());
            }
            _ => {}
        }

        if self.report_dante_subscriber {
            let read_pos = self.get_read_pos();

            if !self.has_subscriber && read_pos != 0 && read_pos != self.last_read_pos {
                self.has_subscriber = true;
                self.last_read_pos_change = Some(Instant::now());
                self.last_read_pos = read_pos;
                info!("DANTE subscriber detected (read_pos={})", read_pos);
                self.queue_sync_state(ClientSyncState::Synchronized);
            } else if self.has_subscriber && read_pos != 0 {
                if read_pos != self.last_read_pos {
                    self.last_read_pos_change = Some(Instant::now());
                    self.last_read_pos = read_pos;
                } else if let Some(last_change) = self.last_read_pos_change {
                    if last_change.elapsed() > Duration::from_secs(10) {
                        warn!("DANTE subscriber appears lost (read_pos stale for >10s)");
                        self.has_subscriber = false;
                        self.queue_sync_state(ClientSyncState::ExternalSource);
                    }
                }
            } else if self.has_subscriber && read_pos == 0 {
                self.last_read_pos_change = None;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_read_pos_tracks_elapsed_server_time() {
        let anchor_ring_pos = 10_000usize;
        let prebuffer_target = ms_to_samples(5);
        let expected = expected_read_pos(anchor_ring_pos, prebuffer_target, 1_000_000);
        assert_eq!(
            expected,
            anchor_ring_pos
                .wrapping_sub(prebuffer_target)
                .wrapping_add(SAMPLE_RATE as usize)
        );
    }

    #[test]
    fn drift_median_rejects_one_direction_reversing_outlier() {
        assert_eq!(median_drift_sample([66, -378, 117]), 66);
        assert_eq!(median_drift_sample([-66, 378, -117]), -66);
    }

    #[test]
    fn correction_state_repeats_one_complete_stereo_frame() {
        let mut state = CorrectionState::default();
        state.last_frame = Some([9, 90]);
        state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 2,
            drop_every_n_frames: 0,
            reanchor: false,
        });
        let mut channels = vec![vec![10, 11, 12], vec![100, 110, 120]];

        let (inserted, dropped) = state.apply(&mut channels);

        assert_eq!((inserted, dropped), (1, 0));
        assert_eq!(channels[0], vec![10, 10, 11, 12]);
        assert_eq!(channels[1], vec![100, 100, 110, 120]);
    }

    #[test]
    fn correction_state_drops_one_complete_stereo_frame() {
        let mut state = CorrectionState::default();
        state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 0,
            drop_every_n_frames: 2,
            reanchor: false,
        });
        let mut channels = vec![vec![10, 11, 12], vec![100, 110, 120]];

        let (inserted, dropped) = state.apply(&mut channels);

        assert_eq!((inserted, dropped), (0, 1));
        assert_eq!(channels[0], vec![10, 12]);
        assert_eq!(channels[1], vec![100, 120]);
    }

    #[test]
    fn correction_counter_continues_across_chunks() {
        let mut state = CorrectionState::default();
        state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 3,
            drop_every_n_frames: 0,
            reanchor: false,
        });
        let mut first = vec![vec![1, 2], vec![10, 20]];
        let mut second = vec![vec![3, 4], vec![30, 40]];

        assert_eq!(state.apply(&mut first), (0, 0));
        assert_eq!(state.apply(&mut second), (1, 0));
        assert_eq!(second[0], vec![2, 3, 4]);
        assert_eq!(second[1], vec![20, 30, 40]);
    }

    #[test]
    fn same_direction_schedule_update_preserves_counter_progress() {
        let mut state = CorrectionState::default();
        state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 5,
            drop_every_n_frames: 0,
            reanchor: false,
        });
        let mut first = vec![vec![1, 2], vec![10, 20]];
        assert_eq!(state.apply(&mut first), (0, 0));

        state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 6,
            drop_every_n_frames: 0,
            reanchor: false,
        });
        let mut second = vec![vec![3, 4, 5], vec![30, 40, 50]];

        assert_eq!(state.apply(&mut second), (1, 0));
        assert_eq!(second[0], vec![3, 4, 4, 5]);
        assert_eq!(second[1], vec![30, 40, 40, 50]);
    }

    #[test]
    fn correction_direction_reversal_resets_the_new_direction_counter() {
        let mut state = CorrectionState::default();
        state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 5,
            drop_every_n_frames: 0,
            reanchor: false,
        });
        let mut first = vec![vec![1, 2], vec![10, 20]];
        assert_eq!(state.apply(&mut first), (0, 0));

        state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 0,
            drop_every_n_frames: 4,
            reanchor: false,
        });

        assert_eq!(state.insert_counter, 0);
        assert_eq!(state.drop_counter, 4);
    }

    #[test]
    fn first_insert_without_a_previous_frame_is_skipped() {
        let mut state = CorrectionState::default();
        state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 1,
            drop_every_n_frames: 0,
            reanchor: false,
        });
        let mut channels = vec![vec![10], vec![100]];

        assert_eq!(state.apply(&mut channels), (0, 0));
        assert_eq!(channels, vec![vec![10], vec![100]]);
        assert_eq!(state.last_frame, Some([10, 100]));
    }

    #[test]
    fn multi_chunk_drain_keeps_corrected_targets_contiguous() {
        let mut bridge = SendspinBridge::new(
            "ws://unused".to_string(),
            "test".to_string(),
            5,
            2_000,
            1_000_000,
            5,
            1_000,
            48,
            "test-client".to_string(),
            VolumeControlMode::None,
            None,
            false,
        );
        let anchor_us = 1_000_000;
        let anchor_pos = 10_000;
        let chunk_frames = 480;
        bridge.anchor_server_us = Some(anchor_us);
        bridge.anchor_ring_pos = Some(anchor_pos);
        bridge.anchor_set_at = Some(Instant::now());
        bridge.write_pos = anchor_pos;
        bridge.scheduler_settled = true;
        bridge.state = BridgeState::Running;
        bridge.correction_state.last_frame = Some([0, 0]);
        bridge.correction_state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: chunk_frames as u32,
            drop_every_n_frames: 0,
            reanchor: false,
        });
        for chunk_index in 0..2 {
            bridge.pending_chunks.push_back(PendingChunk {
                timestamp_us: anchor_us + chunk_index * 10_000,
                frames: chunk_frames,
                channel_samples: vec![vec![1; chunk_frames], vec![2; chunk_frames]],
            });
            bridge.pending_frames += chunk_frames;
        }

        bridge.drain_pending(anchor_pos - 100);

        assert!(bridge.pending_chunks.is_empty());
        assert_eq!(bridge.drift_inserted_frames, 2);
        assert_eq!(bridge.anchor_ring_pos, Some(anchor_pos + 2));
        assert_eq!(bridge.write_pos, anchor_pos + 2 * chunk_frames + 2);
    }

    #[test]
    fn trimmed_chunk_refreshes_the_previous_pre_gain_frame() {
        let mut bridge = SendspinBridge::new(
            "ws://unused".to_string(),
            "test".to_string(),
            5,
            2_000,
            1_000_000,
            5,
            1_000,
            48,
            "test-client".to_string(),
            VolumeControlMode::Bridge,
            None,
            false,
        );
        let gain_control = GainControl::new(50, false);
        let gain = gain_control.gain();
        bridge.gain_ramp = BridgeGainRamp::with_gain(gain);
        bridge.gain_control = Some(gain_control);
        let anchor_us = 1_000_000;
        let anchor_pos = 10_000;
        let frames = 480;
        bridge.anchor_server_us = Some(anchor_us);
        bridge.anchor_ring_pos = Some(anchor_pos);
        bridge.anchor_set_at = Some(Instant::now());
        bridge.write_pos = anchor_pos + 10;
        bridge.scheduler_settled = true;
        bridge.state = BridgeState::Running;
        bridge.correction_state.last_frame = Some([-1, -2]);
        bridge.pending_chunks.push_back(PendingChunk {
            timestamp_us: anchor_us,
            frames,
            channel_samples: vec![vec![10_000; frames], vec![20_000; frames]],
        });
        bridge.pending_frames = frames;

        bridge.drain_pending(anchor_pos + 10);

        assert!(bridge.pending_chunks.is_empty());
        assert_eq!(bridge.correction_state.last_frame, Some([10_000, 20_000]));

        bridge.correction_state.set_schedule(CorrectionSchedule {
            insert_every_n_frames: 1,
            drop_every_n_frames: 0,
            reanchor: false,
        });
        let mut next = vec![vec![30_000], vec![40_000]];
        assert_eq!(bridge.correction_state.apply(&mut next), (1, 0));
        bridge.gain_ramp.apply(&mut next, 2, gain);
        assert_eq!(next[0][0], (10_000.0 * gain as f64) as Sample);
        assert_eq!(next[1][0], (20_000.0 * gain as f64) as Sample);
    }
}

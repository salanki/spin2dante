use clap::Parser;
use log::info;
use std::env;

mod bridge;
mod gain;
mod metrics;
mod state;

#[derive(Clone, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum VolumeControlMode {
    #[default]
    None,
    Bridge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum DanteLatency {
    #[value(name = "0.5")]
    Ms0_5,
    #[value(name = "1")]
    Ms1,
    #[value(name = "2")]
    Ms2,
    #[value(name = "5")]
    Ms5,
    #[value(name = "10")]
    Ms10,
    #[value(name = "20")]
    Ms20,
}

impl DanteLatency {
    pub fn as_ns(self) -> u32 {
        match self {
            Self::Ms0_5 => 500_000,
            Self::Ms1 => 1_000_000,
            Self::Ms2 => 2_000_000,
            Self::Ms5 => 5_000_000,
            Self::Ms10 => 10_000_000,
            Self::Ms20 => 20_000_000,
        }
    }
}

impl std::fmt::Display for DanteLatency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ms0_5 => write!(f, "0.5ms"),
            Self::Ms1 => write!(f, "1ms"),
            Self::Ms2 => write!(f, "2ms"),
            Self::Ms5 => write!(f, "5ms"),
            Self::Ms10 => write!(f, "10ms"),
            Self::Ms20 => write!(f, "20ms"),
        }
    }
}

const ABOUT: &str = "spin2dante
Copyright (C) 2025

Bridges Sendspin audio streams (e.g., from Music Assistant) to DANTE
network audio using inferno_aoip. Stereo (2-channel) only.

Receives audio via WebSocket from a Sendspin server and transmits it
as a DANTE device on the local network. Bit-perfect for PCM (16/24-bit).

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License (v3+) or the
GNU Affero General Public License (v3+).
";

#[derive(Parser, Debug)]
#[command(author, version, about = ABOUT, long_about = None)]
struct Args {
    /// Sendspin server WebSocket URL (e.g., ws://192.168.1.100:8927)
    #[arg(long, short)]
    url: String,

    /// DANTE device name visible on the network
    #[arg(long, short, default_value = "Sendspin Bridge")]
    name: String,

    /// Playout buffer / latency in milliseconds.
    ///
    /// Larger values improve jitter tolerance, but they also delay audio by
    /// that amount. Bridges that should remain in sync should use the same
    /// buffer_ms value.
    #[arg(long, default_value_t = 5)]
    buffer_ms: u32,

    /// Trigger in-place drift correction once offset exceeds this many ms.
    #[arg(long, default_value_t = 5)]
    drift_threshold_ms: u32,

    /// How often to sample clock drift and evaluate correction.
    #[arg(long, default_value_t = 1000)]
    drift_check_interval_ms: u64,

    /// Maximum anchor shift to apply in one drift-correction tick.
    #[arg(long, default_value_t = 48)]
    max_correction_samples_per_tick: usize,

    /// Stable Sendspin client ID. If omitted, derived from name (+ INFERNO_PROCESS_ID if set).
    #[arg(long)]
    client_id: Option<String>,

    /// Volume control mode: "none" (default, no volume capability) or
    /// "bridge" (software gain applied inside the bridge).
    #[arg(long, value_enum, default_value_t = VolumeControlMode::None)]
    volume_control: VolumeControlMode,

    /// Path to persist volume/mute state across restarts.
    /// Only used when --volume-control=bridge.
    #[arg(long)]
    state_file: Option<String>,

    /// DANTE transmit latency in milliseconds. Advertised to receivers and
    /// determines their minimum playout buffer. Standard Dante values only.
    #[arg(long, value_enum, default_value = "10")]
    dante_latency: DanteLatency,

    /// Report DANTE receiver subscription state to the Sendspin server.
    ///
    /// When enabled, the bridge sends ExternalSource while no DANTE receiver
    /// is subscribed, causing Music Assistant to remove the player from its
    /// group. Once a receiver subscribes, it sends Synchronized so the player
    /// can rejoin. Disabled by default.
    #[arg(long, default_value_t = false)]
    report_dante_subscriber: bool,
}

#[tokio::main]
async fn main() {
    let logenv = env_logger::Env::default().default_filter_or("info");
    env_logger::init_from_env(logenv);

    let args = Args::parse();

    let state_file = if args.volume_control == VolumeControlMode::Bridge {
        args.state_file.map(std::path::PathBuf::from)
    } else {
        if args.state_file.is_some() {
            log::warn!("--state-file is ignored when --volume-control is not 'bridge'");
        }
        None
    };

    info!(
        "spin2dante starting: url={} name={} buffer={}ms dante_latency={} drift_threshold={}ms drift_check_interval={}ms max_correction={}samples volume_control={:?} state_file={:?} report_dante_subscriber={}",
        args.url,
        args.name,
        args.buffer_ms,
        args.dante_latency,
        args.drift_threshold_ms,
        args.drift_check_interval_ms,
        args.max_correction_samples_per_tick,
        args.volume_control,
        state_file,
        args.report_dante_subscriber,
    );

    let client_id = args
        .client_id
        .unwrap_or_else(|| derive_client_id(&args.name));
    info!("using Sendspin client_id={}", client_id);

    let mut bridge = bridge::SendspinBridge::new(
        args.url,
        args.name,
        args.buffer_ms,
        args.dante_latency.as_ns(),
        args.drift_threshold_ms,
        args.drift_check_interval_ms,
        args.max_correction_samples_per_tick,
        client_id,
        args.volume_control,
        state_file,
        args.report_dante_subscriber,
    );

    if let Err(e) = bridge.run().await {
        log::error!("bridge error: {e}");
        std::process::exit(1);
    }
}

fn derive_client_id(name: &str) -> String {
    let mut material = format!("spin2dante:{}", name);
    if let Ok(process_id) = env::var("INFERNO_PROCESS_ID") {
        material.push(':');
        material.push_str(&process_id);
    }

    let hash = fnv1a64(material.as_bytes());
    format!("spin2dante-{hash:016x}")
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

use clap::Parser;
use log::info;
use std::env;

mod bridge;
mod metrics;

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

    /// Jitter buffer size in milliseconds
    #[arg(long, default_value_t = 50)]
    buffer_ms: u32,

    /// Stable Sendspin client ID. If omitted, derived from name (+ INFERNO_PROCESS_ID if set).
    #[arg(long)]
    client_id: Option<String>,
}

#[tokio::main]
async fn main() {
    let logenv = env_logger::Env::default().default_filter_or("info");
    env_logger::init_from_env(logenv);

    let args = Args::parse();

    info!(
        "spin2dante starting: url={} name={} buffer={}ms",
        args.url, args.name, args.buffer_ms
    );

    let client_id = args.client_id.unwrap_or_else(|| derive_client_id(&args.name));
    info!("using Sendspin client_id={}", client_id);

    let mut bridge = bridge::SendspinBridge::new(args.url, args.name, args.buffer_ms, client_id);

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

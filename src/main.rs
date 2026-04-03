use clap::Parser;
use log::info;

mod bridge;
mod metrics;

const ABOUT: &str = "Sendspin-to-DANTE Bridge
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
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let logenv = env_logger::Env::default().default_filter_or("info");
    env_logger::init_from_env(logenv);

    let args = Args::parse();

    info!(
        "sendspin_bridge starting: url={} name={} buffer={}ms",
        args.url, args.name, args.buffer_ms
    );

    let mut bridge = bridge::SendspinBridge::new(args.url, args.name, args.buffer_ms);

    if let Err(e) = bridge.run().await {
        log::error!("bridge error: {e}");
        std::process::exit(1);
    }
}

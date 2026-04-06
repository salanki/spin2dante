#!/usr/bin/env python3
"""
Minimal Sendspin test server that exercises bridge resilience scenarios.

Scenarios run in sequence:
1. Normal stream (5s) → stream/end → pause → new stream/start (5s)
2. Mid-stream clear (seek simulation) → continue with fresh audio
3. Server shutdown → bridge should reconnect

The server generates a 1kHz sine tone and logs each protocol event.
"""

import asyncio
import json
import math
import struct
import sys
import time

import websockets

SAMPLE_RATE = 48000
CHANNELS = 2
BIT_DEPTH = 24
BYTES_PER_SAMPLE = 3
FRAME_SIZE = BYTES_PER_SAMPLE * CHANNELS
# Send 10ms chunks (480 frames)
CHUNK_FRAMES = 480
CHUNK_BYTES = CHUNK_FRAMES * FRAME_SIZE

PORT = 8927
FREQ = 1000
AMPLITUDE = 0x3FFFFF  # ~half of 24-bit max


def generate_sine_chunk(start_frame: int, frames: int) -> bytes:
    """Generate interleaved 24-bit LE stereo PCM sine wave."""
    data = bytearray(frames * FRAME_SIZE)
    for i in range(frames):
        val = int(AMPLITUDE * math.sin(2 * math.pi * FREQ * (start_frame + i) / SAMPLE_RATE))
        # 24-bit LE: 3 bytes, signed
        b = struct.pack("<i", val)[:3]
        offset = i * FRAME_SIZE
        data[offset : offset + 3] = b  # left
        data[offset + 3 : offset + 6] = b  # right
    return bytes(data)


def make_text_msg(msg_type: str, payload: dict | None = None) -> str:
    """Create a Sendspin JSON text message.
    Format: {"type": "<msg_type>", "payload": {<payload>}}
    """
    obj = {"type": msg_type}
    if payload is not None:
        obj["payload"] = payload
    else:
        obj["payload"] = {}
    return json.dumps(obj)


def make_audio_binary(timestamp_us: int, pcm_data: bytes) -> bytes:
    """Create a Sendspin binary audio message (type 4)."""
    return struct.pack(">Bq", 4, timestamp_us) + pcm_data


async def send_audio(ws, duration_s: float, label: str) -> int:
    """Send audio for duration_s seconds. Returns frame count."""
    frames_total = int(SAMPLE_RATE * duration_s)
    frames_sent = 0
    ts = int(time.time() * 1_000_000)

    print(f"[server] sending {duration_s}s of audio ({label})", flush=True)
    while frames_sent < frames_total:
        chunk = generate_sine_chunk(frames_sent, CHUNK_FRAMES)
        msg = make_audio_binary(ts, chunk)
        await ws.send(msg)
        frames_sent += CHUNK_FRAMES
        ts += int(CHUNK_FRAMES / SAMPLE_RATE * 1_000_000)
        # Pace at ~real time to avoid overwhelming the bridge
        await asyncio.sleep(CHUNK_FRAMES / SAMPLE_RATE * 0.8)

    print(f"[server] sent {frames_sent} frames ({label})", flush=True)
    return frames_sent


async def handle_client(ws):
    """Run the resilience test sequence for one client."""
    print("[server] client connected, waiting for hello...", flush=True)

    # Wait for client/hello
    raw = await ws.recv()
    hello = json.loads(raw)
    print(f"[server] got hello: {hello.get('type', 'unknown')}", flush=True)

    # Send server/hello
    await ws.send(make_text_msg("server/hello", {
        "server_id": "resilience-test",
        "name": "Resilience Test Server",
        "version": 1,
        "active_roles": ["player@v1"],
        "connection_reason": "playback",
    }))

    await asyncio.sleep(0.5)

    # ──── Scenario 1: Normal stream → end → restart ────
    print("\n[server] === SCENARIO 1: stream start/stop/restart ===", flush=True)

    # Start stream
    await ws.send(make_text_msg("stream/start", {
        "player": {
            "codec": "pcm",
            "sample_rate": SAMPLE_RATE,
            "channels": CHANNELS,
            "bit_depth": BIT_DEPTH,
        }
    }))
    print("[server] sent stream/start", flush=True)
    await asyncio.sleep(0.2)

    await send_audio(ws, 5.0, "scenario 1a")

    # End stream
    await ws.send(make_text_msg("stream/end", {}))
    print("[server] sent stream/end", flush=True)

    # Pause (bridge should go idle)
    print("[server] pausing 3s (bridge should be idle)...", flush=True)
    await asyncio.sleep(3.0)

    # Restart stream
    await ws.send(make_text_msg("stream/start", {
        "player": {
            "codec": "pcm",
            "sample_rate": SAMPLE_RATE,
            "channels": CHANNELS,
            "bit_depth": BIT_DEPTH,
        }
    }))
    print("[server] sent stream/start (restart)", flush=True)
    await asyncio.sleep(0.2)

    await send_audio(ws, 5.0, "scenario 1b")

    # ──── Scenario 2: Mid-stream clear (seek) ────
    print("\n[server] === SCENARIO 2: mid-stream clear (seek) ===", flush=True)

    # Send 2s of audio
    await send_audio(ws, 2.0, "scenario 2 pre-seek")

    # Clear (simulates seek)
    await ws.send(make_text_msg("stream/clear", {}))
    print("[server] sent stream/clear (seek)", flush=True)
    await asyncio.sleep(0.5)

    # Continue with fresh audio
    await send_audio(ws, 3.0, "scenario 2 post-seek")

    # End stream
    await ws.send(make_text_msg("stream/end", {}))
    print("[server] sent stream/end", flush=True)

    # ──── Scenario 3: Server disconnect ────
    print("\n[server] === SCENARIO 3: server disconnect ===", flush=True)
    print("[server] closing connection (bridge should reconnect)...", flush=True)
    await ws.close()
    print("[server] connection closed", flush=True)


async def main():
    print(f"[server] starting resilience test server on port {PORT}", flush=True)

    # Run for multiple connection cycles to test reconnect
    cycle = 0
    async with websockets.serve(handle_client, "0.0.0.0", PORT):
        print(f"[server] listening on ws://0.0.0.0:{PORT}/sendspin", flush=True)
        # Keep running for reconnect tests
        await asyncio.sleep(60)
        print("[server] server timeout, shutting down", flush=True)


if __name__ == "__main__":
    asyncio.run(main())

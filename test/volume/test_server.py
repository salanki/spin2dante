#!/usr/bin/env python3
"""
Sendspin test server for volume control verification.

Streams a constant-amplitude sine tone in a single long session.
After DURATION_BEFORE seconds, sends a server/command to set volume
to TARGET_VOLUME. Continues streaming for DURATION_AFTER seconds so
the validator can measure the amplitude drop.

Does NOT close the connection — stays alive for the full test duration.
"""

import asyncio
import json
import math
import struct
import time

import websockets

SAMPLE_RATE = 48000
CHANNELS = 2
BIT_DEPTH = 24
BYTES_PER_SAMPLE = 3
FRAME_SIZE = BYTES_PER_SAMPLE * CHANNELS
CHUNK_FRAMES = 480  # 10ms
PORT = 8927
FREQ = 1000
AMPLITUDE = 0x3FFFFF  # ~half of 24-bit max

DURATION_BEFORE = 15.0   # seconds at 100% before volume change
TARGET_VOLUME = 50       # volume command value (0-100)
DURATION_AFTER = 25.0    # seconds after volume change


def generate_sine_chunk(start_frame, frames):
    data = bytearray(frames * FRAME_SIZE)
    for i in range(frames):
        val = int(AMPLITUDE * math.sin(2 * math.pi * FREQ * (start_frame + i) / SAMPLE_RATE))
        b = struct.pack("<i", val)[:3]
        offset = i * FRAME_SIZE
        data[offset:offset + 3] = b
        data[offset + 3:offset + 6] = b
    return bytes(data)


def make_text_msg(msg_type, payload=None):
    obj = {"type": msg_type, "payload": payload or {}}
    return json.dumps(obj)


def make_audio_binary(timestamp_us, pcm_data):
    return struct.pack(">Bq", 4, timestamp_us) + pcm_data


async def send_audio(ws, duration_s, start_frame, ts):
    frames_total = int(SAMPLE_RATE * duration_s)
    frames_sent = 0
    while frames_sent < frames_total:
        chunk = generate_sine_chunk(start_frame + frames_sent, CHUNK_FRAMES)
        await ws.send(make_audio_binary(ts, chunk))
        frames_sent += CHUNK_FRAMES
        ts += int(CHUNK_FRAMES / SAMPLE_RATE * 1_000_000)
        await asyncio.sleep(CHUNK_FRAMES / SAMPLE_RATE * 0.8)
    return frames_sent, ts


async def handle_client(ws):
    print("[server] client connected", flush=True)
    raw = await ws.recv()
    hello = json.loads(raw)
    print(f"[server] got hello: {hello.get('type')}", flush=True)

    await ws.send(make_text_msg("server/hello", {
        "server_id": "volume-test",
        "name": "Volume Test Server",
        "version": 1,
        "active_roles": ["player@v1"],
        "connection_reason": "playback",
    }))
    await asyncio.sleep(0.5)

    await ws.send(make_text_msg("stream/start", {
        "player": {
            "codec": "pcm",
            "sample_rate": SAMPLE_RATE,
            "channels": CHANNELS,
            "bit_depth": BIT_DEPTH,
        }
    }))
    print("[server] stream started", flush=True)
    await asyncio.sleep(0.2)

    ts = int(time.time() * 1_000_000)

    # Phase 1: full volume
    print(f"[server] sending {DURATION_BEFORE}s at 100% volume", flush=True)
    frames_sent, ts = await send_audio(ws, DURATION_BEFORE, 0, ts)

    # Send volume command
    await ws.send(make_text_msg("server/command", {
        "player": {
            "command": "volume",
            "volume": TARGET_VOLUME,
        }
    }))
    print(f"[server] sent volume={TARGET_VOLUME} command", flush=True)

    # Phase 2: reduced volume (same source amplitude — bridge applies gain)
    print(f"[server] sending {DURATION_AFTER}s at volume={TARGET_VOLUME}", flush=True)
    frames_sent2, ts = await send_audio(ws, DURATION_AFTER, frames_sent, ts)

    # Write timing metadata for validator
    with open("/shared/volume_test_meta.txt", "w") as f:
        f.write(f"volume_change_at_frame={frames_sent}\n")
        f.write(f"target_volume={TARGET_VOLUME}\n")
        f.write(f"total_frames={frames_sent + frames_sent2}\n")

    print("[server] audio complete, keeping connection alive", flush=True)

    # Keep connection alive until test completes
    try:
        await asyncio.sleep(120)
    except asyncio.CancelledError:
        pass


async def main():
    print(f"[server] starting volume test server on port {PORT}", flush=True)
    async with websockets.serve(handle_client, "0.0.0.0", PORT):
        await asyncio.sleep(180)

if __name__ == "__main__":
    asyncio.run(main())
